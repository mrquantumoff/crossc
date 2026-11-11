//! Audio path, kernel side: the `crossc_ioctl::audio` IOCTLs and the PCM tap
//! that feeds them. All decisions are made by [`Session`] (`audio_core.rs`)
//! and by the loop over the queues of parked requests (`audio_park.rs`),
//! both pure code that is unit-tested on the host; this file only does what
//! needs the kernel: the spinlock, the request queues, completing requests,
//! and the entry points `acx.rs` calls from its stream callbacks.
//!
//! # Where pended requests live
//! ATTACH, WAIT_EVENT and READ_PCM pend. Each kind is parked in its own
//! *manual, not power-managed* WDFQUEUE (`WdfRequestForwardToIoQueue`) and
//! taken out again with `WdfIoQueueRetrieveNextRequest` when it can be
//! completed. While a request sits in such a queue the framework owns its
//! cancellation, and every way a request can die reaches the driver as one
//! callback, `EvtIoCanceledOnQueue`:
//! * `CancelIoEx`, thread or process exit (the IRP is cancelled);
//! * `CloseHandle` with the request still pending: on IRP_MJ_CLEANUP the
//!   framework cancels what is *queued* for that file object
//!   (`FxPkgGeneral::OnCleanup` -> `FlushAllQueuesByFileObject`). It does
//!   not do that for requests a driver holds itself ("it's the
//!   responsibility of the driver to complete them when the cleanup callback
//!   is invoked"), and with ACX on the device our cleanup callback is not
//!   called at all (see `evt_file_object_cleanup` in `lib.rs`). That is why
//!   the requests are queued rather than held with
//!   `WdfRequestMarkCancelableEx`;
//! * device removal (the framework purges the queues).
//!
//! Power-down is the one case the framework does not cover for a queue that
//! is not power-managed, so `evt_d0_exit` / `evt_self_managed_io_flush` call
//! [`detach_all`].
//!
//! # Locking
//! One WDFSPINLOCK (`AudioDev::lock`) guards the [`Session`] and the ring
//! storage pointer. It is held for bookkeeping and for one bounded `memcpy`
//! (a packet into the ring, or ring frames into a reply), nothing else:
//! * never across a queue call (`Forward`, `Retrieve`, `Requeue`) or a
//!   completion: those can run `EvtIoCanceledOnQueue` inline, which takes
//!   the lock;
//! * never together with the pipe lock (`DeviceContext::lock`);
//! * never across an ACX call.
//!
//! # Parked WAIT_EVENT / READ_PCM
//! The session gives every request it parks a ticket (`audio_core::Ticket`,
//! never reused) and remembers the one that is parked per kind. The ticket is
//! written into the request's context before the request is forwarded, so a
//! request in a queue is either *the* parked one or a leftover, whatever
//! handle it came through.
//!
//! Because the lock is dropped between "decide to park" and "forward", and
//! because a request is out of its queue while somebody looks at it, three
//! things can happen. The loop that deals with them is
//! [`audio_park::service`] (pure code, run on the host against a simulated
//! queue); [`Port`] gives it the WDF queue:
//! * a request reaches its queue after its session has ended (also when the
//!   same handle has attached again since): it is a leftover, and is
//!   completed with `STATUS_CANCELLED` by the sweep that follows its own
//!   forward, wherever in the queue it sits;
//! * what a request waits for happens before the request is in the queue:
//!   that is checked again right after the forward;
//! * a kick or a detach finds the queue empty because another thread holds
//!   the request at that moment: whoever puts a request back checks again
//!   afterwards, under the lock, that it may stay ([`Session::wait_rests`] /
//!   [`Session::read_rests`]) and goes round once more if not.
//!
//! # IRQL
//! IOCTL handlers run at PASSIVE (the default queue is passive-level). The
//! tap ([`render`]) runs in the stream's timer DPC. Everything else here is
//! written for IRQL <= DISPATCH; only the wait for L2CAP closes in
//! [`detach`] needs PASSIVE and is skipped above it.

use core::ptr::{addr_of_mut, null_mut};

use crossc_ioctl::audio as wire;
use wdk_sys::ntddk::KeGetCurrentIrql;
use wdk_sys::*;

