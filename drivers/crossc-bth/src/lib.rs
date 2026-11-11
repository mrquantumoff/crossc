//! crossc-bth: thin KMDF function driver for a remote device's A2DP-sink
//! BTHENUM node. It replaces BthA2dp.sys on that one node and exposes raw
//! L2CAP channels (as initiator) to user mode through the IOCTLs defined in
//! `crossc-ioctl`. All protocol logic (AVDTP, RTP, codecs) lives in user mode.
//!
//! The same device is an ACX render endpoint (`acx.rs`) whose PCM is handed
//! to user mode through the audio IOCTLs (`audio.rs`; the logic is in
//! `audio_core.rs` / `audio_park.rs` / `pcm_ring.rs`, which are unit-tested
//! on the host).
//!
//! # Access
//! ACX needs the device object to be openable by the audio stack, so the
//! device DACL also admits interactive users (see the INX). Every crossc
//! IOCTL except GET_INFO is therefore gated in `evt_io_device_control` on
//! the *requesting process* being elevated ([`requestor_is_admin`]).
//!
//! Modelled on the WDK `bluetooth/bthecho` client sample (bthcli): profile
//! interface query in DeviceAdd, remote address via
//! IOCTL_INTERNAL_BTHENUM_GET_DEVINFO in SelfManagedIoInit, BRBs carried in the
//! request context and submitted by formatting the *incoming* request with
//! IOCTL_INTERNAL_BTH_SUBMIT_BRB, async CLOSE_CHANNEL from the DISPATCH-level
//! disconnect indication, EvtIoStop cancelling sent requests.
//!
//! # Concurrency model
//! * One `WDFSPINLOCK` per device guards every [`Channel`]. It is never held
//!   across a call that can complete a request synchronously (WdfRequestSend,
//!   WdfRequestCancelSentRequest, WdfRequestComplete).
//! * Every request forwarded to bthport is linked into its channel's
//!   `pending` list (intrusive, via the request context) while in flight, so
//!   close/disconnect can cancel it (reference taken under the lock, cancel
//!   issued after releasing it - "Synchronizing Cancellation of Sent Requests").
//! * Channel close is always the async CLOSE_CHANNEL BRB on a per-channel
//!   preallocated request (usable at DISPATCH); synchronous callers (CLOSE
//!   IOCTL, file cleanup, D0Exit) wait on the channel's `done` event at PASSIVE.
//! * Cancel decisions are made under the lock on both sides: a canceller
//!   either cancels a request that is already sent (`sent`), or leaves a
//!   sticky note (`cancel_wanted`) that the submitter checks, under the lock,
//!   right after WdfRequestSend returns. No cancel can fall into the gap
//!   between linking a request and sending it.
//! * A slot returns to `Free` only after the close BRB completed AND no
//!   forwarded request still references it.
//! * Synchronous closes (CLOSE ioctl, file cleanup, D0Exit, flush) wait with
//!   an upper bound ([`CLOSE_TIMEOUT_PASSES`]). On timeout the slot is
//!   abandoned: it stays `Closing` (so the close request is never reused in
//!   flight), loses its owner, and frees itself when bthport finally drains.
//! * A handle that goes away without CLOSE_CHANNEL (process crash/kill) has
//!   its channels closed by `evt_file_cleanup` (IRP_MJ_CLEANUP). With ACX on,
//!   WDF does not call that callback (see `evt_file_object_cleanup`), so the
//!   same close also hangs on the WDFFILEOBJECT's context cleanup, which runs
//!   when the file object is destroyed (IRP_MJ_CLOSE).
#![no_std]
#![allow(clippy::missing_safety_doc)]
// The WDF binding macro expands to a fn taking every WDF parameter.
#![allow(clippy::too_many_arguments)]

#[cfg(not(test))]
extern crate wdk_panic;
#[cfg(not(test))]
use wdk_alloc::WdkAllocator;
#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: WdkAllocator = WdkAllocator;

mod acx;
mod audio;
mod audio_core;
mod audio_park;
mod bt_name;
mod bth;
mod pcm_ring;
mod trace;

use core::mem::{offset_of, size_of, zeroed};
use core::ptr::{addr_of_mut, null_mut};
use core::sync::atomic::{AtomicBool, Ordering};

use bth::*;
use crossc_ioctl as ioc;
use wdk_sys::ntddk::{
    IoGetRequestorProcess, KeClearEvent, KeGetCurrentIrql, KeInitializeEvent, KeSetEvent, KeWaitForSingleObject,
    PsDereferencePrimaryToken, PsReferencePrimaryToken, SeTokenIsAdmin,
};
use wdk_sys::*;

/// Driver version reported by GET_INFO: `major << 24 | minor << 16 | patch`
/// (keep in sync with Cargo.toml `version`).
const DRIVER_VERSION: u32 = 1 << 16; // 0.1.0
const INCOMING_QUEUE_DEPTH: u8 = 50;
/// Max forwarded requests (open/read/write) in flight per channel.
const MAX_PENDING_PER_CHANNEL: u32 = 32;
/// Length of one wait pass in a synchronous close (100 ms, relative).
const CLOSE_PASS_100NS: i64 = -1_000_000;
/// Upper bound for one synchronous close operation, in passes (8 s). Shared
/// across all channels closed by one D0Exit / flush / cleanup, and below the
/// CLI's 10 s IOCTL timeout.
const CLOSE_TIMEOUT_PASSES: u32 = 80;
/// Re-issue cancels for still-pending requests every this many passes (1 s).
const RECANCEL_EVERY_PASSES: u32 = 10;

// ---------------------------------------------------------------------------
// WDF context plumbing
// ---------------------------------------------------------------------------

#[repr(transparent)]
struct CtxInfo(WDF_OBJECT_CONTEXT_TYPE_INFO);
// SAFETY: immutable, contains only pointers to other statics.
unsafe impl Sync for CtxInfo {}

macro_rules! context_type {
    ($info:ident, $ty:ty, $name:literal, $getter:ident) => {
        static $info: CtxInfo = CtxInfo(WDF_OBJECT_CONTEXT_TYPE_INFO {
            Size: size_of::<WDF_OBJECT_CONTEXT_TYPE_INFO>() as ULONG,
            ContextName: $name.as_ptr().cast(),
            ContextSize: size_of::<$ty>(),
            UniqueType: &raw const $info.0,
            EvtDriverGetUniqueContextType: None,
        });
        /// SAFETY: `h` must be a live WDF object created with this context type.
        unsafe fn $getter(h: WDFOBJECT) -> *mut $ty {
            unsafe { call_unsafe_wdf_function_binding!(WdfObjectGetTypedContextWorker, h, &raw const $info.0) }
                .cast::<$ty>()
        }
    };
}

fn attributes() -> WDF_OBJECT_ATTRIBUTES {
    WDF_OBJECT_ATTRIBUTES {
        Size: size_of::<WDF_OBJECT_ATTRIBUTES>() as ULONG,
        ExecutionLevel: _WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent,
        SynchronizationScope: _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent,
        ..Default::default()
    }
}