use crate::audio_core::{Id, ReadDone, Refuse, Session, Ticket, WaitDone};
use crate::audio_park::{self, Parking, Verdict};
use crate::{
    CLOSE_TIMEOUT_PASSES, DeviceContext, RawInput, RequestContext, attributes, close_all, complete, device_ctx,
    nt_success, request_ctx, trace, with_input,
};

/// Most ATTACH requests taken out of the attach queue in one go. The queue
/// holds at most one, so this is never reached; it only bounds the loop.
const MAX_SWEEP: u32 = 16;

/// Audio state of one device, part of the pipe's [`DeviceContext`].
#[repr(C)]
pub struct AudioDev {
    /// Guards `session`, `ring`, `ring_len`.
    lock: WDFSPINLOCK,
    session: Session,
    /// Ring storage of the stream that feeds the tap (nonpaged pool owned by
    /// that stream, see `acx.rs`), or null. `ring_len` is the session's ring
    /// capacity.
    ring: *mut u8,
    ring_len: usize,
    /// ACX is on for this device: the queues below exist. Written once in
    /// DeviceAdd.
    enabled: bool,
    attach_queue: WDFQUEUE,
    wait_queue: WDFQUEUE,
    read_queue: WDFQUEUE,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Parked {
    Wait,
    Read,
}

fn status_of(r: Refuse) -> NTSTATUS {
    match r {
        Refuse::NotSupported => STATUS_NOT_SUPPORTED,
        Refuse::Denied => STATUS_ACCESS_DENIED,
        Refuse::Busy => STATUS_DEVICE_BUSY,
        Refuse::TooSmall => STATUS_BUFFER_TOO_SMALL,
    }
}

/// One of the audio IOCTL codes.
pub fn is_audio_ioctl(code: ULONG) -> bool {
    matches!(
        code,
        wire::IOCTL_AUDIO_ATTACH
            | wire::IOCTL_AUDIO_SET_LINK
            | wire::IOCTL_AUDIO_GET_STATE
            | wire::IOCTL_AUDIO_WAIT_EVENT
            | wire::IOCTL_AUDIO_READ_PCM
    )
}

unsafe fn audio(dev: *mut DeviceContext) -> *mut AudioDev {
    unsafe { addr_of_mut!((*dev).audio) }
}

unsafe fn lock(a: *mut AudioDev) {
    unsafe { call_unsafe_wdf_function_binding!(WdfSpinLockAcquire, (*a).lock) }
}
unsafe fn unlock(a: *mut AudioDev) {
    unsafe { call_unsafe_wdf_function_binding!(WdfSpinLockRelease, (*a).lock) }
}

/// The session. SAFETY: the caller holds the audio lock and lets the
/// reference go before releasing it.
unsafe fn session<'a>(a: *mut AudioDev) -> &'a mut Session {
    unsafe { &mut *addr_of_mut!((*a).session) }
}

/// The installed ring storage (empty if none). SAFETY: as [`session`].
unsafe fn storage<'a>(a: *mut AudioDev) -> &'a mut [u8] {
    unsafe {
        if (*a).ring.is_null() || (*a).ring_len == 0 {
            &mut []
        } else {
            core::slice::from_raw_parts_mut((*a).ring, (*a).ring_len)
        }
    }
}

/// Output buffer of a request (METHOD_BUFFERED system buffer: nonpaged,
/// valid until the request is completed). SAFETY: the caller owns the
/// request.
unsafe fn out_buffer<'a>(rc: *mut RequestContext) -> &'a mut [u8] {
    unsafe {
        if (*rc).out_buf.is_null() || (*rc).out_len == 0 {
            &mut []
        } else {
            core::slice::from_raw_parts_mut((*rc).out_buf, (*rc).out_len)
        }
    }
}

/// Identity of the handle a request came through.
unsafe fn requester(req: WDFREQUEST) -> Id {
    unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetFileObject, req) as Id }
}

unsafe fn queue_of(a: *mut AudioDev, kind: Parked) -> WDFQUEUE {
    unsafe {
        match kind {
            Parked::Wait => (*a).wait_queue,
            Parked::Read => (*a).read_queue,
        }
    }
}

/// Take the request at the head of `queue`, if any. The driver owns it then.
pub unsafe fn retrieve(queue: WDFQUEUE) -> Option<WDFREQUEST> {
    unsafe {
        if queue.is_null() {
            return None;
        }
        let mut req: WDFREQUEST = null_mut();
        let st = call_unsafe_wdf_function_binding!(WdfIoQueueRetrieveNextRequest, queue, &raw mut req);
        if nt_success(st) && !req.is_null() { Some(req) } else { None }
    }
}

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