fn nt_success(s: NTSTATUS) -> bool {
    s >= 0
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
enum ChanState {
    Free = 0,
    /// OPEN_CHANNEL BRB in flight.
    Opening,
    Open,
    /// CLOSE_CHANNEL BRB in flight and/or forwarded requests draining.
    Closing,
    /// Remote disconnected and our close finished; the id stays reserved so
    /// I/O on it reports STATUS_CONNECTION_DISCONNECTED until the owner closes
    /// it (CLOSE_CHANNEL) or its handle goes away.
    Zombie,
}

#[repr(C)]
struct Channel {
    state: ChanState,
    index: u32,
    generation: u32,
    owner: WDFFILEOBJECT,
    handle: L2CAP_CHANNEL_HANDLE,
    psm: u16,
    /// Largest SDU we may send: the MTU the peer asked for in its
    /// configuration request (BRB `InResults.Params.Mtu`).
    out_mtu: u16,
    /// Largest SDU we receive: the MTU of our own configuration request as
    /// the peer accepted it (BRB `OutResults.Params.Mtu`).
    in_mtu: u16,
    /// Close requested while Opening (handle closed / D0Exit / CLOSE ioctl).
    close_requested: bool,
    /// Peer disconnected (IndicationRemoteDisconnect). After the close, go Zombie.
    remote_disconnected: bool,
    close_brb_done: bool,
    /// When the close finishes go straight to Free, never Zombie: nobody can
    /// release the id later (no OPEN reply was delivered, the owner is
    /// closing it, or a synchronous close gave up and abandoned the slot).
    release_when_done: bool,
    pending: LIST_ENTRY,
    pending_count: u32,
    /// Signalled whenever state is Free or Zombie.
    done: KEVENT,
    close_req: WDFREQUEST,
    close_mem: WDFMEMORY,
    close_brb: _BRB_L2CA_CLOSE_CHANNEL,
    dev: *mut DeviceContext,
}

#[repr(C)]
struct DeviceContext {
    device: WDFDEVICE,
    target: WDFIOTARGET,
    lock: WDFSPINLOCK,
    profile: BTH_PROFILE_DRIVER_INTERFACE,
    profile_acquired: bool,
    in_d0: AtomicBool,
    remote_addr: u64,
    addr_flags: u32,
    next_generation: u32,
    channels: [Channel; ioc::MAX_CHANNELS],
    /// Audio path (session, PCM tap, queues of pended audio requests).
    audio: audio::AudioDev,
}
context_type!(DEVICE_CTX_INFO, DeviceContext, c"CrossCDeviceContext", device_ctx);

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum ReqKind {
    Other = 0,
    Open,
    Read,
    Write,
}

/// Context of every request the framework creates for this device
/// (WdfDeviceInitSetRequestAttributes). Nonpaged; holds the BRB so no
/// per-request allocation is needed (same as bthecho).
#[repr(C)]
struct RequestContext {
    brb: _BRB,
    link: LIST_ENTRY,
    request: WDFREQUEST,
    chan: *mut Channel,
    out_buf: *mut u8,
    out_len: usize,
    kind: ReqKind,
    /// Linked into its channel's `pending` list (lock). Cleared, under the
    /// lock, by the completion routine before it completes the request, so
    /// `on_list` == "still in flight, handle valid while the lock is held".
    on_list: bool,
    /// WdfRequestSend returned TRUE and the submitter has finished its
    /// post-send check (lock). Only then may a canceller call
    /// WdfRequestCancelSentRequest directly.
    sent: bool,
    /// A canceller found the request linked but not yet `sent`; the submitter
    /// cancels it right after sending (lock). Sticky; relies on WDF zeroing
    /// the request context for every new request.
    cancel_wanted: bool,
    /// Cancelled because the peer disconnected -> report CONNECTION_DISCONNECTED.
    disconnected: bool,
    /// Ticket of a parked WAIT_EVENT / READ_PCM (`audio_core::Ticket`), 0 for
    /// every other request. Written by `audio::park` before the request is
    /// forwarded to its queue, read only by whoever owns the request then
    /// (after a retrieve, or in EvtIoCanceledOnQueue).
    audio_ticket: u64,
}
context_type!(REQUEST_CTX_INFO, RequestContext, c"CrossCRequestContext", request_ctx);

/// Context type attached to every WDFFILEOBJECT when ACX is on. It carries no
/// data: it exists so the file-object attributes have a context type of their
/// own, which is what makes WDF keep their `EvtCleanupCallback`
/// ([`evt_file_object_cleanup`]). With a class extension on the device, WDF
/// creates the file object with the extension's attributes and attaches the
/// client's as an extra context (`FxPkgGeneral::OnCreate`); an extra context
/// whose type pointer equals an existing one (NULL == NULL included) is
/// dropped as a duplicate (`FxObject::AddContext`), callbacks and all.
#[repr(C)]
struct FileContext {
    _unused: u32,
}
static FILE_CTX_INFO: CtxInfo = CtxInfo(WDF_OBJECT_CONTEXT_TYPE_INFO {
    Size: size_of::<WDF_OBJECT_CONTEXT_TYPE_INFO>() as ULONG,
    ContextName: c"CrossCFileContext".as_ptr().cast(),
    ContextSize: size_of::<FileContext>(),
    UniqueType: &raw const FILE_CTX_INFO.0,
    EvtDriverGetUniqueContextType: None,
});

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

unsafe fn lock(dev: *mut DeviceContext) {
    unsafe { call_unsafe_wdf_function_binding!(WdfSpinLockAcquire, (*dev).lock) }
}
unsafe fn unlock(dev: *mut DeviceContext) {
    unsafe { call_unsafe_wdf_function_binding!(WdfSpinLockRelease, (*dev).lock) }
}

unsafe fn list_init(head: *mut LIST_ENTRY) {
    unsafe {
        (*head).Flink = head;
        (*head).Blink = head;
    }
}
unsafe fn list_is_empty(head: *const LIST_ENTRY) -> bool {
    unsafe { core::ptr::eq((*head).Flink, head) }
}
unsafe fn list_insert_tail(head: *mut LIST_ENTRY, e: *mut LIST_ENTRY) {
    unsafe {
        let last = (*head).Blink;
        (*e).Flink = head;
        (*e).Blink = last;
        (*last).Flink = e;
        (*head).Blink = e;
    }
}
unsafe fn list_remove(e: *mut LIST_ENTRY) {
    unsafe {
        let f = (*e).Flink;
        let b = (*e).Blink;
        (*b).Flink = f;
        (*f).Blink = b;
        (*e).Flink = e;
        (*e).Blink = e;
    }
}
unsafe fn ctx_from_link(e: *mut LIST_ENTRY) -> *mut RequestContext {
    (e as usize - offset_of!(RequestContext, link)) as *mut RequestContext
}

unsafe fn complete(req: WDFREQUEST, status: NTSTATUS, info: usize) {
    unsafe { call_unsafe_wdf_function_binding!(WdfRequestCompleteWithInformation, req, status, info as ULONG_PTR) }
}

/// Raw-pointer reads on purpose: other CPUs may be touching the channel's
/// KEVENT (KeSetEvent / KeWaitForSingleObject) concurrently, so no shared
/// `&Channel` is ever formed.
unsafe fn chan_id(c: *const Channel) -> u32 {
    unsafe { ioc::channel_id::make((*c).index as usize, (*c).generation) }
}

/// Looks up the channel for `id` owned by `file`. Caller holds the lock.
unsafe fn find_channel(dev: *mut DeviceContext, id: u32, file: WDFFILEOBJECT) -> Option<*mut Channel> {
    let slot = ioc::channel_id::slot(id);
    if slot >= ioc::MAX_CHANNELS {
        return None;
    }
    let c = unsafe { addr_of_mut!((*dev).channels[slot]) };
    unsafe {
        if (*c).state == ChanState::Free || chan_id(c) != id || (*c).owner != file {
            return None;
        }
    }
    Some(c)
}

// ---------------------------------------------------------------------------
// DriverEntry / DeviceAdd
// ---------------------------------------------------------------------------

/// DriverEntry.
///
/// # Safety
/// Called by the kernel.
#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(driver: PDRIVER_OBJECT, registry_path: PCUNICODE_STRING) -> NTSTATUS {
    trace!("DriverEntry v{}.{}.{}", DRIVER_VERSION >> 24, (DRIVER_VERSION >> 16) & 0xFF, DRIVER_VERSION & 0xFFFF);
    let mut cfg = WDF_DRIVER_CONFIG {
        Size: size_of::<WDF_DRIVER_CONFIG>() as ULONG,
        EvtDriverDeviceAdd: Some(evt_device_add),
        ..Default::default()
    };
    let mut wdf_driver: WDFDRIVER = null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut cfg,
            &raw mut wdf_driver
        )
    };
    if !nt_success(status) {
        trace!("WdfDriverCreate failed {:#x}", status);
        return status;
    }
    // ACX class-extension init (render endpoint). Never fails the driver.
    unsafe { acx::driver_init(wdf_driver) };
    status
}

unsafe extern "C" fn evt_device_add(_driver: WDFDRIVER, mut init: PWDFDEVICE_INIT) -> NTSTATUS {
    trace!("EvtDriverDeviceAdd");
    unsafe {
        // ACX must see the WDFDEVICE_INIT before anything else. `false` =
        // ACX is off (DisableAcx kill switch or ACX unavailable): from here
        // on this is exactly the pre-ACX driver.
        let acx_on = match acx::device_init(init) {
            Ok(on) => on,
            Err(status) => return status,
        };

        let mut pnp = WDF_PNPPOWER_EVENT_CALLBACKS {
            Size: size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>() as ULONG,
            EvtDeviceSelfManagedIoInit: Some(evt_self_managed_io_init),
            EvtDeviceSelfManagedIoFlush: Some(evt_self_managed_io_flush),
            EvtDeviceD0Entry: Some(evt_d0_entry),
            EvtDeviceD0Exit: Some(evt_d0_exit),
            ..Default::default()
        };
        if acx_on {
            // Circuits are added/removed in PrepareHardware/ReleaseHardware.
            pnp.EvtDevicePrepareHardware = Some(acx::evt_prepare_hardware);
            pnp.EvtDeviceReleaseHardware = Some(acx::evt_release_hardware);
        }
        call_unsafe_wdf_function_binding!(WdfDeviceInitSetPnpPowerEventCallbacks, init, &raw mut pnp);

        let mut fcfg = WDF_FILEOBJECT_CONFIG {
            Size: size_of::<WDF_FILEOBJECT_CONFIG>() as ULONG,
            EvtDeviceFileCreate: None,
            EvtFileClose: None,
            EvtFileCleanup: Some(evt_file_cleanup),
            AutoForwardCleanupClose: _WDF_TRI_STATE::WdfUseDefault,
            FileObjectClass: _WDF_FILEOBJECT_CLASS::WdfFileObjectWdfCannotUseFsContexts,
        };
        // With ACX on, WDF never calls EvtFileCleanup above for our handles
        // (see evt_file_object_cleanup), so the file object also gets a
        // context cleanup callback. With ACX off the attributes stay
        // WDF_NO_OBJECT_ATTRIBUTES: exactly the pre-ACX driver.
        // Constraints: FxPkgGeneral::ConfigureConstraints must end up where
        // WDF_NO_OBJECT_ATTRIBUTES put it (passive, no synchronization).
        // Scope None is pinned so that does not depend on what the device
        // inherits; the execution level is passive for KMDF >= 1.11 whatever
        // is inherited.
        let mut fattr = attributes();
        fattr.SynchronizationScope = _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeNone;
        fattr.ContextTypeInfo = &raw const FILE_CTX_INFO.0;
        fattr.EvtCleanupCallback = Some(evt_file_object_cleanup);
        let fattr_ptr: PWDF_OBJECT_ATTRIBUTES = if acx_on { &raw mut fattr } else { WDF_NO_OBJECT_ATTRIBUTES };
        call_unsafe_wdf_function_binding!(WdfDeviceInitSetFileObjectConfig, init, &raw mut fcfg, fattr_ptr);

        let mut rattr = attributes();
        rattr.ContextTypeInfo = &raw const REQUEST_CTX_INFO.0;
        call_unsafe_wdf_function_binding!(WdfDeviceInitSetRequestAttributes, init, &raw mut rattr);

        let mut dattr = attributes();
        dattr.ContextTypeInfo = &raw const DEVICE_CTX_INFO.0;
        dattr.EvtCleanupCallback = Some(evt_device_cleanup);
        let mut device: WDFDEVICE = null_mut();
        let status = call_unsafe_wdf_function_binding!(WdfDeviceCreate, &raw mut init, &raw mut dattr, &raw mut device);
        if !nt_success(status) {
            trace!("WdfDeviceCreate failed {:#x}", status);
            return status;
        }

        if acx_on {
            let status = acx::device_created(device);
            if !nt_success(status) {
                return status;
            }
        }

        let dev = device_ctx(device.cast());
        (*dev).device = device;
        (*dev).target = call_unsafe_wdf_function_binding!(WdfDeviceGetIoTarget, device);
        (*dev).next_generation = 1;

        let mut lattr = attributes();
        lattr.ParentObject = device.cast();
        let status = call_unsafe_wdf_function_binding!(WdfSpinLockCreate, &raw mut lattr, addr_of_mut!((*dev).lock));
        if !nt_success(status) {
            return status;
        }
        // Audio session state; with ACX on also the queues for the pended
        // audio IOCTLs.
        let status = audio::init(dev, device, acx_on);
        if !nt_success(status) {
            return status;
        }

        for i in 0..ioc::MAX_CHANNELS {
            let c = addr_of_mut!((*dev).channels[i]);
            (*c).index = i as u32;
            (*c).dev = dev;
            (*c).state = ChanState::Free;
            list_init(addr_of_mut!((*c).pending));
            KeInitializeEvent(addr_of_mut!((*c).done), _EVENT_TYPE::NotificationEvent, 1);
            let mut a = attributes();
            a.ParentObject = device.cast();
            let status = call_unsafe_wdf_function_binding!(WdfRequestCreate, &raw mut a, (*dev).target, addr_of_mut!((*c).close_req));
            if !nt_success(status) {
                return status;
            }
            let mut a = attributes();
            a.ParentObject = device.cast();
            let status = call_unsafe_wdf_function_binding!(
                WdfMemoryCreatePreallocated,
                &raw mut a,
                addr_of_mut!((*c).close_brb).cast(),
                size_of::<_BRB_L2CA_CLOSE_CHANNEL>(),
                addr_of_mut!((*c).close_mem)
            );
            if !nt_success(status) {
                return status;
            }
        }

        // Profile driver interface (BRB alloc/init). Same place as bthecho.
        let status = call_unsafe_wdf_function_binding!(
            WdfFdoQueryForInterface,
            device,
            &GUID_BTHDDI_PROFILE_DRIVER_INTERFACE,
            addr_of_mut!((*dev).profile).cast::<INTERFACE>(),
            size_of::<BTH_PROFILE_DRIVER_INTERFACE>() as USHORT,
            BTHDDI_PROFILE_DRIVER_INTERFACE_VERSION_FOR_QI as USHORT,
            null_mut()
        );
        if !nt_success(status) {
            trace!("QueryForInterface(GUID_BTHDDI_PROFILE_DRIVER_INTERFACE) failed {:#x}", status);
            return status;
        }
        (*dev).profile_acquired = true;
        if (*dev).profile.BthInitializeBrb.is_none() {
            trace!("profile interface has no BthInitializeBrb");
            return STATUS_NOT_SUPPORTED;
        }

        let status = call_unsafe_wdf_function_binding!(
            WdfDeviceCreateDeviceInterface,
            device,
            &raw const DEVICE_INTERFACE_GUID,
            null_mut()
        );
        if !nt_success(status) {
            trace!("WdfDeviceCreateDeviceInterface failed {:#x}", status);
            return status;
        }

        // Default queue: parallel, power-managed (like bthcli), PASSIVE
        // callbacks so CLOSE can wait for the close BRB. EvtIoStop cancels
        // requests we forwarded so power-down / removal never hangs on a
        // READ that is waiting for data.
        let mut q = WDF_IO_QUEUE_CONFIG {
            Size: size_of::<WDF_IO_QUEUE_CONFIG>() as ULONG,
            DispatchType: _WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchParallel,
            PowerManaged: _WDF_TRI_STATE::WdfUseDefault,
            DefaultQueue: 1,
            EvtIoDeviceControl: Some(evt_io_device_control),
            EvtIoStop: Some(evt_io_stop),
            ..Default::default()
        };
        q.Settings.Parallel.NumberOfPresentedRequests = u32::MAX; // unlimited
        let mut qattr = attributes();
        qattr.ExecutionLevel = _WDF_EXECUTION_LEVEL::WdfExecutionLevelPassive;
        let status = call_unsafe_wdf_function_binding!(WdfIoQueueCreate, device, &raw mut q, &raw mut qattr, null_mut());
        if !nt_success(status) {
            trace!("WdfIoQueueCreate failed {:#x}", status);
            return status;
        }
        trace!("device added");
        STATUS_SUCCESS
    }
}

static DEVICE_INTERFACE_GUID: GUID = GUID {
    Data1: ioc::DEVICE_INTERFACE_GUID.data1,
    Data2: ioc::DEVICE_INTERFACE_GUID.data2,
    Data3: ioc::DEVICE_INTERFACE_GUID.data3,
    Data4: ioc::DEVICE_INTERFACE_GUID.data4,
};

unsafe extern "C" fn evt_device_cleanup(obj: WDFOBJECT) {
    unsafe {
        let dev = device_ctx(obj);
        if (*dev).profile_acquired {
            if let Some(deref) = (*dev).profile.Interface.InterfaceDereference {
                deref((*dev).profile.Interface.Context);
            }
            (*dev).profile_acquired = false;
        }
    }
    trace!("device cleanup");
}

// ---------------------------------------------------------------------------
// PnP / power
// ---------------------------------------------------------------------------

unsafe extern "C" fn evt_self_managed_io_init(device: WDFDEVICE) -> NTSTATUS {
    unsafe {
        let dev = device_ctx(device.cast());
        // Remote BD_ADDR, exactly as bthcli does it: internal IOCTL with an
        // *output* memory descriptor (not the "Others" argument form).
        let mut info: BTH_DEVICE_INFO = zeroed();
        let mut out = WDF_MEMORY_DESCRIPTOR {
            Type: _WDF_MEMORY_DESCRIPTOR_TYPE::WdfMemoryDescriptorTypeBuffer,
            ..zeroed()
        };
        out.u.BufferType.Buffer = (&raw mut info).cast();
        out.u.BufferType.Length = size_of::<BTH_DEVICE_INFO>() as ULONG;
        let status = call_unsafe_wdf_function_binding!(
            WdfIoTargetSendInternalIoctlSynchronously,
            (*dev).target,
            null_mut(),
            IOCTL_INTERNAL_BTHENUM_GET_DEVINFO,
            null_mut(),
            &raw mut out,
            null_mut(),
            null_mut()
        );
        if nt_success(status) && info.address != 0 {
            (*dev).remote_addr = info.address;
            (*dev).addr_flags = ioc::INFO_FLAG_ADDR_VALID | ioc::INFO_FLAG_ADDR_FROM_DEVINFO;
        } else {
            trace!("GET_DEVINFO failed {:#x}; falling back to instance id", status);
            if let Some(a) = addr_from_instance_id(device) {
                (*dev).remote_addr = a;
                (*dev).addr_flags = ioc::INFO_FLAG_ADDR_VALID;
            }
        }
        let mut s = [0u8; 17];
        ioc::format_bd_addr((*dev).remote_addr, &mut s);
        trace!(
            "remote {} (flags {:#x}) devinfo flags={:#x} cod={:#08x}",
            core::str::from_utf8(&s).unwrap_or("?"),
            (*dev).addr_flags,
            info.flags,
            info.classOfDevice
        );
        if audio::enabled(dev) {
            // BTH_DEVICE_INFO.name is CHAR[248], UTF-8.
            let name = core::slice::from_raw_parts(info.name.as_ptr().cast::<u8>(), info.name.len());
            assign_friendly_name(device, if nt_success(status) { name } else { &[] });
        }
    }
    // Never fail start over this: GET_INFO must stay reachable for diagnosis.
    STATUS_SUCCESS
}

/// Name used when the Bluetooth name of the buds is not known and the device
/// has no friendly name yet. Same text as `CrossC.Headphones.szPname` in the
/// INX. NUL-terminated.
static FALLBACK_NAME: [u16; 12] = {
    let mut out = [0u16; 12];
    let b = b"Galaxy Buds";
    let mut i = 0;
    while i < b.len() {
        out[i] = b[i] as u16;
        i += 1;
    }
    out
};