pub unsafe fn create_manual_queue(
    device: WDFDEVICE,
    level: WDF_EXECUTION_LEVEL,
    canceled: PFN_WDF_IO_QUEUE_IO_CANCELED_ON_QUEUE,
    out: *mut WDFQUEUE,
) -> NTSTATUS {
    unsafe {
        // WDF_IO_QUEUE_CONFIG_INIT(WdfIoQueueDispatchManual), then not
        // power-managed: a parked request must stay retrievable (and
        // completable) while the device is leaving D0.
        let mut cfg = WDF_IO_QUEUE_CONFIG {
            Size: size_of::<WDF_IO_QUEUE_CONFIG>() as ULONG,
            DispatchType: _WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchManual,
            PowerManaged: _WDF_TRI_STATE::WdfFalse,
            DefaultQueue: 0,
            EvtIoCanceledOnQueue: canceled,
            ..Default::default()
        };
        let mut attr = attributes();
        attr.ExecutionLevel = level;
        attr.SynchronizationScope = _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeNone;
        call_unsafe_wdf_function_binding!(WdfIoQueueCreate, device, &raw mut cfg, &raw mut attr, out)
    }
}

/// DeviceAdd, after WdfDeviceCreate. With `acx_on` false nothing but the
/// lock is created and every audio IOCTL except GET_STATE reports
/// `STATUS_NOT_SUPPORTED`.
pub unsafe fn init(dev: *mut DeviceContext, device: WDFDEVICE, acx_on: bool) -> NTSTATUS {
    unsafe {
        let a = audio(dev);
        // The device context is zero-filled; give the session its real
        // initial value before anything can look at it.
        addr_of_mut!((*a).session).write(Session::new());
        (*a).ring = null_mut();
        (*a).ring_len = 0;
        let mut lattr = attributes();
        lattr.ParentObject = device.cast();
        let status = call_unsafe_wdf_function_binding!(WdfSpinLockCreate, &raw mut lattr, addr_of_mut!((*a).lock));
        if !nt_success(status) || !acx_on {
            return status;
        }
        // The ATTACH cancel path closes the session's L2CAP channels and
        // waits for that, so its callback is asked for at PASSIVE. The other
        // two queues are emptied from the stream's timer DPC: DISPATCH.
        let queues: [(WDF_EXECUTION_LEVEL, PFN_WDF_IO_QUEUE_IO_CANCELED_ON_QUEUE, *mut WDFQUEUE); 3] = [
            (_WDF_EXECUTION_LEVEL::WdfExecutionLevelPassive, Some(evt_attach_canceled), addr_of_mut!((*a).attach_queue)),
            (_WDF_EXECUTION_LEVEL::WdfExecutionLevelDispatch, Some(evt_wait_canceled), addr_of_mut!((*a).wait_queue)),
            (_WDF_EXECUTION_LEVEL::WdfExecutionLevelDispatch, Some(evt_read_canceled), addr_of_mut!((*a).read_queue)),
        ];
        for (level, canceled, out) in queues {
            let status = create_manual_queue(device, level, canceled, out);
            if !nt_success(status) {
                trace!("audio: queue creation failed {:#x}", status);
                return status;
            }
        }
        (*a).enabled = true;
        STATUS_SUCCESS
    }
}

/// ACX is on for this device (the audio IOCTLs can work at all).
pub unsafe fn enabled(dev: *mut DeviceContext) -> bool {
    unsafe { (*audio(dev)).enabled }
}

// ---------------------------------------------------------------------------
// IOCTLs (PASSIVE, from evt_io_device_control, after the admin check)
// ---------------------------------------------------------------------------