/// Give the device node the Bluetooth name of the buds as its friendly name
/// (`DEVPKEY_Device_FriendlyName`), so that the playback device reads
/// "Headphones (<name>)" as it does with the inbox A2DP driver.
///
/// Where the "(...)" comes from, as far as the registry of the test machine
/// shows it (`MMDevices\Audio\Render\*\Properties`): the endpoint property
/// `PKEY_DeviceInterface_FriendlyName` follows the name of the *device node*
/// (friendly name, else device description), not the `FriendlyName` value
/// of the KS interface. A Realtek HD Audio node whose interfaces are named
/// "HD Audio Headphone" gives "(High Definition Audio Device)", its device
/// description; the inbox hands-free audio node gives its device node's
/// friendly name. For the inbox A2DP driver the endpoint read
/// "(<Bluetooth name>)" and its KS interface had no `FriendlyName`, which
/// fits, but the friendly name that node had then could not be read back.
/// "Headphones" is the category name of the bridge pin. Not confirmed with
/// this driver; docs/SETUP.md section 12 asks for the name Windows shows.
///
/// The property is persistent, so the name is already there at the next
/// start. If the Bluetooth name is unknown the previous name is kept; only a
/// device that has no friendly name at all gets [`FALLBACK_NAME`]. Failures
/// are logged and ignored (the name is cosmetic). PASSIVE.
unsafe fn assign_friendly_name(device: WDFDEVICE, bt_name: &[u8]) {
    unsafe {
        // Flags: 0 to query (reserved in IoGetDevicePropertyData),
        // PLUGPLAY_PROPERTY_PERSISTENT to set (IoSetDevicePropertyData).
        let mut pd = WDF_DEVICE_PROPERTY_DATA {
            Size: size_of::<WDF_DEVICE_PROPERTY_DATA>() as ULONG,
            PropertyKey: &DEVPKEY_DEVICE_FRIENDLY_NAME,
            Lcid: 0, // LOCALE_NEUTRAL
            Flags: 0,
        };
        let mut buf: bt_name::Utf16Name = [0; bt_name::MAX_UNITS + 1];
        let units = bt_name::to_utf16(bt_name, &mut buf);
        let (ptr, units, what) = if units > 0 {
            (buf.as_ptr(), units, "Bluetooth name")
        } else {
            // Keep a name set by an earlier start: the fallback is written
            // only when the property is known to be absent.
            let mut probe = [0u16; 2];
            let mut len: ULONG = 0;
            let mut ty: DEVPROPTYPE = 0;
            let st = call_unsafe_wdf_function_binding!(
                WdfDeviceQueryPropertyEx,
                device,
                &raw mut pd,
                size_of_val(&probe) as ULONG,
                probe.as_mut_ptr().cast(),
                &raw mut len,
                &raw mut ty
            );
            if st != STATUS_OBJECT_NAME_NOT_FOUND {
                trace!("no Bluetooth name; the device's friendly name is left as it is (query {:#x})", st);
                return;
            }
            (FALLBACK_NAME.as_ptr(), FALLBACK_NAME.len(), "fallback name")
        };
        pd.Flags = PLUGPLAY_PROPERTY_PERSISTENT;
        let st = call_unsafe_wdf_function_binding!(
            WdfDeviceAssignProperty,
            device,
            &raw mut pd,
            DEVPROP_TYPE_STRING,
            (units * size_of::<u16>()) as ULONG,
            ptr.cast_mut().cast()
        );
        trace!("friendly name set from the {} ({} UTF-16 units): {:#x}", what, units - 1, st);
    }
}

unsafe fn addr_from_instance_id(device: WDFDEVICE) -> Option<u64> {
    unsafe {
        let mut buf = [0u16; 400];
        let mut pd = WDF_DEVICE_PROPERTY_DATA {
            Size: size_of::<WDF_DEVICE_PROPERTY_DATA>() as ULONG,
            PropertyKey: &DEVPKEY_DEVICE_INSTANCE_ID,
            Lcid: 0, // LOCALE_NEUTRAL
            Flags: 0,
        };
        let mut len: ULONG = 0;
        let mut ty: DEVPROPTYPE = 0;
        let status = call_unsafe_wdf_function_binding!(
            WdfDeviceQueryPropertyEx,
            device,
            &raw mut pd,
            (buf.len() * 2) as ULONG,
            buf.as_mut_ptr().cast(),
            &raw mut len,
            &raw mut ty
        );
        if !nt_success(status) {
            trace!("instance id query failed {:#x}", status);
            return None;
        }
        ioc::bd_addr_from_instance_id(&buf[..(len as usize / 2).min(buf.len())])
    }
}

unsafe extern "C" fn evt_d0_entry(device: WDFDEVICE, _prev: WDF_POWER_DEVICE_STATE) -> NTSTATUS {
    unsafe { (*device_ctx(device.cast())).in_d0.store(true, Ordering::SeqCst) };
    trace!("D0Entry");
    STATUS_SUCCESS
}