pub unsafe fn ioctl(dev: *mut DeviceContext, req: WDFREQUEST, rc: *mut RequestContext, input: RawInput, code: ULONG) {
    unsafe {
        // No endpoint (ACX off by either kill switch, or no circuit): only
        // GET_STATE works. Checked before the buffers, so the answer does
        // not depend on what the caller passed. (The session checks it again
        // under the lock together with everything else.)
        if code != wire::IOCTL_AUDIO_GET_STATE {
            let a = audio(dev);
            lock(a);
            let supported = session(a).endpoint();
            unlock(a);
            if !supported {
                return complete(req, STATUS_NOT_SUPPORTED, 0);
            }
        }
        match code {
            wire::IOCTL_AUDIO_GET_STATE => ioctl_get_state(dev, req, rc),
            wire::IOCTL_AUDIO_ATTACH => ioctl_attach(dev, req),
            wire::IOCTL_AUDIO_SET_LINK => ioctl_set_link(dev, req, input),
            wire::IOCTL_AUDIO_WAIT_EVENT => ioctl_wait_event(dev, req, rc, input),
            wire::IOCTL_AUDIO_READ_PCM => ioctl_read_pcm(dev, req, rc),
            _ => complete(req, STATUS_INVALID_DEVICE_REQUEST, 0),
        }
    }
}

/// Copy a state reply into the request's output buffer and complete it.
unsafe fn complete_with_state(req: WDFREQUEST, rc: *mut RequestContext, state: &wire::AudioState) {
    unsafe {
        let e = state.encode();
        match out_buffer(rc).get_mut(..e.len()) {
            Some(dst) => {
                dst.copy_from_slice(&e);
                complete(req, STATUS_SUCCESS, e.len());
            }
            None => complete(req, STATUS_BUFFER_TOO_SMALL, 0),
        }
    }
}

/// GET_STATE: any (admin) handle, never pends, works with ACX off (the
/// flags then lack AUDIO_FLAG_ACX).
unsafe fn ioctl_get_state(dev: *mut DeviceContext, req: WDFREQUEST, rc: *mut RequestContext) {
    unsafe {
        if (*rc).out_len < wire::AudioState::SIZE {
            return complete(req, STATUS_BUFFER_TOO_SMALL, 0);
        }
        let a = audio(dev);
        lock(a);
        let state = session(a).state();
        unlock(a);
        complete_with_state(req, rc, &state);
    }
}

/// ATTACH: becomes the session holder and pends in the attach queue for the
/// life of the session.
unsafe fn ioctl_attach(dev: *mut DeviceContext, req: WDFREQUEST) {
    unsafe {
        let a = audio(dev);
        let who = requester(req);
        lock(a);
        let r = session(a).attach(who);
        unlock(a);
        if let Err(e) = r {
            return complete(req, status_of(e), 0);
        }
        trace!("audio: session attached");
        // From a successful forward on, the request belongs to the queue and
        // may already be cancelled (and completed): do not touch it again.
        let st = call_unsafe_wdf_function_binding!(WdfRequestForwardToIoQueue, req, (*a).attach_queue);
        if !nt_success(st) {
            trace!("audio: ATTACH could not be parked {:#x}", st);
            // Undo the attach only; the handle keeps its channels.
            let _ = end_session(dev, who, "ATTACH not parked", false);
            complete(req, st, 0);
        }
        // No orphan check here (unlike WAIT/READ): the only things that end a
        // session without going through this very request are detach_all()
        // (D0Exit / flush, which the framework runs after the default queue
        // has stopped, i.e. after this handler returned) and file_gone()
        // (IRP_MJ_CLOSE, impossible while this IRP is alive).
    }
}

/// SET_LINK: the jack follows the flag.
unsafe fn ioctl_set_link(dev: *mut DeviceContext, req: WDFREQUEST, input: RawInput) {
    unsafe {
        let Ok(r) = with_input(input, wire::SetLinkRequest::decode) else {
            return complete(req, STATUS_BUFFER_TOO_SMALL, 0);
        };
        let a = audio(dev);
        let who = requester(req);
        lock(a);
        let res = session(a).set_link(who, r.connected);
        unlock(a);
        match res {
            Err(e) => complete(req, status_of(e), 0),
            Ok(changed) => {
                if changed {
                    trace!("audio: link {}", if r.connected { "up" } else { "down" });
                    crate::acx::jack_changed((*dev).device);
                }
                complete(req, STATUS_SUCCESS, 0);
                if changed {
                    kick_wait(dev);
                }
            }
        }
    }
}