unsafe extern "C" fn evt_d0_exit(device: WDFDEVICE, target: WDF_POWER_DEVICE_STATE) -> NTSTATUS {
    trace!("D0Exit -> {}", target);
    unsafe {
        let dev = device_ctx(device.cast());
        (*dev).in_d0.store(false, Ordering::SeqCst);
        // The audio session ends with D0: its ATTACH is completed, the
        // endpoint shows unplugged.
        audio::detach_all(dev, "D0Exit");
        let _ = close_all(dev, None, CLOSE_TIMEOUT_PASSES);
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn evt_self_managed_io_flush(device: WDFDEVICE) {
    trace!("SelfManagedIoFlush");
    unsafe {
        let dev = device_ctx(device.cast());
        // Removal, surprise removal included.
        audio::detach_all(dev, "device removal");
        let _ = close_all(dev, None, CLOSE_TIMEOUT_PASSES);
    }
}

/// IRP_MJ_CLEANUP: the last handle to `file` is gone; close what it still
/// owns. Going by the WDF code, this is only called while ACX is off (see
/// [`evt_file_object_cleanup`]); not confirmed live.
unsafe extern "C" fn evt_file_cleanup(file: WDFFILEOBJECT) {
    unsafe {
        let device = call_unsafe_wdf_function_binding!(WdfFileObjectGetDevice, file);
        let dev = device_ctx(device.cast());
        audio::file_gone(dev, file);
        let n = close_all(dev, Some(file), CLOSE_TIMEOUT_PASSES);
        if n > 0 {
            trace!("file cleanup: closed {} channel(s) left open by a handle", n);
        }
    }
}

/// Context cleanup of a WDFFILEOBJECT (registered only when ACX is on): WDF
/// is destroying the file object, in IRP_MJ_CLOSE. Close what it still owns.
///
/// Why this exists: with a class extension that has a create callback (ACX),
/// `FxPkgGeneral::OnCreate` records in the file object how far down the
/// driver chain the create got, *before* calling the extension's callback,
/// and for a client without `EvtDeviceFileCreate` never moves that mark when
/// the extension passes the create on. `OnCleanup`/`OnClose` then call only
/// the extension's `EvtFileCleanup`/`EvtFileClose`, never ours (public WDF
/// source and the installed Wdf01000.sys 1.35 agree; nothing was tested
/// live). Object context cleanup does not go through that list: it runs from
/// `FxFileObject::_DestroyFileObject` for every file object.
///
/// Differences from `evt_file_cleanup`:
/// * It runs at IRP_MJ_CLOSE, i.e. after every request on the handle has
///   completed, not at IRP_MJ_CLEANUP. When a process dies the I/O manager
///   cancels its pending requests (bthport must honour the cancel, as it must
///   for WdfRequestCancelSentRequest), so the close follows. A handle closed
///   while one of its own READs is still pending in a live thread keeps its
///   channels until that READ ends.
/// * It also runs for ACX's own (KS) file objects, which own no channel, and
///   again after `evt_file_cleanup` if both ever fire: a no-op in both cases.
/// * The contract of an object cleanup callback is IRQL <= DISPATCH. Close
///   IRPs arrive at PASSIVE; if this one ever does not, the closes are only
///   started and the slots free themselves (budget 0 = abandon at once).
unsafe extern "C" fn evt_file_object_cleanup(obj: WDFOBJECT) {
    unsafe {
        let file: WDFFILEOBJECT = obj.cast();
        let device = call_unsafe_wdf_function_binding!(WdfFileObjectGetDevice, file);
        if device.is_null() {
            return;
        }
        let budget = if u32::from(KeGetCurrentIrql()) < DISPATCH_LEVEL { CLOSE_TIMEOUT_PASSES } else { 0 };
        let dev = device_ctx(device.cast());
        // Safety net only: a pending ATTACH keeps its file object alive, so
        // the session of this handle ended before this point.
        audio::file_gone(dev, file);
        let n = close_all(dev, Some(file), budget);
        if n > 0 {
            trace!("file close: closed {} channel(s) left open by a handle (waited up to {} ms)", n, budget * 100);
        }
    }
}

/// Close (synchronously, PASSIVE) every channel, or every channel of `owner`.
/// Zombie channels of the owner are released too. All closes are started
/// first, then waited for against one shared budget of `budget` passes, so
/// the worst case is one timeout, not one per channel. With `budget` 0 the
/// closes are only started (usable at DISPATCH) and the slots are abandoned
/// to free themselves. Returns the number of closes started.
unsafe fn close_all(dev: *mut DeviceContext, owner: Option<WDFFILEOBJECT>, budget: u32) -> usize {
    unsafe {
        let mut started = [(null_mut::<Channel>(), 0u32); ioc::MAX_CHANNELS];
        let mut n = 0usize;
        for i in 0..ioc::MAX_CHANNELS {
            let c = addr_of_mut!((*dev).channels[i]);
            lock(dev);
            let mine = (*c).state != ChanState::Free && owner.is_none_or(|o| (*c).owner == o);
            let generation = (*c).generation;
            unlock(dev);
            if mine && begin_close(c, false, generation, true) {
                started[n] = (c, generation);
                n += 1;
            }
        }
        let mut left = budget;
        for &(c, generation) in &started[..n] {
            left -= wait_closed(c, generation, left).min(left);
        }
        n
    }
}

// ---------------------------------------------------------------------------
// Channel lifecycle
// ---------------------------------------------------------------------------

/// Start closing `c` (any IRQL <= DISPATCH) if it still is incarnation
/// `generation` (the slot may have been freed and reused since the caller
/// looked). Idempotent. Returns false if the slot is Free or was reused.
/// With `release` the slot goes to Free (not Zombie) when the close finishes,
/// and an existing Zombie is freed right away.
unsafe fn begin_close(c: *mut Channel, remote: bool, generation: u32, release: bool) -> bool {
    unsafe {
        let dev = (*c).dev;
        let mut victims: [WDFREQUEST; MAX_PENDING_PER_CHANNEL as usize] = [null_mut(); MAX_PENDING_PER_CHANNEL as usize];
        let mut n = 0usize;
        let mut send_close = false;
        let mut signal = false;

        lock(dev);
        if (*c).state == ChanState::Free || (*c).generation != generation {
            unlock(dev);
            return false;
        }
        if remote {
            (*c).remote_disconnected = true;
        }
        if release {
            (*c).release_when_done = true;
        }
        match (*c).state {
            ChanState::Free | ChanState::Closing => {}
            ChanState::Zombie => {
                if release {
                    release_slot_locked(c);
                    signal = true;
                }
            }
            ChanState::Opening => (*c).close_requested = true,
            ChanState::Open => {
                (*c).state = ChanState::Closing;
                (*c).close_brb_done = false;
                send_close = true;
            }
        }
        if matches!((*c).state, ChanState::Opening | ChanState::Closing) {
            n = collect_cancels_locked(c, &mut victims);
        }
        unlock(dev);

        if signal {
            KeSetEvent(addr_of_mut!((*c).done), 0, 0);
        }
        cancel_referenced(&victims[..n]);
        if send_close {
            send_close_brb(c);
        }
        true
    }
}

/// For every request in flight on `c`: if it is `sent`, take a reference and
/// return it in `victims` (cancel after dropping the lock); otherwise leave
/// `cancel_wanted` for the submitter. Caller holds the lock.
unsafe fn collect_cancels_locked(c: *mut Channel, victims: &mut [WDFREQUEST]) -> usize {
    unsafe {
        let mut n = 0usize;
        let head = addr_of_mut!((*c).pending);
        let mut e = (*head).Flink;
        while e != head {
            let rc = ctx_from_link(e);
            (*rc).disconnected |= (*c).remote_disconnected;
            if !(*rc).sent {
                (*rc).cancel_wanted = true;
            } else if n < victims.len() {
                obj_ref((*rc).request);
                victims[n] = (*rc).request;
                n += 1;
            }
            e = (*e).Flink;
        }
        n
    }
}

/// Cancel requests referenced by [`collect_cancels_locked`] and drop the
/// references (lock NOT held: cancel can complete synchronously). This is the
/// documented "Synchronizing Cancellation of Sent Requests" sequence.
unsafe fn cancel_referenced(victims: &[WDFREQUEST]) {
    for &r in victims {
        unsafe {
            let _ = call_unsafe_wdf_function_binding!(WdfRequestCancelSentRequest, r);
            obj_deref(r);
        }
    }
}

/// Re-issue cancels for everything still in flight on incarnation
/// `generation` of `c` (defence in depth while waiting for a close).
unsafe fn recancel(c: *mut Channel, generation: u32) {
    unsafe {
        let dev = (*c).dev;
        let mut victims: [WDFREQUEST; MAX_PENDING_PER_CHANNEL as usize] = [null_mut(); MAX_PENDING_PER_CHANNEL as usize];
        lock(dev);
        let n = if (*c).generation == generation && matches!((*c).state, ChanState::Opening | ChanState::Closing) {
            collect_cancels_locked(c, &mut victims)
        } else {
            0
        };
        unlock(dev);
        cancel_referenced(&victims[..n]);
    }
}

unsafe fn obj_ref(r: WDFREQUEST) {
    unsafe { call_unsafe_wdf_function_binding!(WdfObjectReferenceActual, r.cast(), null_mut(), line!() as LONG, c"lib.rs".as_ptr().cast()) }
}
unsafe fn obj_deref(r: WDFREQUEST) {
    unsafe { call_unsafe_wdf_function_binding!(WdfObjectDereferenceActual, r.cast(), null_mut(), line!() as LONG, c"lib.rs".as_ptr().cast()) }
}

/// Sends CLOSE_CHANNEL on the channel's preallocated request (DISPATCH-safe).
unsafe fn send_close_brb(c: *mut Channel) {
    unsafe {
        let dev = (*c).dev;
        trace!("ch{} closing (handle {:p})", (*c).index, (*c).handle);
        let mut rp = WDF_REQUEST_REUSE_PARAMS {
            Size: size_of::<WDF_REQUEST_REUSE_PARAMS>() as ULONG,
            Flags: 0,
            Status: STATUS_NOT_SUPPORTED,
            NewIrp: null_mut(),
        };
        let _ = call_unsafe_wdf_function_binding!(WdfRequestReuse, (*c).close_req, &raw mut rp);
        let init = (*dev).profile.BthInitializeBrb.unwrap_unchecked();
        init(addr_of_mut!((*c).close_brb).cast(), _BRB_TYPE::BRB_L2CA_CLOSE_CHANNEL);
        (*c).close_brb.BtAddress = (*dev).remote_addr;
        (*c).close_brb.ChannelHandle = (*c).handle;
        let status = call_unsafe_wdf_function_binding!(
            WdfIoTargetFormatRequestForInternalIoctlOthers,
            (*dev).target,
            (*c).close_req,
            IOCTL_INTERNAL_BTH_SUBMIT_BRB,
            (*c).close_mem,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut()
        );
        if nt_success(status) {
            call_unsafe_wdf_function_binding!(WdfRequestSetCompletionRoutine, (*c).close_req, Some(close_complete), c.cast());
            if call_unsafe_wdf_function_binding!(WdfRequestSend, (*c).close_req, (*dev).target, null_mut()) != 0 {
                return;
            }
            let st = call_unsafe_wdf_function_binding!(WdfRequestGetStatus, (*c).close_req);
            trace!("ch{} close send failed {:#x}", (*c).index, st);
        } else {
            trace!("ch{} close format failed {:#x}", (*c).index, status);
        }
        // Could not send: the handle is as good as gone; finish locally.
        lock(dev);
        (*c).close_brb_done = true;
        let fin = maybe_finish_locked(c);
        unlock(dev);
        if fin {
            KeSetEvent(addr_of_mut!((*c).done), 0, 0);
        }
    }
}

unsafe extern "C" fn close_complete(_req: WDFREQUEST, _t: WDFIOTARGET, p: PWDF_REQUEST_COMPLETION_PARAMS, ctx: WDFCONTEXT) {
    unsafe {
        let c = ctx.cast::<Channel>();
        let dev = (*c).dev;
        trace!("ch{} close BRB done {:#x}", (*c).index, (*p).IoStatus.__bindgen_anon_1.Status);
        lock(dev);
        (*c).close_brb_done = true;
        let fin = maybe_finish_locked(c);
        unlock(dev);
        if fin {
            KeSetEvent(addr_of_mut!((*c).done), 0, 0);
        }
    }
}

/// If a Closing channel has nothing left in flight, move it to Free/Zombie.
/// Caller holds the lock; returns true if `done` must be signalled.
unsafe fn maybe_finish_locked(c: *mut Channel) -> bool {
    unsafe {
        if (*c).state == ChanState::Closing && (*c).close_brb_done && list_is_empty(addr_of_mut!((*c).pending)) {
            (*c).handle = null_mut();
            if (*c).remote_disconnected && !(*c).release_when_done {
                (*c).state = ChanState::Zombie;
            } else {
                release_slot_locked(c);
            }
            return true;
        }
        false
    }
}

unsafe fn release_slot_locked(c: *mut Channel) {
    unsafe {
        (*c).state = ChanState::Free;
        (*c).owner = null_mut();
        (*c).handle = null_mut();
        (*c).close_requested = false;
        (*c).remote_disconnected = false;
        (*c).close_brb_done = false;
        (*c).release_when_done = false;
    }
}

/// Close incarnation `generation` of `c` (it is freed, never left Zombie)
/// and wait for it, at most `budget` passes (PASSIVE). Returns true if it
/// finished; false if it was abandoned (see [`wait_closed`]).
unsafe fn close_sync(c: *mut Channel, generation: u32, budget: u32) -> bool {
    unsafe {
        if !begin_close(c, false, generation, true) {
            return true;
        }
        wait_closed(c, generation, budget) < budget || finished(c, generation)
    }
}

/// Incarnation `generation` of `c` is gone (Free, reused) or Zombie.
unsafe fn finished(c: *mut Channel, generation: u32) -> bool {
    unsafe {
        let dev = (*c).dev;
        lock(dev);
        let f = (*c).generation != generation || matches!((*c).state, ChanState::Free | ChanState::Zombie);
        unlock(dev);
        f
    }
}

/// Wait (PASSIVE) until incarnation `generation` of `c` is Free/Zombie, for
/// at most `budget` passes of 100 ms; re-issues cancels every second. On
/// timeout the slot is abandoned: it stays Closing (its close request may
/// still be in flight, so it must not be reused), loses its owner, and goes
/// straight to Free when bthport finally drains it. Returns passes used
/// (== `budget` on timeout).
unsafe fn wait_closed(c: *mut Channel, generation: u32, budget: u32) -> u32 {
    unsafe {
        let dev = (*c).dev;
        let mut used = 0u32;
        loop {
            if finished(c, generation) {
                return used;
            }
            if used >= budget {
                lock(dev);
                let abandoned = (*c).generation == generation && !matches!((*c).state, ChanState::Free | ChanState::Zombie);
                if abandoned {
                    (*c).owner = null_mut();
                    (*c).release_when_done = true;
                }
                let state = (*c).state;
                let pending = (*c).pending_count;
                unlock(dev);
                if abandoned {
                    trace!(
                        "ch{} close ABANDONED after {} ms: state={:?} pending={} (slot frees itself when bthport drains)",
                        (*c).index,
                        used * 100,
                        state,
                        pending
                    );
                }
                return budget;
            }
            // `done` is a notification event; the timed re-check covers the
            // case where the slot finished and was immediately reused (event
            // cleared again) before this thread started waiting.
            let mut timeout: LARGE_INTEGER = zeroed();
            timeout.QuadPart = CLOSE_PASS_100NS;
            let _ = KeWaitForSingleObject(addr_of_mut!((*c).done).cast(), _KWAIT_REASON::Executive, 0, 0, &raw mut timeout);
            used += 1;
            if used.is_multiple_of(RECANCEL_EVERY_PASSES) {
                lock(dev);
                let state = (*c).state;
                let pending = (*c).pending_count;
                unlock(dev);
                trace!("ch{} still closing after {} ms: state={:?} pending={}; re-cancelling", (*c).index, used * 100, state, pending);
                recancel(c, generation);
            }
        }
    }
}

/// bthport indication callback (IRQL <= DISPATCH).
unsafe extern "C" fn indication_cb(context: PVOID, code: INDICATION_CODE, params: PINDICATION_PARAMETERS) {
    unsafe {
        let c = context.cast::<Channel>();
        match code {
            _INDICATION_CODE::IndicationRemoteDisconnect => {
                let dev = (*c).dev;
                let p = &*params;
                let reason = p.Parameters.Disconnect.as_ref().Reason;
                lock(dev);
                // While Opening the handle is not known yet; accept it then too.
                let ours = match (*c).state {
                    ChanState::Free => false,
                    ChanState::Opening => true,
                    _ => (*c).handle == p.ConnectionHandle,
                };
                let generation = (*c).generation;
                unlock(dev);
                trace!("ch{} remote disconnect reason={} ours={}", (*c).index, reason, ours);
                if ours {
                    // As bthecho: send CLOSE_CHANNEL from here (async).
                    let _ = begin_close(c, true, generation, false);
                }
            }
            _INDICATION_CODE::IndicationAddReference | _INDICATION_CODE::IndicationReleaseReference => {}
            other => trace!("ch{} indication {}", (*c).index, other),
        }
    }
}

// ---------------------------------------------------------------------------
// I/O
// ---------------------------------------------------------------------------

unsafe extern "C" fn evt_io_stop(q: WDFQUEUE, req: WDFREQUEST, _flags: ULONG) {
    unsafe {
        let device = call_unsafe_wdf_function_binding!(WdfIoQueueGetDevice, q);
        let dev = device_ctx(device.cast());
        let rc = request_ctx(req.cast());
        lock(dev);
        let cancel_now = (*rc).on_list && (*rc).sent;
        if cancel_now {
            obj_ref(req);
        } else {
            // Not sent yet (or still being processed synchronously): the
            // submitter cancels it right after WdfRequestSend. Requests that
            // never reach submit_locked complete on their own.
            (*rc).cancel_wanted = true;
        }
        unlock(dev);
        if cancel_now {
            cancel_referenced(&[req]);
        }
    }
}

unsafe extern "C" fn evt_io_device_control(q: WDFQUEUE, req: WDFREQUEST, out_len: usize, in_len: usize, code: ULONG) {
    unsafe {
        let device = call_unsafe_wdf_function_binding!(WdfIoQueueGetDevice, q);
        let dev = device_ctx(device.cast());
        let rc = request_ctx(req.cast());
        (*rc).request = req;
        (*rc).kind = ReqKind::Other;
        (*rc).on_list = false;
        (*rc).disconnected = false;
        (*rc).sent = false;
        // cancel_wanted is deliberately not reset: EvtIoStop may already have
        // set it (WDF zeroes the context of every new request).

        let mut inb: PVOID = null_mut();
        if in_len > 0 {
            let st = call_unsafe_wdf_function_binding!(WdfRequestRetrieveInputBuffer, req, 0, &raw mut inb, null_mut());
            if !nt_success(st) {
                return complete(req, st, 0);
            }
        }
        // METHOD_BUFFERED: input and output are the same system buffer, and
        // completion (possibly synchronous, inside WdfRequestSend) writes the
        // reply there. So no `&[u8]` over the input may outlive decoding: the
        // handlers get raw parts (see `with_input`).
        let input = RawInput { ptr: inb.cast::<u8>().cast_const(), len: if inb.is_null() { 0 } else { in_len } };
        let mut outb: PVOID = null_mut();
        if out_len > 0 {
            let st = call_unsafe_wdf_function_binding!(WdfRequestRetrieveOutputBuffer, req, 0, &raw mut outb, null_mut());
            if !nt_success(st) {
                return complete(req, st, 0);
            }
        }
        (*rc).out_buf = outb.cast();
        (*rc).out_len = if outb.is_null() { 0 } else { out_len };

        let audio_code = audio::is_audio_ioctl(code);
        let pipe_code =
            matches!(code, ioc::IOCTL_OPEN_CHANNEL | ioc::IOCTL_CLOSE_CHANNEL | ioc::IOCTL_READ | ioc::IOCTL_WRITE);
        // Everything but GET_INFO is for SYSTEM and elevated administrators.
        if (audio_code || pipe_code) && !requestor_is_admin(req) {
            return complete(req, STATUS_ACCESS_DENIED, 0);
        }
        match code {
            ioc::IOCTL_GET_INFO => ioctl_get_info(dev, req, rc),
            ioc::IOCTL_OPEN_CHANNEL => ioctl_open(dev, req, rc, input),
            ioc::IOCTL_CLOSE_CHANNEL => ioctl_close(dev, req, input),
            ioc::IOCTL_READ => ioctl_transfer(dev, req, rc, input, ReqKind::Read),
            ioc::IOCTL_WRITE => ioctl_transfer(dev, req, rc, input, ReqKind::Write),
            _ if audio_code => audio::ioctl(dev, req, rc, input, code),
            _ => complete(req, STATUS_INVALID_DEVICE_REQUEST, 0),
        }
    }
}

/// The admin gate: true if `req` comes from kernel mode, or from a process
/// whose primary token holds the local Administrators group enabled, i.e.
/// SYSTEM or an *elevated* administrator. A UAC-filtered token carries that
/// group as deny-only and does not pass.
///
/// Why here and not in the device DACL: ACX needs the audio stack (LOCAL
/// SERVICE, interactive users) to be able to open this device object, and
/// the pipe and audio IOCTLs share it (see the INX).
///
/// The process is taken from the IRP, not from the current thread: the
/// default queue is passive-level, so the framework may run this callback
/// on a worker thread. `IoGetRequestorProcess` reads the thread the IRP was
/// issued on (`Irp->Tail.Overlay.Thread`), which stays valid while the IRP
/// is pending, and returns its process (NULL if the IRP has no thread:
/// denied).
///
/// ntoskrnl routines used (wdk-sys bindings generated from ntifs.h; all four
/// are documented, exported by ntoskrnl.exe and present in ntoskrnl.lib,
/// checked with `dumpbin /exports`):
/// * `PEPROCESS IoGetRequestorProcess(PIRP)`, IRQL <= DISPATCH;
/// * `PACCESS_TOKEN PsReferencePrimaryToken(PEPROCESS)`, PASSIVE; takes a
///   reference that must be dropped with
/// * `VOID PsDereferencePrimaryToken(PACCESS_TOKEN)`, IRQL < DISPATCH;
/// * `BOOLEAN SeTokenIsAdmin(PACCESS_TOKEN)`, PASSIVE.
///
/// No structure layout is involved: every argument is an opaque pointer.
/// Above PASSIVE (cannot happen with this queue) the answer is "no".
unsafe fn requestor_is_admin(req: WDFREQUEST) -> bool {
    unsafe {
        let mode = call_unsafe_wdf_function_binding!(WdfRequestGetRequestorMode, req);
        if i32::from(mode) == _MODE::KernelMode {
            return true;
        }
        if u32::from(KeGetCurrentIrql()) != PASSIVE_LEVEL {
            return false;
        }
        let irp = call_unsafe_wdf_function_binding!(WdfRequestWdmGetIrp, req);
        if irp.is_null() {
            return false;
        }
        let process = IoGetRequestorProcess(irp);
        if process.is_null() {
            return false;
        }
        let token = PsReferencePrimaryToken(process);
        if token.is_null() {
            return false;
        }
        let admin = SeTokenIsAdmin(token) != 0;
        PsDereferencePrimaryToken(token);
        admin
    }
}

/// Input buffer of a METHOD_BUFFERED request, as raw parts.
#[derive(Clone, Copy)]
struct RawInput {
    ptr: *const u8,
    len: usize,
}

/// Run `f` over the input bytes; the slice cannot escape `f`, so decode into
/// owned (Copy) values inside it.
unsafe fn with_input<R>(input: RawInput, f: impl FnOnce(&[u8]) -> R) -> R {
    if input.ptr.is_null() || input.len == 0 { f(&[]) } else { f(unsafe { core::slice::from_raw_parts(input.ptr, input.len) }) }
}

unsafe fn ioctl_get_info(dev: *mut DeviceContext, req: WDFREQUEST, rc: *mut RequestContext) {
    unsafe {
        if (*rc).out_len < ioc::InfoReply::SIZE {
            return complete(req, STATUS_BUFFER_TOO_SMALL, 0);
        }
        lock(dev);
        let mut open = 0u16;
        for i in 0..ioc::MAX_CHANNELS {
            if *addr_of_mut!((*dev).channels[i].state) != ChanState::Free {
                open += 1;
            }
        }
        unlock(dev);
        let mut flags = (*dev).addr_flags;
        if (*dev).in_d0.load(Ordering::SeqCst) {
            flags |= ioc::INFO_FLAG_D0;
        }
        let r = ioc::InfoReply {
            protocol_version: ioc::PROTOCOL_VERSION,
            driver_version: DRIVER_VERSION,
            remote_addr: (*dev).remote_addr,
            channels_open: open,
            channels_max: ioc::MAX_CHANNELS as u16,
            flags,
        }
        .encode();
        core::ptr::copy_nonoverlapping(r.as_ptr(), (*rc).out_buf, r.len());
        complete(req, STATUS_SUCCESS, r.len());
    }
}

/// Link `rc` into `c.pending` and hand it to bthport with the
/// already-initialised BRB in the request context. Lock must be held on entry;
/// it is released here. On failure the request is completed.
unsafe fn submit_locked(dev: *mut DeviceContext, c: *mut Channel, req: WDFREQUEST, rc: *mut RequestContext, brb_size: usize, done: PFN_WDF_REQUEST_COMPLETION_ROUTINE) {
    unsafe {
        (*rc).chan = c;
        (*rc).on_list = true;
        (*rc).sent = false;
        (*rc).disconnected = false;
        list_insert_tail(addr_of_mut!((*c).pending), addr_of_mut!((*rc).link));
        (*c).pending_count += 1;
        unlock(dev);

        let mut status;
        let mut mattr = attributes();
        mattr.ParentObject = req.cast();
        let mut mem: WDFMEMORY = null_mut();
        status = call_unsafe_wdf_function_binding!(WdfMemoryCreatePreallocated, &raw mut mattr, addr_of_mut!((*rc).brb).cast(), brb_size, &raw mut mem);
        if nt_success(status) {
            status = call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForInternalIoctlOthers,
                (*dev).target,
                req,
                IOCTL_INTERNAL_BTH_SUBMIT_BRB,
                mem,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut()
            );
        }
        if nt_success(status) {
            call_unsafe_wdf_function_binding!(WdfRequestSetCompletionRoutine, req, done, c.cast());
            // Keep the handle valid across the post-send check even if the
            // completion routine runs (and completes the request) first.
            obj_ref(req);
            if call_unsafe_wdf_function_binding!(WdfRequestSend, req, (*dev).target, null_mut()) != 0 {
                // Close/stop may have looked at this request between the
                // link above and now; they left `cancel_wanted` for us.
                lock(dev);
                let in_flight = (*rc).on_list;
                if in_flight {
                    (*rc).sent = true;
                }
                let cancel = in_flight && (*rc).cancel_wanted;
                unlock(dev);
                if cancel {
                    trace!("ch{} cancelling a request stopped/closed while it was being sent", (*c).index);
                    let _ = call_unsafe_wdf_function_binding!(WdfRequestCancelSentRequest, req);
                }
                obj_deref(req);
                return; // completion routine owns it now
            }
            obj_deref(req);
            status = call_unsafe_wdf_function_binding!(WdfRequestGetStatus, req);
        }
        trace!("ch{} submit failed {:#x}", (*c).index, status);
        // Undo and fail; this path never ran the completion routine.
        let fin = unlink_request(c, rc);
        if fin {
            KeSetEvent(addr_of_mut!((*c).done), 0, 0);
        }
        if (*rc).kind == ReqKind::Open {
            open_failed(c);
        }
        complete(req, status, 0);
    }
}

/// Remove `rc` from its channel's pending list; returns true if `done` must
/// be signalled (channel finished closing).
unsafe fn unlink_request(c: *mut Channel, rc: *mut RequestContext) -> bool {
    unsafe {
        let dev = (*c).dev;
        lock(dev);
        if (*rc).on_list {
            list_remove(addr_of_mut!((*rc).link));
            (*rc).on_list = false;
            (*c).pending_count -= 1;
        }
        (*rc).sent = false;
        let fin = maybe_finish_locked(c);
        unlock(dev);
        fin
    }
}

unsafe fn open_failed(c: *mut Channel) {
    unsafe {
        let dev = (*c).dev;
        lock(dev);
        let fin = (*c).state == ChanState::Opening;
        if fin {
            release_slot_locked(c);
        }
        unlock(dev);
        if fin {
            KeSetEvent(addr_of_mut!((*c).done), 0, 0);
        }
    }
}

unsafe fn ioctl_open(dev: *mut DeviceContext, req: WDFREQUEST, rc: *mut RequestContext, input: RawInput) {
    unsafe {
        let Ok(o) = with_input(input, ioc::OpenChannelRequest::decode) else {
            return complete(req, STATUS_BUFFER_TOO_SMALL, 0);
        };
        if (*rc).out_len < ioc::OpenChannelReply::SIZE {
            return complete(req, STATUS_BUFFER_TOO_SMALL, 0);
        }
        // Valid PSMs are odd, with the LSB of the upper octet clear.
        if o.psm & 0x0101 != 0x0001 {
            return complete(req, STATUS_INVALID_PARAMETER, 0);
        }
        if !(*dev).in_d0.load(Ordering::SeqCst) || (*dev).addr_flags & ioc::INFO_FLAG_ADDR_VALID == 0 {
            return complete(req, STATUS_DEVICE_NOT_READY, 0);
        }
        let file = call_unsafe_wdf_function_binding!(WdfRequestGetFileObject, req);

        lock(dev);
        let Some(i) = (0..ioc::MAX_CHANNELS).find(|&i| (*dev).channels[i].state == ChanState::Free) else {
            unlock(dev);
            return complete(req, STATUS_INSUFFICIENT_RESOURCES, 0);
        };
        let c = addr_of_mut!((*dev).channels[i]);
        (*c).state = ChanState::Opening;
        (*c).generation = (*dev).next_generation;
        (*dev).next_generation = ((*dev).next_generation % 0x00FF_FFFF) + 1;
        (*c).owner = file;
        (*c).psm = o.psm;
        (*c).close_requested = false;
        (*c).remote_disconnected = false;
        (*c).release_when_done = false;
        KeClearEvent(addr_of_mut!((*c).done));

        (*rc).kind = ReqKind::Open;
        let init = (*dev).profile.BthInitializeBrb.unwrap_unchecked();
        init(addr_of_mut!((*rc).brb).cast(), _BRB_TYPE::BRB_L2CA_OPEN_CHANNEL);
        let b = &mut *addr_of_mut!((*rc).brb).cast::<_BRB_L2CA_OPEN_CHANNEL>();
        b.BtAddress = (*dev).remote_addr;
        *b.__bindgen_anon_1.Psm.as_mut() = o.psm;
        b.ChannelFlags = CF_ROLE_EITHER;
        if o.flags & ioc::OPEN_FLAG_AUTHENTICATED != 0 {
            b.ChannelFlags |= CF_LINK_AUTHENTICATED;
        }
        if o.flags & ioc::OPEN_FLAG_ENCRYPTED != 0 {
            b.ChannelFlags |= CF_LINK_ENCRYPTED;
        }
        // ConfigOut = our configuration request to the peer: its MTU option
        // is the MTU *we* can receive. The value the peer accepted comes back
        // in OutResults.
        let in_mtu = o.effective_in_mtu();
        b.ConfigOut.Flags = CFG_MTU;
        b.ConfigOut.Mtu.Min = ioc::L2CAP_MIN_MTU;
        b.ConfigOut.Mtu.Preferred = in_mtu;
        b.ConfigOut.Mtu.Max = in_mtu;
        // ConfigIn = how to validate the peer's configuration request: its MTU
        // option is what *we* may send. The value the peer asked for (within
        // these bounds) comes back in InResults.
        let out_max = o.effective_out_mtu_max();
        b.ConfigIn.Flags = CFG_MTU;
        b.ConfigIn.Mtu.Min = ioc::L2CAP_MIN_MTU;
        b.ConfigIn.Mtu.Preferred = out_max;
        b.ConfigIn.Mtu.Max = out_max;
        b.CallbackFlags = CALLBACK_DISCONNECT;
        b.Callback = Some(indication_cb);
        b.CallbackContext = c.cast();
        b.ReferenceObject = call_unsafe_wdf_function_binding!(WdfDeviceWdmGetDeviceObject, (*dev).device).cast();
        b.IncomingQueueDepth = INCOMING_QUEUE_DEPTH;
        trace!("ch{} open psm={:#06x} in_mtu={} out_max={} flags={:#x}", i, o.psm, in_mtu, out_max, b.ChannelFlags);

        submit_locked(dev, c, req, rc, size_of::<_BRB_L2CA_OPEN_CHANNEL>(), Some(open_complete));
    }
}

unsafe extern "C" fn open_complete(req: WDFREQUEST, _t: WDFIOTARGET, p: PWDF_REQUEST_COMPLETION_PARAMS, ctx: WDFCONTEXT) {
    unsafe {
        let c = ctx.cast::<Channel>();
        let dev = (*c).dev;
        let rc = request_ctx(req.cast());
        let b = &*addr_of_mut!((*rc).brb).cast::<_BRB_L2CA_OPEN_CHANNEL>();
        let mut status = (*p).IoStatus.__bindgen_anon_1.Status;
        // InResults = outcome of the peer's configuration request (what we
        // may send); OutResults = outcome of ours (what we receive).
        trace!(
            "ch{} open done {:#x} brb.status={:#x} bt={} handle={:p} out_mtu={} in_mtu={}",
            (*c).index,
            status,
            b.Hdr.Status,
            b.Hdr.BtStatus,
            b.ChannelHandle,
            b.InResults.Params.Mtu,
            b.OutResults.Params.Mtu
        );

        let mut reply = None;
        let mut close_now = false;
        let generation = (*c).generation;
        let mut signal = false;
        lock(dev);
        if (*rc).on_list {
            list_remove(addr_of_mut!((*rc).link));
            (*rc).on_list = false;
            (*c).pending_count -= 1;
        }
        (*rc).sent = false;
        if nt_success(status) {
            (*c).handle = b.ChannelHandle;
            (*c).out_mtu = b.InResults.Params.Mtu;
            (*c).in_mtu = b.OutResults.Params.Mtu;
            (*c).state = ChanState::Open;
            if (*c).close_requested || (*c).remote_disconnected {
                // Owner went away / close or disconnect raced with the open.
                close_now = true;
                status = STATUS_CANCELLED;
            } else {
                reply = Some(ioc::OpenChannelReply { channel_id: chan_id(c), out_mtu: (*c).out_mtu, in_mtu: (*c).in_mtu });
            }
        } else {
            release_slot_locked(c);
            signal = true;
        }
        unlock(dev);
        if signal {
            KeSetEvent(addr_of_mut!((*c).done), 0, 0);
        }
        if close_now {
            // No reply carried the id to anyone, so nobody could ever release
            // a Zombie: free the slot when the close finishes.
            let _ = begin_close(c, false, generation, true);
        }
        match reply {
            Some(r) => {
                let e = r.encode();
                core::ptr::copy_nonoverlapping(e.as_ptr(), (*rc).out_buf, e.len());
                complete(req, STATUS_SUCCESS, e.len());
            }
            None => complete(req, status, 0),
        }
    }
}

unsafe fn ioctl_close(dev: *mut DeviceContext, req: WDFREQUEST, input: RawInput) {
    unsafe {
        let Ok(r) = with_input(input, ioc::ChannelRequest::decode) else {
            return complete(req, STATUS_BUFFER_TOO_SMALL, 0);
        };
        let file = call_unsafe_wdf_function_binding!(WdfRequestGetFileObject, req);
        lock(dev);
        let c = find_channel(dev, r.channel_id, file);
        unlock(dev);
        match c {
            None => complete(req, STATUS_INVALID_HANDLE, 0),
            Some(c) => {
                // Bounded: this request sits in the power-managed queue, so an
                // unbounded wait here would also block queue stop / D0Exit.
                if close_sync(c, ioc::channel_id::generation(r.channel_id), CLOSE_TIMEOUT_PASSES) {
                    complete(req, STATUS_SUCCESS, 0)
                } else {
                    complete(req, STATUS_IO_TIMEOUT, 0)
                }
            }
        }
    }
}

unsafe fn ioctl_transfer(dev: *mut DeviceContext, req: WDFREQUEST, rc: *mut RequestContext, input: RawInput, kind: ReqKind) {
    unsafe {
        let Ok(hdr) = with_input(input, ioc::ChannelRequest::decode) else {
            return complete(req, STATUS_BUFFER_TOO_SMALL, 0);
        };
        // WRITE payload follows the header; kept as raw parts (see RawInput).
        // decode() succeeded, so input.len >= ChannelRequest::SIZE.
        let (payload_ptr, payload_len) = match kind {
            ReqKind::Write => (input.ptr.add(ioc::ChannelRequest::SIZE), input.len - ioc::ChannelRequest::SIZE),
            _ => (core::ptr::null(), 0),
        };
        match kind {
            ReqKind::Write if (*rc).out_len < ioc::WriteReply::SIZE => return complete(req, STATUS_BUFFER_TOO_SMALL, 0),
            ReqKind::Read if (*rc).out_len == 0 => return complete(req, STATUS_BUFFER_TOO_SMALL, 0),
            _ => {}
        }
        let file = call_unsafe_wdf_function_binding!(WdfRequestGetFileObject, req);
        lock(dev);
        let Some(c) = find_channel(dev, hdr.channel_id, file) else {
            unlock(dev);
            return complete(req, STATUS_INVALID_HANDLE, 0);
        };
        let st = match (*c).state {
            ChanState::Open => STATUS_SUCCESS,
            ChanState::Zombie => STATUS_CONNECTION_DISCONNECTED,
            _ if (*c).remote_disconnected => STATUS_CONNECTION_DISCONNECTED,
            _ => STATUS_INVALID_DEVICE_STATE,
        };
        let st = if nt_success(st) && (*c).pending_count >= MAX_PENDING_PER_CHANNEL { STATUS_INSUFFICIENT_RESOURCES } else { st };
        // One WRITE is one SDU: at most the MTU the peer asked for (`out_mtu`).
        let st = if nt_success(st) && kind == ReqKind::Write && (payload_len == 0 || payload_len > (*c).out_mtu as usize) {
            STATUS_INVALID_BUFFER_SIZE
        } else {
            st
        };
        if !nt_success(st) {
            unlock(dev);
            return complete(req, st, 0);
        }

        (*rc).kind = kind;
        let init = (*dev).profile.BthInitializeBrb.unwrap_unchecked();
        init(addr_of_mut!((*rc).brb).cast(), _BRB_TYPE::BRB_L2CA_ACL_TRANSFER);
        let b = &mut *addr_of_mut!((*rc).brb).cast::<_BRB_L2CA_ACL_TRANSFER>();
        b.BtAddress = (*dev).remote_addr;
        b.ChannelHandle = (*c).handle;
        b.BufferMDL = null_mut();
        if kind == ReqKind::Read {
            // One READ = one inbound SDU. Data waits in bthport's per-channel
            // queue (IncomingQueueDepth) until a READ is pending.
            b.TransferFlags = ACL_TRANSFER_DIRECTION_IN | ACL_SHORT_TRANSFER_OK;
            b.Buffer = (*rc).out_buf.cast();
            b.BufferSize = (*rc).out_len.min(u32::MAX as usize) as ULONG;
        } else {
            b.TransferFlags = ACL_TRANSFER_DIRECTION_OUT;
            b.Buffer = payload_ptr.cast_mut().cast();
            b.BufferSize = payload_len as ULONG;
        }
        submit_locked(dev, c, req, rc, size_of::<_BRB_L2CA_ACL_TRANSFER>(), Some(transfer_complete));
    }
}

unsafe extern "C" fn transfer_complete(req: WDFREQUEST, _t: WDFIOTARGET, p: PWDF_REQUEST_COMPLETION_PARAMS, ctx: WDFCONTEXT) {
    unsafe {
        let c = ctx.cast::<Channel>();
        let rc = request_ctx(req.cast());
        let b = &*addr_of_mut!((*rc).brb).cast::<_BRB_L2CA_ACL_TRANSFER>();
        let mut status = (*p).IoStatus.__bindgen_anon_1.Status;
        let n = b.BufferSize as usize;
        if !nt_success(status) {
            trace!(
                "ch{} {} failed {:#x} bt={} remaining={}",
                (*c).index,
                if (*rc).kind == ReqKind::Read { "read" } else { "write" },
                status,
                b.Hdr.BtStatus,
                b.RemainingBufferSize
            );
            let dev = (*c).dev;
            lock(dev);
            let gone = (*rc).disconnected || (*c).remote_disconnected;
            unlock(dev);
            if gone {
                status = STATUS_CONNECTION_DISCONNECTED;
            }
        }
        let fin = unlink_request(c, rc);
        if fin {
            KeSetEvent(addr_of_mut!((*c).done), 0, 0);
        }
        if !nt_success(status) {
            return complete(req, status, 0);
        }
        if (*rc).kind == ReqKind::Read {
            complete(req, STATUS_SUCCESS, n.min((*rc).out_len));
        } else {
            let e = ioc::WriteReply { bytes_written: n as u32 }.encode();
            core::ptr::copy_nonoverlapping(e.as_ptr(), (*rc).out_buf, e.len());
            complete(req, STATUS_SUCCESS, e.len());
        }
    }
}