/// WAIT_EVENT: hanging get on the sequence number.
unsafe fn ioctl_wait_event(dev: *mut DeviceContext, req: WDFREQUEST, rc: *mut RequestContext, input: RawInput) {
    unsafe {
        let Ok(r) = with_input(input, wire::WaitEventRequest::decode) else {
            return complete(req, STATUS_BUFFER_TOO_SMALL, 0);
        };
        if (*rc).out_len < wire::AudioState::SIZE {
            return complete(req, STATUS_BUFFER_TOO_SMALL, 0);
        }
        let a = audio(dev);
        let who = requester(req);
        lock(a);
        let res = session(a).begin_wait(who, r.seq);
        let ticket = session(a).wait_ticket();
        unlock(a);
        match res {
            Err(e) => complete(req, status_of(e), 0),
            Ok(Some(state)) => complete_with_state(req, rc, &state),
            Ok(None) => park(dev, req, rc, ticket, Parked::Wait),
        }
    }
}

/// READ_PCM: the next chunk of the tap, at once if frames are waiting.
unsafe fn ioctl_read_pcm(dev: *mut DeviceContext, req: WDFREQUEST, rc: *mut RequestContext) {
    unsafe {
        let a = audio(dev);
        let who = requester(req);
        let out = out_buffer(rc);
        lock(a);
        let res = session(a).begin_read(who, storage(a), out);
        let ticket = session(a).read_ticket();
        unlock(a);
        match res {
            Err(e) => complete(req, status_of(e), 0),
            Ok(0) => park(dev, req, rc, ticket, Parked::Read),
            Ok(n) => complete(req, STATUS_SUCCESS, n),
        }
    }
}

/// The session decided (under the lock, already released) that `req` pends
/// as `ticket`: put it into its queue.
unsafe fn park(dev: *mut DeviceContext, req: WDFREQUEST, rc: *mut RequestContext, ticket: Ticket, kind: Parked) {
    unsafe {
        let a = audio(dev);
        // Still ours: nobody else can see the request before the forward.
        (*rc).audio_ticket = ticket;
        let st = call_unsafe_wdf_function_binding!(WdfRequestForwardToIoQueue, req, queue_of(a, kind));
        if !nt_success(st) {
            gone(a, ticket, kind);
            return complete(req, st, 0);
        }
        // `req` belongs to the queue now. Its session may have ended and
        // what it waits for may have happened since the lock was dropped.
        audio_park::parked(&mut Port { dev, kind }, ticket);
    }
}

/// The request `ticket` no longer exists.
unsafe fn gone(a: *mut AudioDev, ticket: Ticket, kind: Parked) {
    unsafe {
        lock(a);
        match kind {
            Parked::Wait => session(a).wait_gone(ticket),
            Parked::Read => session(a).read_gone(ticket),
        }
        unlock(a);
    }
}

/// The request `ticket` may stay in its queue: it is the parked one and what
/// it waits for has not happened.
unsafe fn rests(a: *mut AudioDev, ticket: Ticket, kind: Parked) -> bool {
    unsafe {
        lock(a);
        let rests = match kind {
            Parked::Wait => session(a).wait_rests(ticket),
            Parked::Read => session(a).read_rests(ticket),
        };
        unlock(a);
        rests
    }
}

/// Put a request we took out back at the head of its queue. If that fails
/// the request is failed and false is returned.
unsafe fn requeue(a: *mut AudioDev, req: WDFREQUEST, ticket: Ticket, kind: Parked) -> bool {
    unsafe {
        let st = call_unsafe_wdf_function_binding!(WdfRequestRequeue, req);
        if nt_success(st) {
            return true;
        }
        trace!("audio: requeue failed {:#x}", st);
        gone(a, ticket, kind);
        complete(req, STATUS_CANCELLED, 0);
        false
    }
}

/// How a parked request is completed: status and reply length.
type Reply = (NTSTATUS, usize);

/// Decide about the request `rc` / `ticket` we own, and write its reply if
/// it is completed with one.
unsafe fn settle(a: *mut AudioDev, rc: *mut RequestContext, ticket: Ticket, kind: Parked) -> Verdict<Reply> {
    unsafe {
        // We own the request: its context and buffer are ours to use.
        let out = out_buffer(rc);
        match kind {
            Parked::Wait => {
                lock(a);
                let done = session(a).finish_wait(ticket);
                unlock(a);
                match done {
                    WaitDone::Stale => Verdict::Complete((STATUS_CANCELLED, 0)),
                    WaitDone::NotReady => Verdict::Keep,
                    WaitDone::Done(state) => {
                        let e = state.encode();
                        // The size was checked before the request was parked.
                        match out.get_mut(..e.len()) {
                            Some(dst) => {
                                dst.copy_from_slice(&e);
                                Verdict::Complete((STATUS_SUCCESS, e.len()))
                            }
                            None => Verdict::Complete((STATUS_BUFFER_TOO_SMALL, 0)),
                        }
                    }
                }
            }
            Parked::Read => {
                lock(a);
                let done = session(a).finish_read(ticket, storage(a), out);
                unlock(a);
                match done {
                    ReadDone::Stale => Verdict::Complete((STATUS_CANCELLED, 0)),
                    // The ring was emptied since somebody saw frames in it
                    // (stream or format change): keep waiting.
                    ReadDone::Empty => Verdict::Keep,
                    ReadDone::TooSmall => Verdict::Complete((STATUS_BUFFER_TOO_SMALL, 0)),
                    ReadDone::Done(n) => Verdict::Complete((STATUS_SUCCESS, n)),
                }
            }
        }
    }
}

/// The queue of parked `kind` requests of a device, as
/// [`audio_park::service`] sees it.
///
/// Only made by [`service`] and [`park`], for the duration of one call, from
/// a `dev` their caller vouches for; with no lock held, at IRQL <= DISPATCH.
/// That is what the `unsafe` blocks below rely on.
struct Port {
    dev: *mut DeviceContext,
    kind: Parked,
}

impl Parking for Port {
    type Request = WDFREQUEST;
    type Reply = Reply;

    fn retrieve(&mut self) -> Option<(WDFREQUEST, Ticket)> {
        unsafe {
            let req = retrieve(queue_of(audio(self.dev), self.kind))?;
            // The driver owns the request now, so its context can be read.
            Some((req, (*request_ctx(req.cast())).audio_ticket))
        }
    }

    fn settle(&mut self, request: &WDFREQUEST, ticket: Ticket) -> Verdict<Reply> {
        unsafe { settle(audio(self.dev), request_ctx((*request).cast()), ticket, self.kind) }
    }

    fn complete(&mut self, request: WDFREQUEST, (status, len): Reply) {
        unsafe { complete(request, status, len) }
    }

    fn cancel(&mut self, request: WDFREQUEST) {
        unsafe { complete(request, STATUS_CANCELLED, 0) }
    }

    fn requeue(&mut self, request: WDFREQUEST, ticket: Ticket) -> bool {
        unsafe { requeue(audio(self.dev), request, ticket, self.kind) }
    }

    fn rests(&mut self, ticket: Ticket) -> bool {
        unsafe { rests(audio(self.dev), ticket, self.kind) }
    }
}

/// Bring the queue of `kind` in line with the session: leftovers in it are
/// completed with STATUS_CANCELLED, the parked request is completed if what
/// it waits for has happened and stays parked otherwise
/// ([`audio_park::service`]). Call with no lock held. IRQL <= DISPATCH.
unsafe fn service(dev: *mut DeviceContext, kind: Parked) {
    audio_park::service(&mut Port { dev, kind });
}

/// Complete the parked WAIT_EVENT if the state has changed. Call after
/// every change of the state, with no lock held. IRQL <= DISPATCH.
pub unsafe fn kick_wait(dev: *mut DeviceContext) {
    unsafe {
        let a = audio(dev);
        if !(*a).enabled {
            return;
        }
        lock(a);
        let ready = session(a).wait_ready();
        unlock(a);
        // If the request is not in its queue at this moment (not forwarded
        // yet, or held by another thread), service() finds nothing; then
        // the check after its forward, or that thread's own check, sees the
        // change.
        if ready {
            service(dev, Parked::Wait);
        }
    }
}

// ---------------------------------------------------------------------------
// Cancellation and detach
// ---------------------------------------------------------------------------

/// A parked ATTACH was cancelled: CancelIoEx, its handle closed, its process
/// died, or the device is being removed. The driver owns the request now.
/// PASSIVE by the queue's execution level; written for <= DISPATCH.
unsafe extern "C" fn evt_attach_canceled(queue: WDFQUEUE, req: WDFREQUEST) {
    unsafe {
        let device = call_unsafe_wdf_function_binding!(WdfIoQueueGetDevice, queue);
        let dev = device_ctx(device.cast());
        let who = requester(req);
        let _ = detach(dev, who, "ATTACH cancelled or its handle closed");
        // Completed last: when the caller sees the ATTACH end, the session
        // is gone and its channels are closed.
        complete(req, STATUS_CANCELLED, 0);
    }
}

unsafe extern "C" fn evt_wait_canceled(queue: WDFQUEUE, req: WDFREQUEST) {
    unsafe { parked_canceled(queue, req, Parked::Wait) }
}

unsafe extern "C" fn evt_read_canceled(queue: WDFQUEUE, req: WDFREQUEST) {
    unsafe { parked_canceled(queue, req, Parked::Read) }
}

/// A parked WAIT_EVENT / READ_PCM was cancelled. IRQL <= DISPATCH.
unsafe fn parked_canceled(queue: WDFQUEUE, req: WDFREQUEST, kind: Parked) {
    unsafe {
        let device = call_unsafe_wdf_function_binding!(WdfIoQueueGetDevice, queue);
        let dev = device_ctx(device.cast());
        // The driver owns the request here, so its context can be read. A
        // leftover's ticket matches nothing and frees nothing.
        let ticket = (*request_ctx(req.cast())).audio_ticket;
        gone(audio(dev), ticket, kind);
        complete(req, STATUS_CANCELLED, 0);
    }
}

/// End the session of handle `who` (0 = whoever is attached): link flag
/// cleared and jack unplugged, tap off, that session's WAIT_EVENT / READ_PCM
/// cancelled, that handle's L2CAP channels closed. Returns false if `who`
/// is not attached. Does not touch the ATTACH request (the callers own it).
/// IRQL <= DISPATCH; below DISPATCH it waits for the channel closes (same
/// bound as every other synchronous close).
pub unsafe fn detach(dev: *mut DeviceContext, who: Id, why: &str) -> bool {
    unsafe { end_session(dev, who, why, true) }
}

unsafe fn end_session(dev: *mut DeviceContext, who: Id, why: &str, close_channels: bool) -> bool {
    unsafe {
        let a = audio(dev);
        if !(*a).enabled {
            return false;
        }
        lock(a);
        let holder = session(a).holder();
        let ended = session(a).detach(who);
        unlock(a);
        let Some(ended) = ended else {
            return false;
        };
        // The session forgot its parked requests, so they are leftovers now.
        service(dev, Parked::Wait);
        service(dev, Parked::Read);
        if ended.link_changed {
            crate::acx::jack_changed((*dev).device);
        }
        let closed = if close_channels {
            let budget = if u32::from(KeGetCurrentIrql()) < DISPATCH_LEVEL { CLOSE_TIMEOUT_PASSES } else { 0 };
            close_all(dev, Some(holder as WDFFILEOBJECT), budget)
        } else {
            0
        };
        trace!("audio: session detached ({}): link was {}, {} channel(s) closed", why, ended.link_changed, closed);
        true
    }
}

/// End whatever session exists and complete its ATTACH: the device leaves
/// D0 or goes away. PASSIVE (D0Exit, SelfManagedIoFlush).
pub unsafe fn detach_all(dev: *mut DeviceContext, why: &str) {
    unsafe {
        let a = audio(dev);
        if !(*a).enabled {
            return;
        }
        for _ in 0..MAX_SWEEP {
            let Some(req) = retrieve((*a).attach_queue) else {
                break;
            };
            let _ = detach(dev, requester(req), why);
            complete(req, STATUS_CANCELLED, 0);
        }
        // A holder without a parked ATTACH cannot exist; if it ever does,
        // do not leave the endpoint plugged in.
        let _ = detach(dev, 0, why);
    }
}

/// The file object of a handle is going away. Its ATTACH cannot be pending
/// any more (a pending IRP keeps the file object alive), so normally this
/// finds nothing. IRQL <= DISPATCH.
pub unsafe fn file_gone(dev: *mut DeviceContext, file: WDFFILEOBJECT) {
    unsafe {
        let _ = detach(dev, file as Id, "handle closed");
    }
}

// ---------------------------------------------------------------------------
// Endpoint, jack and tap (called from acx.rs)
// ---------------------------------------------------------------------------

/// The render circuit was added / is being removed. PASSIVE.
pub unsafe fn set_endpoint(dev: *mut DeviceContext, present: bool) {
    unsafe {
        let a = audio(dev);
        lock(a);
        let changed = session(a).set_endpoint(present);
        unlock(a);
        if changed {
            kick_wait(dev);
        }
    }
}

/// What the jack reports: the link flag. Any IRQL <= DISPATCH.
pub unsafe fn link(dev: *mut DeviceContext) -> bool {
    unsafe {
        let a = audio(dev);
        lock(a);
        let link = session(a).link();
        unlock(a);
        link
    }
}

/// Snapshot for the stream statistics trace: (state, frames stored as silence).
pub unsafe fn stats(dev: *mut DeviceContext) -> (wire::AudioState, u64) {
    unsafe {
        let a = audio(dev);
        lock(a);
        let s = (session(a).state(), session(a).silence_frames());
        unlock(a);
        s
    }
}

/// A stream with `format` was created. Returns true if it feeds the tap
/// from now on (false: the contract cannot describe the format; the stream
/// is a plain null sink). PASSIVE.
pub unsafe fn stream_created(dev: *mut DeviceContext, id: Id, format: wire::PcmFormat) -> bool {
    unsafe {
        let a = audio(dev);
        lock(a);
        let tapped = session(a).stream_created(id, format);
        if tapped {
            // An older stream's storage is no longer the ring's; that
            // stream frees it itself.
            (*a).ring = null_mut();
            (*a).ring_len = 0;
        }
        unlock(a);
        if tapped {
            kick_wait(dev);
        }
        tapped
    }
}

/// The stream object is being deleted. IRQL <= DISPATCH.
pub unsafe fn stream_destroyed(dev: *mut DeviceContext, id: Id) {
    unsafe {
        let a = audio(dev);
        lock(a);
        let was_tap = session(a).stream_destroyed(id);
        if was_tap {
            (*a).ring = null_mut();
            (*a).ring_len = 0;
        }
        unlock(a);
        if was_tap {
            kick_wait(dev);
        }
    }
}

/// Run / Pause. PASSIVE.
pub unsafe fn stream_running(dev: *mut DeviceContext, id: Id, running: bool) {
    unsafe {
        let a = audio(dev);
        lock(a);
        let changed = session(a).stream_running(id, running);
        unlock(a);
        if changed {
            kick_wait(dev);
        }
    }
}

/// Stream `id` offers `len` bytes of nonpaged storage at `ptr` for the ring.
/// Returns true if the ring uses it from now on; the stream must then call
/// [`ring_remove`] before freeing it. PASSIVE.
pub unsafe fn ring_install(dev: *mut DeviceContext, id: Id, ptr: *mut u8, len: usize) -> bool {
    unsafe {
        let a = audio(dev);
        if ptr.is_null() {
            return false;
        }
        lock(a);
        let used = session(a).install_ring(id, len);
        if used {
            (*a).ring = ptr;
            (*a).ring_len = session(a).ring_capacity();
        }
        unlock(a);
        used
    }
}

/// Stop using `ptr` as ring storage (no-op if the ring uses something
/// else). After this returns nothing reads or writes it any more: every
/// access is made under the lock taken here. IRQL <= DISPATCH.
pub unsafe fn ring_remove(dev: *mut DeviceContext, ptr: *mut u8) {
    unsafe {
        let a = audio(dev);
        lock(a);
        if !ptr.is_null() && (*a).ring == ptr {
            session(a).remove_ring();
            (*a).ring = null_mut();
            (*a).ring_len = 0;
        }
        unlock(a);
    }
}

/// The tap. Timer DPC of stream `id` (DISPATCH): the audio engine finished
/// the `len` bytes at `packet`; with `silence` the engine cannot have
/// written that packet and zeros are stored in its place.
///
/// Cost: the lock, one `memcpy` of a packet, and, only if a READ_PCM is
/// parked, two queue retrieves (the second finds the queue empty), one more
/// `memcpy` (at most the ring) and the completion. No allocation, no wait. A
/// reader that does not read costs nothing here: the ring drops its oldest
/// frames.
pub unsafe fn render(dev: *mut DeviceContext, id: Id, packet: *const u8, len: usize, silence: bool) {
    unsafe {
        let a = audio(dev);
        lock(a);
        if silence {
            session(a).render_silence(id, storage(a), len);
        } else if !packet.is_null() {
            session(a).render(id, storage(a), core::slice::from_raw_parts(packet, len));
        }
        let ready = session(a).read_ready();
        unlock(a);
        if ready {
            service(dev, Parked::Read);
        }
    }
}
