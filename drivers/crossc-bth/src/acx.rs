//! ACX (Audio Class eXtension) render endpoint.
//!
//! The same KMDF device that is the Bluetooth profile client (L2CAP pipe, see
//! `lib.rs`) is also an ACX device with one render circuit, "Headphones0".
//! The audio engine renders PCM into it; the stream engine here consumes the
//! packets in real time (timer-driven position + packet-complete
//! notifications) and copies each finished packet into the PCM tap
//! (`audio.rs`), from where the user-mode service reads it and encodes it.
//! With no service attached the audio is counted and dropped.
//!
//! The endpoint's jack reports the link flag the service sets (SET_LINK), so
//! Windows shows the device as unplugged while the buds are away or no
//! service is attached.
//!
//! Modelled on microsoft/Windows-driver-samples `audio/Acx/Samples`
//! (AudioCodec: `Device.cpp`, `RenderCircuit.cpp`, `StreamEngine.cpp`), minus
//! the S0-idle policy (D0Exit closes every L2CAP channel).
//!
//! # Unified volume (hardware volume through the inbox AVRCP stack)
//! Unless `DisableAvrcpVolume` is set (below), the render circuit also
//! provides the private contract the inbox AVRCP volume controller
//! (`BthAvrcp.dll` in BthAvctpSvc) drives for `BthA2dp.sys`, so the buds'
//! volume and the Windows slider become one control. Recovered from the
//! disassembly in `target/avrcp-research` (`inbox-contract.md`, `PLAN.md`);
//! nothing of it is documented:
//! * interface category `{e630310c-1733-42bb-9847-7e45d1a27490}`
//!   ([`KSCATEGORY_AVRCP_VOLUME`]): BthAvrcp and BthAvctpSvc find the
//!   interface by this class on the A2DP devnode (instance id + class +
//!   enabled; no reference string, no driver test). Assigned with
//!   `AcxCircuitInitAssignCategories`, which *replaces* ACX's own list, so
//!   [`RENDER_CATEGORIES`] repeats that list (required first entry
//!   `{2c6bb644-...}`, `Acx01000!AfxCircuit::FinishCategoryInitializing`).
//! * KS property `{401e566b-b47e-4e34-b0fc-94826e9e2a1a}` id 1 ([`AVRCP_VOLUME_SET`],
//!   GET/SET, one LONG in 1/65536 dB, -63.5..0 dB): BthAvrcp reads the level
//!   to send to the buds and writes the buds' level.
//! * KS event, same set, id 1: generated when the Windows-side level or mute
//!   changes; BthAvrcp then reads the property.
//! * KS method `{0e62540f-bac4-4b85-b344-0d82ed5008ac}` id 6
//!   ([`A2DP_SERVICE_METHOD_SET`], `MethodServiceReady`): sent by
//!   BthAvctpSvc when it finds the interface. BthA2dp parks it; its sender
//!   only traces the result (`lambda_9e75...::operator()` in BthAvctpSvc), so
//!   it is completed at once with success here.
//! * an `AcxVolume` and an `AcxMute` element with BthA2dp's range: Windows
//!   stops scaling the PCM and hands the level to the driver.
//!
//! The state and its rules (taper, echo rule, connect sync, mute) are in
//! `avrcp_vol.rs`; the service applies the level in software while nobody
//! on the AVRCP side answers (`crossc_ioctl::audio` "Volume"). ACX
//! marshals every one of these requests (caller-context capture, size
//! checks); the handlers below see captured buffers at PASSIVE.
//!
//! Everything new is traced with an `avrcp:` prefix (DebugView, "Capture
//! Kernel"): circuit build, method 6, event enable / disable, every property
//! GET / SET with its value, every Windows volume / mute write. The process
//! id in those lines is the context the callback runs in
//! (`PsGetCurrentProcessId`); ACX runs the event enable in the caller's
//! context, the property and method callbacks possibly not.
//!
//! # How ACX is called from Rust
//! ACX is a KMDF class extension. Every DDI is an entry of the function table
//! `AcxFunctions`, which lives *in this image* (`acxstub.lib`, data only) and
//! is filled by the WDF loader before `DriverEntry` through `_Acx_BIND_INFO`
//! in section `.kmdfclassbind$b` (walked by `WdfDriverEntry.lib`). The C
//! headers wrap each entry in a `FORCEINLINE`; the [`acx!`] macro is the same
//! thing: index the table with the bindgen `_ACXFUNCENUM` constant, cast to
//! the bindgen `PFN_ACX*` type, pass `AcxDriverGlobals` first.
//! Types come from bindgen over `acx.h` (build.rs, with clang's layout
//! asserts); the `*_INIT` helpers, GUIDs and the KS wave-format struct are
//! hand-written here and checked against cl.exe (`tools/acxcheck`).
//!
//! # Kill switch
//! `DisableAcx` (REG_DWORD, non-zero) is read in two places:
//! * `HKLM\SYSTEM\CurrentControlSet\Services\crossc_bth\Parameters` (whole
//!   driver): no ACX call is made at all, not even `AcxDriverInitialize`. The
//!   code that runs is the pre-ACX driver's. This is the recovery switch.
//! * the device's hardware key (`...\Enum\<instance>\Device Parameters`):
//!   every device-level ACX call is skipped for that device, but
//!   `AcxDriverInitialize` has already run in `DriverEntry`. An
//!   ACX-initialised driver with a non-ACX device is a combination no sample
//!   exercises (untested).
//!
//! Neither can skip the class-extension *binding* done by the loader before
//! `DriverEntry`, and neither undoes the INF changes.
//!
//! `DisableAvrcpVolume` (REG_DWORD, non-zero), in the same two keys, turns
//! off only the unified volume: no category list, property, method, event,
//! volume or mute element is registered, and the endpoint is exactly the
//! v0.1.1 one (Windows applies software volume). The device key is read when
//! the device is added, so a change needs a device restart (disable/enable,
//! or reconnecting the buds is not enough).
//!
//! # Failure policy
//! * `AcxDriverInitialize` fails -> ACX off for the driver, pipe unaffected.
//! * `AcxDeviceInitInitialize` / `AcxDeviceInitialize` fail -> DeviceAdd fails
//!   (the WDFDEVICE_INIT has already been modified by ACX).
//! * Anything circuit-level fails -> logged, no endpoint, pipe unaffected.
//! * The unified volume costs the endpoint nothing: with it on, any failure
//!   building or adding the circuit (the circuit init, `AcxCircuitCreate`,
//!   a missing volume element or AVRCP event, the pins, and
//!   `AcxDeviceAddCircuit`, where ACX checks the category list and the
//!   elements' default connections and registers the interfaces) deletes the
//!   half-built circuit (`WdfObjectDelete` of a circuit that was never
//!   added, as after `AcxDeviceDetachCircuit`) and builds the v0.1.1 circuit
//!   once instead; the feature then stays off for the device until it is
//!   added again. Only a missing mute element is tolerated (Windows mutes in
//!   software).

use core::mem::{offset_of, size_of, zeroed};
use core::ptr::{addr_of_mut, null_mut};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crossc_ioctl::audio::PcmFormat;

use wdk_sys::ntddk::{
    ExAllocatePool2, ExFreePool, IoAllocateMdl, IoFreeMdl, KeFlushQueuedDpcs, KeQueryPerformanceCounter,
    MmBuildMdlForNonPagedPool,
};
use wdk_sys::*;

use crate::audio_core::ring_bytes;
use crate::avrcp_vol::{self, CHANNELS, LEVEL_MAX, LEVEL_MIN, LEVEL_STEP};
use crate::rt_limits::{self, StreamSlots};
use crate::{CtxInfo, DeviceContext, attributes, audio, nt_success, trace};
use sys::*;

/// bindgen output for `acx.h` (ACX 1.1). Referenced WDF/NT types resolve to
/// the wdk-sys definitions.
#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    unused_imports,
    missing_docs,
    unnecessary_transmutes,
    clippy::all
)]
pub mod sys {
    use wdk_sys::*;

    include!(concat!(env!("OUT_DIR"), "/acx.rs"));
}

/// `acx.h`: `__declspec(selectany) ULONG AcxMinimumVersionRequired =
/// ACX_MINIMUM_VERSION_REQUIRED;` - the client must define it; `acxstub.lib`'s
/// `_Acx_BIND_INFO` points at it. Equals the ACX minor we build against
/// (build.rs `ACX_VERSION`), so the driver does not load on an older ACX and
/// plain `size_of` is the right `ACX_STRUCTURE_SIZE`.
#[unsafe(no_mangle)]
pub static mut AcxMinimumVersionRequired: ULONG = 1;

/// Calls an ACX DDI through the in-image function table, exactly like the
/// `FORCEINLINE` wrappers in the ACX headers.
///
/// SAFETY (caller): [`ACX_READY`] is true (the table was bound and
/// `AcxDriverInitialize` succeeded), or the call is `AcxDriverInitialize`
/// itself after [`table_bound`].
macro_rules! acx {
    ($index:ident, $pfn:ident $(, $arg:expr)* $(,)?) => {{
        let entry: ACXFUNC = (&raw const AcxFunctions).cast::<ACXFUNC>().add(_ACXFUNCENUM::$index as usize).read();
        let f = core::mem::transmute::<ACXFUNC, $pfn>(entry);
        (f.unwrap_unchecked())(AcxDriverGlobals $(, $arg)*)
    }};
}

/// Set once by [`driver_init`]; never cleared.
static ACX_READY: AtomicBool = AtomicBool::new(false);

const POOL_TAG: ULONG = u32::from_le_bytes(*b"xAcC");
const HNS_PER_SEC: u64 = 10_000_000;
/// ACX asks for at most two RT packets (ping-pong); same limit as the sample.
const MAX_PACKETS: usize = 2;
/// Late timer passes processed back to back before the stream timeline is
/// shifted instead (the sample recurses without a bound).
const MAX_CATCH_UP: u32 = 4;
/// Packets of one timer pass that can hold audio: the one that just
/// finished, and the next if the pass is a whole packet late. The audio
/// engine writes packet N+1 only after it was told that packet N-1
/// completed, so a third packet in the same pass cannot have been written.
const FRESH_PACKETS_PER_PASS: u32 = 2;

// ---------------------------------------------------------------------------
// Constants bindgen cannot produce (DEFINE_GUID / macros). tools/acxcheck.
// ---------------------------------------------------------------------------

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
    GUID { Data1: d1, Data2: d2, Data3: d3, Data4: d4 }
}

/// ksmedia.h `KSCATEGORY_AUDIO` {6994AD04-93EF-11D0-A3CC-00A0C9223196}
static KSCATEGORY_AUDIO: GUID = guid(0x6994_AD04, 0x93EF, 0x11D0, [0xA3, 0xCC, 0x00, 0xA0, 0xC9, 0x22, 0x31, 0x96]);
/// ksmedia.h `KSNODETYPE_HEADPHONES` {DFF21CE2-F70F-11D0-B917-00A0C9223196}
static KSNODETYPE_HEADPHONES: GUID =
    guid(0xDFF2_1CE2, 0xF70F, 0x11D0, [0xB9, 0x17, 0x00, 0xA0, 0xC9, 0x22, 0x31, 0x96]);
/// ksmedia.h `KSDATAFORMAT_TYPE_AUDIO` {73647561-0000-0010-8000-00AA00389B71}
const KSDATAFORMAT_TYPE_AUDIO: GUID = guid(0x7364_7561, 0x0000, 0x0010, [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71]);
/// ksmedia.h `KSDATAFORMAT_SUBTYPE_PCM` {00000001-0000-0010-8000-00AA00389B71}
const KSDATAFORMAT_SUBTYPE_PCM: GUID =
    guid(0x0000_0001, 0x0000, 0x0010, [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71]);
/// ksmedia.h `KSDATAFORMAT_SPECIFIER_WAVEFORMATEX` {05589F81-C356-11CE-BF01-00AA0055595A}
const KSDATAFORMAT_SPECIFIER_WAVEFORMATEX: GUID =
    guid(0x0558_9F81, 0xC356, 0x11CE, [0xBF, 0x01, 0x00, 0xAA, 0x00, 0x55, 0x59, 0x5A]);
/// ksmedia.h `AUDIO_SIGNALPROCESSINGMODE_DEFAULT` {C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}
static AUDIO_SIGNALPROCESSINGMODE_DEFAULT: GUID =
    guid(0xC18E_2F7E, 0x933D, 0x4965, [0xB7, 0xD1, 0x1E, 0xEF, 0x22, 0x8D, 0x2A, 0xF3]);
/// ksmedia.h `AUDIO_SIGNALPROCESSINGMODE_RAW` {9E90EA20-B493-4FD1-A1A8-7E1361A956CF}
static AUDIO_SIGNALPROCESSINGMODE_RAW: GUID =
    guid(0x9E90_EA20, 0xB493, 0x4FD1, [0xA1, 0xA8, 0x7E, 0x13, 0x61, 0xA9, 0x56, 0xCF]);
/// Vendor component id of the render circuit {871EEB92-ABB3-43B1-9E2F-4C2206CC0CCF}.
static RENDER_COMPONENT_ID: GUID = guid(0x871E_EB92, 0xABB3, 0x43B1, [0x9E, 0x2F, 0x4C, 0x22, 0x06, 0xCC, 0x0C, 0xCF]);

// ---- unified volume (see the module docs) ---------------------------------
// The private GUIDs were read from BthA2dp.sys 10.0.26100.9750 with its
// public PDB (`db 140073eb0`, `db 140073f20`, `db 1400740a0`) and are the
// ones BthAvrcp.dll / BthAvctpSvc.dll reference; ACX's category list from
// Acx01000.sys (`db 14002e8e0 L60`).

/// BthA2dp "VolumeLevelCache" property / "VolumeCacheWritten" event set
/// {401E566B-B47E-4E34-B0FC-94826E9E2A1A}.
static AVRCP_VOLUME_SET: GUID = guid(0x401E_566B, 0xB47E, 0x4E34, [0xB0, 0xFC, 0x94, 0x82, 0x6E, 0x9E, 0x2A, 0x1A]);
/// The one property / event id in that set.
const AVRCP_VOLUME_ID: ULONG = 1;
/// BthA2dp controller method set {0E62540F-BAC4-4B85-B344-0D82ED5008AC}.
static A2DP_SERVICE_METHOD_SET: GUID =
    guid(0x0E62_540F, 0xBAC4, 0x4B85, [0xB3, 0x44, 0x0D, 0x82, 0xED, 0x50, 0x08, 0xAC]);
/// `AVFilter::MethodServiceReady`, the only id BthAvctpSvc sends to a sink
/// node (ids 0-5 and 7 are used by the PC-as-sink path only).
const A2DP_METHOD_SERVICE_READY: ULONG = 6;
/// Interface class of BthA2dp's controller filter {E630310C-1733-42BB-9847-7E45D1A27490}.
const KSCATEGORY_AVRCP_VOLUME: GUID = guid(0xE630_310C, 0x1733, 0x42BB, [0x98, 0x47, 0x7E, 0x45, 0xD1, 0xA2, 0x74, 0x90]);
/// First entry ACX requires in a category list {2C6BB644-E1AE-47F8-9A2B-1D1FA750F2FA}.
const ACX_CIRCUIT_CATEGORY: GUID = guid(0x2C6B_B644, 0xE1AE, 0x47F8, [0x9A, 0x2B, 0x1D, 0x1F, 0xA7, 0x50, 0xF2, 0xFA]);
/// ks.h `KSCATEGORY_RENDER` {65E8773E-8F56-11D0-A3B9-00A0C9223196}
const KSCATEGORY_RENDER: GUID = guid(0x65E8_773E, 0x8F56, 0x11D0, [0xA3, 0xB9, 0x00, 0xA0, 0xC9, 0x22, 0x31, 0x96]);
/// ks.h `KSCATEGORY_CAPTURE` {65E8773D-8F56-11D0-A3B9-00A0C9223196}
const KSCATEGORY_CAPTURE: GUID = guid(0x65E8_773D, 0x8F56, 0x11D0, [0xA3, 0xB9, 0x00, 0xA0, 0xC9, 0x22, 0x31, 0x96]);
/// ksmedia.h `KSCATEGORY_REALTIME` {EB115FFC-10C8-4964-831D-6DCB02E6F23F}
const KSCATEGORY_REALTIME: GUID = guid(0xEB11_5FFC, 0x10C8, 0x4964, [0x83, 0x1D, 0x6D, 0xCB, 0x02, 0xE6, 0xF2, 0x3F]);
/// ksmedia.h `KSCATEGORY_TOPOLOGY` {DDA54A40-1E4C-11D1-A050-405705C10000}
const KSCATEGORY_TOPOLOGY: GUID = guid(0xDDA5_4A40, 0x1E4C, 0x11D1, [0xA0, 0x50, 0x40, 0x57, 0x05, 0xC1, 0x00, 0x00]);
/// ksmedia.h `KSAUDFNAME_VOLUME_CONTROL` {185FEDF7-9905-11D1-95A9-00C04FB925D3}
static KSAUDFNAME_VOLUME_CONTROL: GUID =
    guid(0x185F_EDF7, 0x9905, 0x11D1, [0x95, 0xA9, 0x00, 0xC0, 0x4F, 0xB9, 0x25, 0xD3]);
/// ksmedia.h `KSAUDFNAME_WAVE_MUTE` {185FEDE6-9905-11D1-95A9-00C04FB925D3}
static KSAUDFNAME_WAVE_MUTE: GUID = guid(0x185F_EDE6, 0x9905, 0x11D1, [0x95, 0xA9, 0x00, 0xC0, 0x4F, 0xB9, 0x25, 0xD3]);
/// ACX's default list for a render circuit with a sink pin, in its order
/// (`Acx01000!AfxCircuit::FinishCategoryInitializing`, `14002e8e0`), plus
/// the AVRCP volume class. ACX copies the array.
static RENDER_CATEGORIES: [GUID; 7] = [
    ACX_CIRCUIT_CATEGORY,
    guid(0x6994_AD04, 0x93EF, 0x11D0, [0xA3, 0xCC, 0x00, 0xA0, 0xC9, 0x22, 0x31, 0x96]), // KSCATEGORY_AUDIO
    KSCATEGORY_RENDER,
    KSCATEGORY_CAPTURE,
    KSCATEGORY_REALTIME,
    KSCATEGORY_TOPOLOGY,
    KSCATEGORY_AVRCP_VOLUME,
];
/// acxrequest.h item flags (KSPROPERTY_TYPE_GET / _SET, KSMETHOD_TYPE_SEND).
const PROPERTY_FLAG_GET: ULONG = 0x1;
const PROPERTY_FLAG_SET: ULONG = 0x2;
const METHOD_FLAG_SEND: ULONG = 0x1;
/// VARENUM `VT_I4`: what ACX reports in a BASICSUPPORT answer.
const VT_I4: ULONG = 3;
/// acxelements.h `AcxElementIdDefault`.
const ACX_ELEMENT_ID_DEFAULT: ULONG = u32::MAX;

/// Item tables handed to ACX. Writable statics like the samples' (ACX gets
/// non-const pointers), never written by this driver.
#[repr(transparent)]
struct AcxItems<T>(core::cell::UnsafeCell<T>);
// SAFETY: written by nobody; ACX only reads them.
unsafe impl<T> Sync for AcxItems<T> {}

/// Property `{401e566b}` id 1: GET and SET of one LONG; the KSPROPERTY
/// alone is the control (BthA2dp: MinProperty 0x18, MinData 4).
static AVRCP_PROPERTIES: AcxItems<[ACX_PROPERTY_ITEM; 1]> = AcxItems(core::cell::UnsafeCell::new([ACX_PROPERTY_ITEM {
    Set: &raw const AVRCP_VOLUME_SET,
    Id: AVRCP_VOLUME_ID,
    Flags: PROPERTY_FLAG_GET | PROPERTY_FLAG_SET,
    EvtAcxObjectProcessRequest: Some(evt_avrcp_property),
    Reserved: null_mut(),
    ControlCb: 0,
    ValueCb: size_of::<LONG>() as ULONG,
    ValueType: VT_I4,
}]));

/// Method `{0e62540f}` id 6: no arguments, no result (BthA2dp: MinMethod
/// 0x18, MinData 0; BthAvctpSvc sends KSMETHOD flags 1 = SEND).
static AVRCP_METHODS: AcxItems<[ACX_METHOD_ITEM; 1]> = AcxItems(core::cell::UnsafeCell::new([ACX_METHOD_ITEM {
    Set: &raw const A2DP_SERVICE_METHOD_SET,
    Id: A2DP_METHOD_SERVICE_READY,
    Flags: METHOD_FLAG_SEND,
    EvtAcxObjectProcessRequest: Some(evt_avrcp_method),
    Reserved: null_mut(),
    ArgsCb: 0,
    ResultCb: 0,
    ResultType: 0,
}]));

const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SPEAKER_FRONT_LEFT: u32 = 0x1;
const SPEAKER_FRONT_RIGHT: u32 = 0x2;
const KSAUDIO_SPEAKER_STEREO: u32 = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT;
/// acxpin.h `AcxPinIdDefault` / `AcxJackPropertiesIdDefault`.
const ACX_ID_DEFAULT: ULONG = u32::MAX;
/// acxpin.h `AcxInstanceIndeterminate`.
const ACX_INSTANCE_INDETERMINATE: ULONG = u32::MAX;

const fn utf16<const N: usize>(s: &str) -> [u16; N] {
    let b = s.as_bytes();
    assert!(b.len() == N);
    let mut out = [0u16; N];
    let mut i = 0;
    while i < N {
        assert!(b[i] < 0x80);
        out[i] = b[i] as u16;
        i += 1;
    }
    out
}

/// Circuit name. MUST equal the `AddInterface` reference string
/// (`KSNAME_Headphones`) in crossc_bth.inx.
static CIRCUIT_NAME: [u16; 11] = utf16("Headphones0");
/// Static like the sample's `DECLARE_CONST_UNICODE_STRING`, so it does not
/// matter whether ACX copies the string or keeps the pointer.
#[repr(transparent)]
struct StaticUnicode(UNICODE_STRING);
// SAFETY: immutable, points at an immutable static.
unsafe impl Sync for StaticUnicode {}
static CIRCUIT_NAME_US: StaticUnicode = StaticUnicode(UNICODE_STRING {
    Length: (CIRCUIT_NAME.len() * 2) as u16,
    MaximumLength: (CIRCUIT_NAME.len() * 2) as u16,
    Buffer: CIRCUIT_NAME.as_ptr().cast_mut(),
});
static DISABLE_ACX_VALUE: [u16; 10] = utf16("DisableAcx");
static DISABLE_AVRCP_VOLUME_VALUE: [u16; 18] = utf16("DisableAvrcpVolume");

fn unicode(s: &'static [u16]) -> UNICODE_STRING {
    UNICODE_STRING {
        Length: (s.len() * 2) as u16,
        MaximumLength: (s.len() * 2) as u16,
        Buffer: s.as_ptr().cast_mut(),
    }
}

/// ksmedia.h `KSDATAFORMAT_WAVEFORMATEXTENSIBLE`, flattened: `KSDATAFORMAT`
/// (64 bytes) followed by `WAVEFORMATEXTENSIBLE` (40 bytes, 1-packed in C;
/// every field sits on its natural alignment at these offsets, so plain
/// `repr(C)` gives the identical layout).
#[repr(C)]
struct KsWaveFormatExtensible {
    // KSDATAFORMAT
    format_size: u32,
    flags: u32,
    sample_size: u32,
    reserved: u32,
    major_format: GUID,
    sub_format: GUID,
    specifier: GUID,
    // WAVEFORMATEX
    format_tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
    cb_size: u16,
    // WAVEFORMATEXTENSIBLE
    valid_bits_per_sample: u16,
    channel_mask: u32,
    wave_sub_format: GUID,
}

/// Stereo PCM, same initialiser as the sample's `Inc/AudioFormats.h`.
const fn pcm_stereo(rate: u32, container_bits: u16, valid_bits: u16) -> KsWaveFormatExtensible {
    let block_align = 2 * container_bits / 8;
    KsWaveFormatExtensible {
        format_size: size_of::<KsWaveFormatExtensible>() as u32,
        flags: 0,
        sample_size: 0,
        reserved: 0,
        major_format: KSDATAFORMAT_TYPE_AUDIO,
        sub_format: KSDATAFORMAT_SUBTYPE_PCM,
        specifier: KSDATAFORMAT_SPECIFIER_WAVEFORMATEX,
        format_tag: WAVE_FORMAT_EXTENSIBLE,
        channels: 2,
        samples_per_sec: rate,
        avg_bytes_per_sec: rate * block_align as u32,
        block_align,
        bits_per_sample: container_bits,
        cb_size: 22, // sizeof(WAVEFORMATEXTENSIBLE) - sizeof(WAVEFORMATEX)
        valid_bits_per_sample: valid_bits,
        channel_mask: KSAUDIO_SPEAKER_STEREO,
        wave_sub_format: KSDATAFORMAT_SUBTYPE_PCM,
    }
}

/// Give the host pin a DEFAULT signal processing mode list next to its RAW
/// one (same [`FORMATS`], same default). Off: the pin offers RAW only,
/// exactly as 1.0.0 did, the only layout run on a device so far.
///
/// Why it exists: the hypothesis that the audio engine's DEFAULT-mode
/// streams would then be described by the pin itself, and that this could
/// let Windows play at 96 kHz. Nothing shows that yet: both lists default to
/// 48 kHz, and the engine takes a mode's format from its list's default
/// (KSPROPERTY_PIN_PROPOSEDATAFORMAT2).
///
/// Why it is off: the host pin allows any number of streams, and a pin with
/// streams in several modes is a multi-stream pin to the engine: it can run
/// a DEFAULT and a RAW stream at the same time. The driver does not mix
/// them; the tap follows the last stream to run (audio_core.rs), so the
/// other stream's app would be silent on the buds. With RAW only, every
/// shared-mode app is mixed into one stream by the engine.
///
/// To try it on a device, set this to true and look for two "stream
/// created" trace lines with different modes, both followed by "stream
/// running": that is the case above. Also unchecked on a device: whether
/// ACX takes a list owned by the circuit and assigned after the pin was
/// created.
const DEFAULT_MODE_LIST: bool = false;

/// Formats offered on the host pin, in its RAW mode list (and in its
/// DEFAULT one, see [`DEFAULT_MODE_LIST`]). Index 0 is the default.
/// 24-bit is offered as 24 valid bits in a 32-bit container, the layout the
/// sample (`Pcm48000c2_24in32`) and HD Audio use; packed 3-byte samples are
/// not offered.
static FORMATS: [KsWaveFormatExtensible; 3] = [
    pcm_stereo(48_000, 32, 24), // HiFi
    pcm_stereo(96_000, 32, 24), // UHQ
    pcm_stereo(48_000, 16, 16), // fallback
];

// ---- layout / constants, mirrored from tools/acxcheck/msvc-layout.txt ------
const _: () = {
    assert!(_ACXFUNCENUM::AcxFunctionTableNumEntries == 221);
    assert!(_ACXFUNCENUM::AcxCircuitInitAllocateTableIndex == 0);
    assert!(_ACXFUNCENUM::AcxCircuitInitFreeTableIndex == 1);
    assert!(_ACXFUNCENUM::AcxCircuitInitSetComponentIdTableIndex == 2);
    assert!(_ACXFUNCENUM::AcxCircuitInitAssignNameTableIndex == 4);
    assert!(_ACXFUNCENUM::AcxCircuitInitSetCircuitTypeTableIndex == 6);
    assert!(_ACXFUNCENUM::AcxCircuitInitSetAcxCircuitPnpPowerCallbacksTableIndex == 9);
    assert!(_ACXFUNCENUM::AcxCircuitInitAssignAcxCreateStreamCallbackTableIndex == 12);
    assert!(_ACXFUNCENUM::AcxCircuitCreateTableIndex == 14);
    assert!(_ACXFUNCENUM::AcxCircuitAddPinsTableIndex == 20);
    assert!(_ACXFUNCENUM::AcxDataFormatCreateTableIndex == 39);
    assert!(_ACXFUNCENUM::AcxDataFormatGetChannelsCountTableIndex == 43);
    assert!(_ACXFUNCENUM::AcxDataFormatGetSampleRateTableIndex == 50);
    assert!(_ACXFUNCENUM::AcxDataFormatGetBitsPerSampleTableIndex == 46);
    assert!(_ACXFUNCENUM::AcxDataFormatGetBlockAlignTableIndex == 49);
    assert!(_ACXFUNCENUM::AcxDataFormatGetAverageBytesPerSecTableIndex == 51);
    assert!(_ACXFUNCENUM::AcxDataFormatListAddDataFormatTableIndex == 61);
    assert!(_ACXFUNCENUM::AcxDataFormatListAssignDefaultDataFormatTableIndex == 63);
    assert!(_ACXFUNCENUM::AcxDeviceInitInitializeTableIndex == 68);
    assert!(_ACXFUNCENUM::AcxDeviceInitializeTableIndex == 69);
    assert!(_ACXFUNCENUM::AcxDeviceAddCircuitTableIndex == 70);
    assert!(_ACXFUNCENUM::AcxDeviceRemoveCircuitTableIndex == 71);
    assert!(_ACXFUNCENUM::AcxDriverInitializeTableIndex == 78);
    assert!(_ACXFUNCENUM::AcxPinCreateTableIndex == 137);
    assert!(_ACXFUNCENUM::AcxPinAddJacksTableIndex == 139);
    assert!(_ACXFUNCENUM::AcxPinGetRawDataFormatListTableIndex == 142);
    assert!(_ACXFUNCENUM::AcxJackCreateTableIndex == 149);
    assert!(_ACXFUNCENUM::AcxJackChangeStateNotificationTableIndex == 150);
    assert!(_ACXFUNCENUM::AcxDataFormatGetValidBitsPerSampleTableIndex == 47);
    assert!(_ACXFUNCENUM::AcxStreamInitAssignAcxStreamCallbacksTableIndex == 157);
    assert!(_ACXFUNCENUM::AcxStreamInitAssignAcxRtStreamCallbacksTableIndex == 158);
    assert!(_ACXFUNCENUM::AcxStreamInitSetAcxRtStreamSupportsNotificationsTableIndex == 159);
    assert!(_ACXFUNCENUM::AcxRtStreamCreateTableIndex == 169);
    assert!(_ACXFUNCENUM::AcxRtStreamNotifyPacketCompleteTableIndex == 170);

    assert!(size_of::<ACXFUNC>() == 8);
    assert!(size_of::<ACX_DRIVER_CONFIG>() == 8);
    assert!(size_of::<ACX_DEVICEINIT_CONFIG>() == 48);
    assert!(offset_of!(ACX_DEVICEINIT_CONFIG, SynchronizationScope) == 8);
    assert!(offset_of!(ACX_DEVICEINIT_CONFIG, ExecutionLevel) == 12);
    assert!(size_of::<ACX_DEVICE_CONFIG>() == 40);
    assert!(size_of::<ACX_CIRCUIT_PNPPOWER_CALLBACKS>() == 40);
    assert!(offset_of!(ACX_CIRCUIT_PNPPOWER_CALLBACKS, EvtAcxCircuitPowerUp) == 24);
    assert!(offset_of!(ACX_CIRCUIT_PNPPOWER_CALLBACKS, EvtAcxCircuitPowerDown) == 32);
    assert!(size_of::<ACX_PIN_CALLBACKS>() == 64);
    assert!(offset_of!(ACX_PIN_CALLBACKS, EvtAcxPinSetDataFormat) == 8);
    assert!(size_of::<ACX_PIN_CONFIG>() == 88);
    assert!(offset_of!(ACX_PIN_CONFIG, Flags) == 4);
    assert!(offset_of!(ACX_PIN_CONFIG, Id) == 8);
    assert!(offset_of!(ACX_PIN_CONFIG, Type) == 12);
    assert!(offset_of!(ACX_PIN_CONFIG, Communication) == 16);
    assert!(offset_of!(ACX_PIN_CONFIG, Category) == 24);
    assert!(offset_of!(ACX_PIN_CONFIG, PinCallbacks) == 40);
    assert!(offset_of!(ACX_PIN_CONFIG, MaxStreams) == 80);
    assert!(size_of::<ACX_JACK_CONFIG>() == 48);
    assert!(offset_of!(ACX_JACK_CONFIG, Id) == 4);
    assert!(offset_of!(ACX_JACK_CONFIG, Flags) == 8);
    assert!(offset_of!(ACX_JACK_CONFIG, Callbacks) == 16);
    assert!(offset_of!(ACX_JACK_CONFIG, Description) == 24);
    assert!(size_of::<ACX_JACK_CALLBACKS>() == 16);
    assert!(offset_of!(ACX_JACK_CALLBACKS, EvtAcxJackRetrievePresenceState) == 8);
    assert!(_ACX_JACK_CONFIG_FLAGS::AcxJackConfigJackDetection == 1);
    assert!(size_of::<ACX_JACK_DESCRIPTION>() == 24);
    assert!(offset_of!(ACX_JACK_DESCRIPTION, ConnectionType) == 8);
    assert!(offset_of!(ACX_JACK_DESCRIPTION, PortConnection) == 20);
    assert!(size_of::<ACX_DATAFORMAT_CONFIG>() == 24);
    assert!(offset_of!(ACX_DATAFORMAT_CONFIG, Type) == 8);
    assert!(offset_of!(ACX_DATAFORMAT_CONFIG, u) == 16);
    assert!(size_of::<ACX_STREAM_CALLBACKS>() == 48);
    assert!(offset_of!(ACX_STREAM_CALLBACKS, EvtAcxStreamPrepareHardware) == 8);
    assert!(offset_of!(ACX_STREAM_CALLBACKS, EvtAcxStreamReleaseHardware) == 16);
    assert!(offset_of!(ACX_STREAM_CALLBACKS, EvtAcxStreamRun) == 24);
    assert!(offset_of!(ACX_STREAM_CALLBACKS, EvtAcxStreamPause) == 32);
    assert!(size_of::<ACX_RT_STREAM_CALLBACKS>() == 64);
    assert!(offset_of!(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamGetHwLatency) == 8);
    assert!(offset_of!(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamAllocateRtPackets) == 16);
    assert!(offset_of!(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamFreeRtPackets) == 24);
    assert!(offset_of!(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamSetRenderPacket) == 32);
    assert!(offset_of!(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamGetCurrentPacket) == 40);
    assert!(offset_of!(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamGetCapturePacket) == 48);
    assert!(offset_of!(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamGetPresentationPosition) == 56);
    assert!(size_of::<ACX_RTPACKET>() == 40);
    assert!(offset_of!(ACX_RTPACKET, RtPacketBuffer) == 8);
    assert!(offset_of!(ACX_RTPACKET, RtPacketOffset) == 32);
    assert!(offset_of!(ACX_RTPACKET, RtPacketSize) == 36);
    assert!(size_of::<WDF_MEMORY_DESCRIPTOR>() == 24);
    assert!(offset_of!(WDF_MEMORY_DESCRIPTOR, u) == 8);
    assert!(size_of::<WDF_TIMER_CONFIG>() == 40);
    assert!(offset_of!(WDF_TIMER_CONFIG, UseHighResolutionTimer) == 32);
    assert!(size_of::<WDF_WORKITEM_CONFIG>() == 24);
    assert!(offset_of!(WDF_WORKITEM_CONFIG, EvtWorkItemFunc) == 8);
    assert!(offset_of!(WDF_WORKITEM_CONFIG, AutomaticSerialization) == 16);
    // Used by audio.rs / lib.rs for the audio path.
    assert!(size_of::<WDF_IO_QUEUE_CONFIG>() == 96);
    assert!(offset_of!(WDF_IO_QUEUE_CONFIG, DispatchType) == 4);
    assert!(offset_of!(WDF_IO_QUEUE_CONFIG, PowerManaged) == 8);
    assert!(offset_of!(WDF_IO_QUEUE_CONFIG, DefaultQueue) == 13);
    assert!(offset_of!(WDF_IO_QUEUE_CONFIG, EvtIoCanceledOnQueue) == 72);
    assert!(_WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchManual == 3);
    assert!(_WDF_TRI_STATE::WdfFalse == 0);
    assert!(_WDF_EXECUTION_LEVEL::WdfExecutionLevelDispatch == 3);
    assert!(size_of::<WDF_DEVICE_PROPERTY_DATA>() == 24);
    assert!(offset_of!(WDF_DEVICE_PROPERTY_DATA, PropertyKey) == 8);
    assert!(offset_of!(WDF_DEVICE_PROPERTY_DATA, Lcid) == 16);
    assert!(offset_of!(WDF_DEVICE_PROPERTY_DATA, Flags) == 20);
    assert!(size_of::<DEVPROPKEY>() == 20);
    assert!(offset_of!(DEVPROPKEY, pid) == 16);
    assert!(DEVPROP_TYPE_STRING == 0x12);
    assert!(PLUGPLAY_PROPERTY_PERSISTENT == 1);
    assert!(_MODE::KernelMode == 0);
    assert!(STATUS_DEVICE_BUSY as u32 == 0x8000_0011);
    assert!(STATUS_ACCESS_DENIED as u32 == 0xC000_0022);
    assert!(STATUS_NOT_SUPPORTED as u32 == 0xC000_00BB);
    assert!(STATUS_BUFFER_TOO_SMALL as u32 == 0xC000_0023);
    assert!(STATUS_CANCELLED as u32 == 0xC000_0120);
    assert!(STATUS_NO_MORE_ENTRIES as u32 == 0x8000_001A);
    // DEVPKEY_Device_FriendlyName (bth.rs)
    assert!(crate::bth::DEVPKEY_DEVICE_FRIENDLY_NAME.fmtid.Data1 == 0xA45C_254E);
    assert!(crate::bth::DEVPKEY_DEVICE_FRIENDLY_NAME.fmtid.Data4[7] == 0xE0);
    assert!(crate::bth::DEVPKEY_DEVICE_FRIENDLY_NAME.pid == 14);

    assert!(_ACX_CIRCUIT_TYPE::AcxCircuitTypeRender == 1);
    assert!(_ACX_PIN_TYPE::AcxPinTypeSink == 1);
    assert!(_ACX_PIN_TYPE::AcxPinTypeSource == 2);
    assert!(_ACX_PIN_COMMUNICATION::AcxPinCommunicationNone == 0);
    assert!(_ACX_PIN_COMMUNICATION::AcxPinCommunicationSink == 1);
    assert!(_ACX_DATAFORMAT_TYPE::AcxDataFormatKsFormat == 0);
    assert!(_ACX_JACK_CONNECTION_TYPE::AcxConnTypeOtherDigital == 6);
    assert!(_ACX_JACK_GEO_LOCATION::AcxGeoLocNotApplicable == 14);
    assert!(_ACX_JACK_GEN_LOCATION::AcxGenLocSeparate == 2);
    assert!(_ACX_JACK_PORT_CONNECTION::AcxPortConnJack == 0);
    assert!(_ACX_STREAM_STATE::AcxStreamStateStop == 0);
    assert!(_ACX_STREAM_STATE::AcxStreamStatePause == 2);
    assert!(_ACX_STREAM_STATE::AcxStreamStateRun == 3);
    assert!(_WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeNone == 4);
    assert!(_WDF_EXECUTION_LEVEL::WdfExecutionLevelPassive == 2);
    assert!(_WDF_MEMORY_DESCRIPTOR_TYPE::WdfMemoryDescriptorTypeMdl == 2);

    // KSDATAFORMAT_WAVEFORMATEXTENSIBLE
    assert!(size_of::<KsWaveFormatExtensible>() == 104);
    assert!(offset_of!(KsWaveFormatExtensible, major_format) == 16);
    assert!(offset_of!(KsWaveFormatExtensible, sub_format) == 32);
    assert!(offset_of!(KsWaveFormatExtensible, specifier) == 48);
    assert!(offset_of!(KsWaveFormatExtensible, format_tag) == 64);
    assert!(offset_of!(KsWaveFormatExtensible, channels) == 66);
    assert!(offset_of!(KsWaveFormatExtensible, samples_per_sec) == 68);
    assert!(offset_of!(KsWaveFormatExtensible, avg_bytes_per_sec) == 72);
    assert!(offset_of!(KsWaveFormatExtensible, block_align) == 76);
    assert!(offset_of!(KsWaveFormatExtensible, bits_per_sample) == 78);
    assert!(offset_of!(KsWaveFormatExtensible, cb_size) == 80);
    assert!(offset_of!(KsWaveFormatExtensible, valid_bits_per_sample) == 82);
    assert!(offset_of!(KsWaveFormatExtensible, channel_mask) == 84);
    assert!(offset_of!(KsWaveFormatExtensible, wave_sub_format) == 88);
    assert!(FORMATS[0].avg_bytes_per_sec == 384_000 && FORMATS[0].block_align == 8);
    assert!(FORMATS[1].avg_bytes_per_sec == 768_000 && FORMATS[1].block_align == 8);
    assert!(FORMATS[2].avg_bytes_per_sec == 192_000 && FORMATS[2].block_align == 4);
    // RT packet window (rt_limits.rs): for every offered format, the sizes
    // Windows is expected to ask for are accepted and the abusive ones are
    // not. In turn: 3 ms and 10 ms packets (two of them, event driven); one
    // page and 2 s rounded up to pages (one packet, timer driven); 5 s (the
    // longest exclusive-mode buffer WASAPI accepts); then one frame, and
    // what the old limit of 1 GiB let through.
    let mut i = 0;
    while i < FORMATS.len() {
        let bps = FORMATS[i].avg_bytes_per_sec;
        let frame = FORMATS[i].block_align as u32;
        let max = MAX_PACKETS as u32;
        assert!(rt_limits::packet_ok(2, max, bps / 1000 * 3, bps));
        assert!(rt_limits::packet_ok(2, max, bps / 100, bps));
        assert!(rt_limits::packet_ok(1, max, PAGE_SIZE, bps));
        assert!(rt_limits::packet_ok(1, max, (bps * 2).div_ceil(PAGE_SIZE) * PAGE_SIZE, bps));
        assert!(rt_limits::packet_ok(2, max, bps * 5, bps));
        assert!(!rt_limits::packet_ok(2, max, frame, bps));
        assert!(!rt_limits::packet_ok(2, max, bps * 5 + frame, bps));
        assert!(!rt_limits::packet_ok(2, max, 0x4000_0000, bps));
        assert!(!rt_limits::packet_ok(3, max, bps / 100, bps));
        assert!(!rt_limits::packet_ok(0, max, bps / 100, bps));
        i += 1;
    }
    assert!(PAGE_SIZE == 0x1000);
    assert!(POOL_FLAG_NON_PAGED == 0x40);
    assert!(PLUGPLAY_REGKEY_DEVICE == 1);
    assert!(KEY_READ == 0x20019);
    assert!(STATUS_DATA_LATE_ERROR as u32 == 0xC000_003D);
    assert!(STATUS_DATA_OVERRUN as u32 == 0xC000_003C);
    assert!(STATUS_INVALID_STATE_TRANSITION as u32 == 0xC000_A003);
    // AcxPinIdDefault / AcxJackPropertiesIdDefault / AcxInstanceIndeterminate
    assert!(ACX_ID_DEFAULT == 0xFFFF_FFFF && ACX_INSTANCE_INDETERMINATE == 0xFFFF_FFFF);
    assert!(WAVE_FORMAT_EXTENSIBLE == 0xFFFE && KSAUDIO_SPEAKER_STEREO == 3);
    assert!(FORMATS[0].cb_size == 22 && FORMATS[0].format_size == 104);

    // ---- DEFAULT mode list (msvc-layout.txt, last part) --------------------
    assert!(_ACXFUNCENUM::AcxDataFormatListCreateTableIndex == 60);
    assert!(_ACXFUNCENUM::AcxPinAssignModeDataFormatListTableIndex == 141);
    assert!(size_of::<ACX_DATAFORMAT_LIST_CONFIG>() == 8);
    assert!(offset_of!(ACX_DATAFORMAT_LIST_CONFIG, Flags) == 4);
    // AUDIO_SIGNALPROCESSINGMODE_DEFAULT / _RAW: compare their definitions
    // with the `guid AUDIO_SIGNALPROCESSINGMODE_*` lines of msvc-layout.txt.
    // The tap's session can know every stream the driver allows.
    assert!(rt_limits::MAX_STREAMS as usize <= crate::audio_core::MAX_TRACKED);

    // ---- unified volume (msvc-layout.txt, "unified volume" part) ----------
    assert!(_ACXFUNCENUM::AcxCircuitInitAssignCategoriesTableIndex == 5);
    assert!(_ACXFUNCENUM::AcxCircuitInitAssignPropertiesTableIndex == 7);
    assert!(_ACXFUNCENUM::AcxCircuitInitAssignMethodsTableIndex == 8);
    assert!(_ACXFUNCENUM::AcxCircuitAddEventsTableIndex == 15);
    assert!(_ACXFUNCENUM::AcxCircuitAddElementsTableIndex == 16);
    assert!(_ACXFUNCENUM::AcxMuteCreateTableIndex == 84);
    assert!(_ACXFUNCENUM::AcxMuteChangeStateNotificationTableIndex == 85);
    assert!(_ACXFUNCENUM::AcxVolumeCreateTableIndex == 86);
    assert!(_ACXFUNCENUM::AcxVolumeChangeLevelNotificationTableIndex == 87);
    assert!(_ACXFUNCENUM::AcxEventCreateTableIndex == 95);
    assert!(_ACXFUNCENUM::AcxEventGenerateEventTableIndex == 97);
    assert!(_ACXFUNCENUM::AcxRequestGetParametersTableIndex == 153);
    assert!(size_of::<ACX_PROPERTY_ITEM>() == 48);
    assert!(offset_of!(ACX_PROPERTY_ITEM, Id) == 8);
    assert!(offset_of!(ACX_PROPERTY_ITEM, Flags) == 12);
    assert!(offset_of!(ACX_PROPERTY_ITEM, EvtAcxObjectProcessRequest) == 16);
    assert!(offset_of!(ACX_PROPERTY_ITEM, Reserved) == 24);
    assert!(offset_of!(ACX_PROPERTY_ITEM, ControlCb) == 32);
    assert!(offset_of!(ACX_PROPERTY_ITEM, ValueCb) == 36);
    assert!(offset_of!(ACX_PROPERTY_ITEM, ValueType) == 40);
    assert!(size_of::<ACX_METHOD_ITEM>() == 48);
    assert!(offset_of!(ACX_METHOD_ITEM, Id) == 8);
    assert!(offset_of!(ACX_METHOD_ITEM, Flags) == 12);
    assert!(offset_of!(ACX_METHOD_ITEM, EvtAcxObjectProcessRequest) == 16);
    assert!(offset_of!(ACX_METHOD_ITEM, Reserved) == 24);
    assert!(offset_of!(ACX_METHOD_ITEM, ArgsCb) == 32);
    assert!(offset_of!(ACX_METHOD_ITEM, ResultCb) == 36);
    assert!(offset_of!(ACX_METHOD_ITEM, ResultType) == 40);
    assert!(size_of::<ACX_EVENT_CALLBACKS>() == 32);
    assert!(offset_of!(ACX_EVENT_CALLBACKS, EvtAcxEventEnable) == 8);
    assert!(offset_of!(ACX_EVENT_CALLBACKS, EvtAcxEventDisable) == 16);
    assert!(offset_of!(ACX_EVENT_CALLBACKS, EvtAcxEventProcessRequest) == 24);
    assert!(size_of::<ACX_EVENT_CONFIG>() == 32);
    assert!(offset_of!(ACX_EVENT_CONFIG, Flags) == 4);
    assert!(offset_of!(ACX_EVENT_CONFIG, Set) == 8);
    assert!(offset_of!(ACX_EVENT_CONFIG, Id) == 16);
    assert!(offset_of!(ACX_EVENT_CONFIG, Type) == 20);
    assert!(offset_of!(ACX_EVENT_CONFIG, Callbacks) == 24);
    assert!(size_of::<ACX_VOLUME_CALLBACKS>() == 40);
    assert!(offset_of!(ACX_VOLUME_CALLBACKS, EvtAcxVolumeAssignLevel) == 8);
    assert!(offset_of!(ACX_VOLUME_CALLBACKS, EvtAcxRampedVolumeAssignLevel) == 16);
    assert!(offset_of!(ACX_VOLUME_CALLBACKS, EvtAcxVolumeRetrieveLevel) == 24);
    assert!(offset_of!(ACX_VOLUME_CALLBACKS, EvtAcxVolumeProcessRequest) == 32);
    assert!(size_of::<ACX_VOLUME_CONFIG>() == 48);
    assert!(offset_of!(ACX_VOLUME_CONFIG, Id) == 4);
    assert!(offset_of!(ACX_VOLUME_CONFIG, Name) == 8);
    assert!(offset_of!(ACX_VOLUME_CONFIG, Flags) == 16);
    assert!(offset_of!(ACX_VOLUME_CONFIG, ChannelsCount) == 20);
    assert!(offset_of!(ACX_VOLUME_CONFIG, Maximum) == 24);
    assert!(offset_of!(ACX_VOLUME_CONFIG, Minimum) == 28);
    assert!(offset_of!(ACX_VOLUME_CONFIG, SteppingDelta) == 32);
    assert!(offset_of!(ACX_VOLUME_CONFIG, Callbacks) == 40);
    assert!(size_of::<ACX_MUTE_CALLBACKS>() == 32);
    assert!(offset_of!(ACX_MUTE_CALLBACKS, EvtAcxMuteAssignState) == 8);
    assert!(offset_of!(ACX_MUTE_CALLBACKS, EvtAcxMuteRetrieveState) == 16);
    assert!(offset_of!(ACX_MUTE_CALLBACKS, EvtAcxMuteProcessRequest) == 24);
    assert!(size_of::<ACX_MUTE_CONFIG>() == 32);
    assert!(offset_of!(ACX_MUTE_CONFIG, Id) == 4);
    assert!(offset_of!(ACX_MUTE_CONFIG, Name) == 8);
    assert!(offset_of!(ACX_MUTE_CONFIG, Flags) == 16);
    assert!(offset_of!(ACX_MUTE_CONFIG, ChannelsCount) == 20);
    assert!(offset_of!(ACX_MUTE_CONFIG, Callbacks) == 24);
    assert!(size_of::<ACX_REQUEST_PARAMETERS>() == 72);
    assert!(offset_of!(ACX_REQUEST_PARAMETERS, Type) == 4);
    assert!(offset_of!(ACX_REQUEST_PARAMETERS, Parameters) == 8);
    assert!(offset_of!(_ACX_REQUEST_PARAMETERS__bindgen_ty_1__bindgen_ty_1, Set) == 0);
    assert!(offset_of!(_ACX_REQUEST_PARAMETERS__bindgen_ty_1__bindgen_ty_1, Id) == 16);
    assert!(offset_of!(_ACX_REQUEST_PARAMETERS__bindgen_ty_1__bindgen_ty_1, Verb) == 20);
    assert!(offset_of!(_ACX_REQUEST_PARAMETERS__bindgen_ty_1__bindgen_ty_1, Value) == 48);
    assert!(offset_of!(_ACX_REQUEST_PARAMETERS__bindgen_ty_1__bindgen_ty_1, ValueCb) == 56);
    assert!(offset_of!(_ACX_REQUEST_PARAMETERS__bindgen_ty_1__bindgen_ty_2, Id) == 16);
    assert!(_ACX_REQUEST_TYPE::AcxRequestTypeProperty == 3);
    assert!(_ACX_REQUEST_TYPE::AcxRequestTypeMethod == 4);
    assert!(_ACX_PROPERTY_VERB::AcxPropertyVerbGet == 1);
    assert!(_ACX_PROPERTY_VERB::AcxPropertyVerbSet == 2);
    assert!(PROPERTY_FLAG_GET == ACX_PROPERTY_ITEM_FLAG_GET && PROPERTY_FLAG_GET == 0x1);
    assert!(PROPERTY_FLAG_SET == ACX_PROPERTY_ITEM_FLAG_SET && PROPERTY_FLAG_SET == 0x2);
    assert!(METHOD_FLAG_SEND == ACX_METHOD_ITEM_FLAG_SEND && METHOD_FLAG_SEND == 0x1);
    assert!(ACX_ELEMENT_ID_DEFAULT == 0xFFFF_FFFF);
    assert!(STATUS_INVALID_DEVICE_STATE as u32 == 0xC000_0184);
    assert!(STATUS_INVALID_DEVICE_REQUEST as u32 == 0xC000_0010);
    assert!(STATUS_INVALID_PARAMETER as u32 == 0xC000_000D);
    // GUIDs: ksmedia.h / ks.h ones as printed by acxcheck; the private ones
    // as dumped from BthA2dp.sys (module docs).
    assert!(KSCATEGORY_CAPTURE.Data1 == 0x65E8_773D && KSCATEGORY_CAPTURE.Data4[7] == 0x96);
    assert!(KSCATEGORY_TOPOLOGY.Data1 == 0xDDA5_4A40 && KSCATEGORY_TOPOLOGY.Data4[7] == 0x00);
    assert!(KSCATEGORY_RENDER.Data1 == 0x65E8_773E && KSCATEGORY_REALTIME.Data1 == 0xEB11_5FFC);
    assert!(KSAUDFNAME_VOLUME_CONTROL.Data1 == 0x185F_EDF7 && KSAUDFNAME_WAVE_MUTE.Data1 == 0x185F_EDE6);
    assert!(RENDER_CATEGORIES[0].Data1 == 0x2C6B_B644 && RENDER_CATEGORIES[6].Data1 == 0xE630_310C);
    assert!(RENDER_CATEGORIES[1].Data1 == KSCATEGORY_AUDIO.Data1);
    assert!(AVRCP_VOLUME_SET.Data1 == 0x401E_566B && A2DP_SERVICE_METHOD_SET.Data1 == 0x0E62_540F);
    // The volume element's range is the one avrcp_vol.rs maps.
    assert!(LEVEL_MIN as u32 == 0xFFC0_8000 && LEVEL_MAX == 0 && LEVEL_STEP == 0x8000 && CHANNELS == 2);
};

// ---------------------------------------------------------------------------
// Contexts
// ---------------------------------------------------------------------------

macro_rules! acx_context_type {
    ($info:ident, $ty:ty, $name:literal, $getter:ident) => {
        static $info: CtxInfo = CtxInfo(WDF_OBJECT_CONTEXT_TYPE_INFO {
            Size: size_of::<WDF_OBJECT_CONTEXT_TYPE_INFO>() as ULONG,
            ContextName: $name.as_ptr().cast(),
            ContextSize: size_of::<$ty>(),
            UniqueType: &raw const $info.0,
            EvtDriverGetUniqueContextType: None,
        });
        /// Null if the object has no context of this type.
        /// SAFETY: `h` must be a live WDF object.
        unsafe fn $getter(h: WDFOBJECT) -> *mut $ty {
            unsafe { call_unsafe_wdf_function_binding!(WdfObjectGetTypedContextWorker, h, &raw const $info.0) }
                .cast::<$ty>()
        }
    };
}

/// Second context on the WDFDEVICE (the pipe's `DeviceContext` is untouched).
/// `render` and `failed` are only touched from PnP callbacks, which WDF
/// serialises.
#[repr(C)]
struct AcxDeviceCtx {
    /// Render circuit, between AcxDeviceAddCircuit and AcxDeviceRemoveCircuit.
    render: ACXCIRCUIT,
    /// Circuit creation failed once; do not retry on a later PrepareHardware
    /// (the half-built circuit object stays parented to the device).
    failed: bool,
    /// The bridge pin's ACXJACK while its circuit is added, else 0. Read by
    /// the work item.
    jack: AtomicUsize,
    /// Runs [`evt_jack_work`] at PASSIVE. Created once in [`device_created`].
    jack_work: WDFWORKITEM,
    /// Streams that exist on this device (at most
    /// [`rt_limits::MAX_STREAMS`]). All zero, as WDF hands the context out,
    /// is the empty count.
    streams: StreamSlots,
    /// The unified volume is on for this device (neither key has
    /// `DisableAvrcpVolume`). Written once in [`device_created`].
    avrcp_volume: bool,
    /// The circuit's ACXVOLUME / ACXMUTE / ACXEVENT while it is added, else
    /// 0. Read by [`evt_vol_work`]; cleared (and the work item flushed) in
    /// [`evt_release_hardware`] before the circuit is deleted.
    volume: AtomicUsize,
    mute: AtomicUsize,
    event: AtomicUsize,
    /// Runs [`evt_vol_work`] at PASSIVE. Created once in [`device_created`].
    vol_work: WDFWORKITEM,
    /// `VOL_WORK_*` bits the work item has to do.
    vol_pending: AtomicU32,
}
acx_context_type!(ACX_DEVICE_CTX_INFO, AcxDeviceCtx, c"CrossCAcxDeviceContext", acx_device_ctx);

/// [`evt_vol_work`] jobs.
const VOL_WORK_LEVEL: u32 = 1;
const VOL_WORK_MUTE: u32 = 2;
const VOL_WORK_EVENT: u32 = 4;

/// Context of the circuit and of its volume, mute and event objects: the
/// way from an ACX callback to the device.
#[repr(C)]
struct LinkCtx {
    dev: *mut DeviceContext,
    adev: *mut AcxDeviceCtx,
}
acx_context_type!(LINK_CTX_INFO, LinkCtx, c"CrossCAcxLinkContext", link_ctx);

/// Context of the ACXJACK: the way from the presence callback to the device.
#[repr(C)]
struct JackCtx {
    dev: *mut DeviceContext,
}
acx_context_type!(JACK_CTX_INFO, JackCtx, c"CrossCAcxJackContext", jack_ctx);

/// Per-stream engine (context of the ACXSTREAM, zero-initialised by WDF).
/// Plain fields are written only by the PASSIVE stream callbacks while the
/// timer is stopped; everything the timer DPC shares is atomic.
#[repr(C)]
struct StreamCtx {
    stream: ACXSTREAM,
    timer: WDFTIMER,
    /// The pipe's device context (owner of the PCM tap). Set at creation.
    dev: *mut DeviceContext,
    /// The device's stream count while this stream holds one of its slots,
    /// else null. Given back in [`evt_stream_cleanup`]. The count is in a
    /// context of the WDFDEVICE, an ancestor of the stream, so it is still
    /// there when the stream's cleanup callback runs.
    slots: *const StreamSlots,
    /// This stream's format is one the audio contract can describe and the
    /// tap's session knows the stream ([`audio::stream_created`]): it feeds
    /// the tap now, or can later (the tap follows the running stream).
    known: bool,
    /// Ring storage for the tap: nonpaged pool, allocated in PrepareHardware
    /// by every known stream (the tap can move to it), freed in
    /// ReleaseHardware (or at destroy) after the session let go of it.
    ring: *mut u8,
    ring_bytes: usize,
    /// ACX_STREAM_STATE as tracked by our callbacks.
    state: AtomicU32,
    qpc_freq: u64,
    sample_rate: u32,
    bits: u32,
    channels: u32,
    block_align: u32,
    bytes_per_sec: u32,
    packet_count: u32,
    packet_size: u32,
    first_packet_offset: u32,
    packets: [*mut u8; MAX_PACKETS],

    /// Timer may re-arm itself.
    running: AtomicBool,
    /// Packets completed since PrepareHardware (the packet being played).
    current_packet: AtomicU32,
    /// Timeline of the current Run: packet `run_base_packet + n` completes at
    /// `run_start_hns + shift_hns + n * packet duration`.
    run_start_hns: AtomicU64,
    run_base_packet: AtomicU32,
    shift_hns: AtomicU64,
    /// Highest position (bytes) reported so far; keeps it monotonic.
    reported_bytes: AtomicU64,

    // stats
    runs: AtomicU32,
    packets_done: AtomicU64,
    nonsilent_packets: AtomicU64,
    late_passes: AtomicU64,
    timeline_shifts: AtomicU64,
    max_late_hns: AtomicU64,
    set_packet_late: AtomicU32,
    set_packet_overrun: AtomicU32,
    /// Packets replaced by silence in the tap (see [`FRESH_PACKETS_PER_PASS`]).
    stale_packets: AtomicU64,
}
acx_context_type!(STREAM_CTX_INFO, StreamCtx, c"CrossCAcxStreamContext", stream_ctx);

// ---------------------------------------------------------------------------
// Kill switch
// ---------------------------------------------------------------------------

/// A REG_DWORD flag in an open key is present and non-zero.
unsafe fn flag_value(key: WDFKEY, name: &'static [u16]) -> bool {
    unsafe {
        let name = unicode(name);
        let mut value: ULONG = 0;
        let status = call_unsafe_wdf_function_binding!(WdfRegistryQueryULong, key, &raw const name, &raw mut value);
        nt_success(status) && value != 0
    }
}

/// Reads `DisableAcx` from an open key and closes the key.
unsafe fn disable_acx_value(key: WDFKEY) -> bool {
    unsafe {
        let off = flag_value(key, &DISABLE_ACX_VALUE);
        call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
        off
    }
}

/// `DisableAvrcpVolume` in the service Parameters key (read in `driver_init`).
static AVRCP_VOLUME_OFF: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Driver / device hooks (called from lib.rs)
// ---------------------------------------------------------------------------

/// The loader filled `AcxFunctions` / `AcxDriverGlobals` (class binding ran).
unsafe fn table_bound() -> bool {
    unsafe {
        let first = (&raw const AcxFunctions).cast::<ACXFUNC>();
        !AcxDriverGlobals.is_null()
            && first.add(_ACXFUNCENUM::AcxDriverInitializeTableIndex as usize).read().is_some()
            && first.add(_ACXFUNCENUM::AcxRtStreamNotifyPacketCompleteTableIndex as usize).read().is_some()
    }
}

/// DriverEntry hook, after WdfDriverCreate. Never fails the driver: on any
/// problem ACX simply stays off and the driver is the plain L2CAP pipe.
pub unsafe fn driver_init(driver: WDFDRIVER) {
    unsafe {
        let mut key: WDFKEY = null_mut();
        let status = call_unsafe_wdf_function_binding!(
            WdfDriverOpenParametersRegistryKey,
            driver,
            KEY_READ,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut key
        );
        if nt_success(status) {
            if flag_value(key, &DISABLE_AVRCP_VOLUME_VALUE) {
                AVRCP_VOLUME_OFF.store(true, Ordering::SeqCst);
                trace!("avrcp: DisableAvrcpVolume set in the service Parameters key - unified volume off");
            }
            if disable_acx_value(key) {
                trace!("acx: DisableAcx set in the service Parameters key - ACX off");
                return;
            }
        }
        if !table_bound() {
            let globals = (&raw const AcxDriverGlobals).read();
            trace!("acx: function table not bound (globals {:p}) - ACX off", globals);
            return;
        }
        let mut cfg = ACX_DRIVER_CONFIG { Size: size_of::<ACX_DRIVER_CONFIG>() as ULONG, Flags: 0 };
        let status = acx!(AcxDriverInitializeTableIndex, PFN_ACXDRIVERINITIALIZE, driver, &raw mut cfg);
        if !nt_success(status) {
            trace!("acx: AcxDriverInitialize failed {:#x} - ACX off", status);
            return;
        }
        ACX_READY.store(true, Ordering::SeqCst);
        trace!("acx: driver initialized (ACX 1.{})", (&raw const AcxMinimumVersionRequired).read());
    }
}

/// EvtDriverDeviceAdd hook; must be the first thing done with `init`.
/// `Ok(true)`: ACX is on for this device - register [`evt_prepare_hardware`] /
/// [`evt_release_hardware`] and call [`device_created`] after WdfDeviceCreate.
/// `Ok(false)`: ACX is off, `init` untouched.
pub unsafe fn device_init(init: PWDFDEVICE_INIT) -> Result<bool, NTSTATUS> {
    unsafe {
        if !ACX_READY.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let mut key: WDFKEY = null_mut();
        let status = call_unsafe_wdf_function_binding!(
            WdfFdoInitOpenRegistryKey,
            init,
            PLUGPLAY_REGKEY_DEVICE,
            KEY_READ,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut key
        );
        if nt_success(status) && disable_acx_value(key) {
            trace!("acx: DisableAcx set in the device key - ACX off for this device");
            return Ok(false);
        }
        // ACX_DEVICEINIT_CONFIG_INIT
        let mut cfg = ACX_DEVICEINIT_CONFIG {
            Size: size_of::<ACX_DEVICEINIT_CONFIG>() as ULONG,
            Flags: 0,
            SynchronizationScope: _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeNone,
            ExecutionLevel: _WDF_EXECUTION_LEVEL::WdfExecutionLevelPassive,
            Context: [null_mut(); 4],
        };
        let status = acx!(AcxDeviceInitInitializeTableIndex, PFN_ACXDEVICEINITINITIALIZE, init, &raw mut cfg);
        if !nt_success(status) {
            trace!("acx: AcxDeviceInitInitialize failed {:#x}", status);
            return Err(status);
        }
        Ok(true)
    }
}

/// EvtDriverDeviceAdd hook, right after WdfDeviceCreate, when [`device_init`]
/// returned `Ok(true)`. A failure must fail DeviceAdd.
pub unsafe fn device_created(device: WDFDEVICE) -> NTSTATUS {
    unsafe {
        let mut attr = attributes();
        attr.ContextTypeInfo = &raw const ACX_DEVICE_CTX_INFO.0;
        let status =
            call_unsafe_wdf_function_binding!(WdfObjectAllocateContext, device.cast(), &raw mut attr, null_mut());
        if !nt_success(status) {
            trace!("acx: device context allocation failed {:#x}", status);
            return status;
        }
        // ACX_DEVICE_CONFIG_INIT
        let mut cfg =
            ACX_DEVICE_CONFIG { Size: size_of::<ACX_DEVICE_CONFIG>() as ULONG, Flags: 0, Context: [null_mut(); 4] };
        let status = acx!(AcxDeviceInitializeTableIndex, PFN_ACXDEVICEINITIALIZE, device, &raw mut cfg);
        if !nt_success(status) {
            trace!("acx: AcxDeviceInitialize failed {:#x}", status);
            return status;
        }
        // Work item for jack state notifications (see jack_changed). Not
        // serialised: WDF_WORKITEM_CONFIG_INIT's AutomaticSerialization = TRUE
        // would require a passive-level parent and nothing here needs it.
        let ctx = acx_device_ctx(device.cast());
        let mut wcfg = WDF_WORKITEM_CONFIG {
            Size: size_of::<WDF_WORKITEM_CONFIG>() as ULONG,
            EvtWorkItemFunc: Some(evt_jack_work),
            AutomaticSerialization: 0,
        };
        let mut attr = attributes();
        attr.ParentObject = device.cast();
        let status =
            call_unsafe_wdf_function_binding!(WdfWorkItemCreate, &raw mut wcfg, &raw mut attr, addr_of_mut!((*ctx).jack_work));
        if !nt_success(status) {
            trace!("acx: WdfWorkItemCreate failed {:#x}", status);
            (*ctx).jack_work = null_mut();
            return status;
        }

        // Unified volume: both kill switches, then its work item. Neither
        // can fail the device; without the work item the feature is off.
        let mut device_off = false;
        let mut key: WDFKEY = null_mut();
        let st = call_unsafe_wdf_function_binding!(
            WdfDeviceOpenRegistryKey,
            device,
            PLUGPLAY_REGKEY_DEVICE,
            KEY_READ,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut key
        );
        if nt_success(st) {
            device_off = flag_value(key, &DISABLE_AVRCP_VOLUME_VALUE);
            call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
        }
        if device_off {
            trace!("avrcp: DisableAvrcpVolume set in the device key - unified volume off for this device");
        }
        (*ctx).avrcp_volume = !device_off && !AVRCP_VOLUME_OFF.load(Ordering::SeqCst);
        if (*ctx).avrcp_volume {
            let mut wcfg = WDF_WORKITEM_CONFIG {
                Size: size_of::<WDF_WORKITEM_CONFIG>() as ULONG,
                EvtWorkItemFunc: Some(evt_vol_work),
                AutomaticSerialization: 0,
            };
            let mut attr = attributes();
            attr.ParentObject = device.cast();
            let st = call_unsafe_wdf_function_binding!(
                WdfWorkItemCreate,
                &raw mut wcfg,
                &raw mut attr,
                addr_of_mut!((*ctx).vol_work)
            );
            if !nt_success(st) {
                trace!("avrcp: WdfWorkItemCreate failed {:#x} - unified volume off", st);
                (*ctx).vol_work = null_mut();
                (*ctx).avrcp_volume = false;
            }
        }
        trace!("acx: device initialized (unified volume {})", if (*ctx).avrcp_volume { "on" } else { "off" });
        STATUS_SUCCESS
    }
}

/// EvtDevicePrepareHardware: create the render circuit and add it to the
/// device (visible once the device is in D0). The circuit is re-created on
/// every PrepareHardware because AcxDeviceRemoveCircuit deletes it. Never
/// fails device start: without the circuit the driver is still the pipe.
pub unsafe extern "C" fn evt_prepare_hardware(device: WDFDEVICE, _raw: WDFCMRESLIST, _translated: WDFCMRESLIST) -> NTSTATUS {
    unsafe {
        let ctx = acx_device_ctx(device.cast());
        if ctx.is_null() || !(*ctx).render.is_null() || (*ctx).failed {
            return STATUS_SUCCESS;
        }
        let mut built = add_render_circuit(device, (*ctx).avrcp_volume);
        if let Err(status) = built
            && (*ctx).avrcp_volume
        {
            // The half-built circuit is gone: the plain v0.1.1 circuit
            // instead, and no new attempt with the feature on this device
            // (a rebalance would only fail the same way).
            trace!("avrcp: circuit with the volume contract failed {:#x} - unified volume off, building it without", status);
            (*ctx).avrcp_volume = false;
            built = add_render_circuit(device, false);
        }
        let Built { circuit, jack, vol } = match built {
            Ok(b) => b,
            Err(status) => {
                trace!("acx: render circuit failed {:#x} - no endpoint", status);
                (*ctx).failed = true;
                return STATUS_SUCCESS;
            }
        };
        (*ctx).render = circuit;
        (*ctx).jack.store(jack as usize, Ordering::SeqCst);
        let dev = crate::device_ctx(device.cast());
        (*ctx).volume.store(vol.volume as usize, Ordering::SeqCst);
        (*ctx).mute.store(vol.mute as usize, Ordering::SeqCst);
        (*ctx).event.store(vol.event as usize, Ordering::SeqCst);
        let element = !vol.volume.is_null();
        audio::volume(dev, |v| v.set_element(element));
        // From here on the audio IOCTLs work (AUDIO_FLAG_ACX).
        audio::set_endpoint(dev, true);
        trace!(
            "acx: render circuit added (jack: unplugged until a service sets the link; hardware volume {}, mute {}, AVRCP event {})",
            element,
            !vol.mute.is_null(),
            !vol.event.is_null()
        );
        STATUS_SUCCESS
    }
}

/// EvtDeviceReleaseHardware: the only place AcxDeviceRemoveCircuit may be
/// called. It also deletes the circuit object.
pub unsafe extern "C" fn evt_release_hardware(device: WDFDEVICE, _translated: WDFCMRESLIST) -> NTSTATUS {
    unsafe {
        let ctx = acx_device_ctx(device.cast());
        if ctx.is_null() || (*ctx).render.is_null() {
            return STATUS_SUCCESS;
        }
        // The jack goes away with the circuit: stop notifying it, and wait
        // out a notification that is running (the work item reads the handle
        // once; one that starts after the store sees 0).
        (*ctx).jack.store(0, Ordering::SeqCst);
        if !(*ctx).jack_work.is_null() {
            call_unsafe_wdf_function_binding!(WdfWorkItemFlush, (*ctx).jack_work);
        }
        // Same for the volume objects, which also go with the circuit.
        (*ctx).volume.store(0, Ordering::SeqCst);
        (*ctx).mute.store(0, Ordering::SeqCst);
        (*ctx).event.store(0, Ordering::SeqCst);
        if !(*ctx).vol_work.is_null() {
            call_unsafe_wdf_function_binding!(WdfWorkItemFlush, (*ctx).vol_work);
        }
        let dev = crate::device_ctx(device.cast());
        audio::volume(dev, |v| v.set_element(false));
        audio::set_endpoint(dev, false);
        let status = acx!(AcxDeviceRemoveCircuitTableIndex, PFN_ACXDEVICEREMOVECIRCUIT, device, (*ctx).render);
        (*ctx).render = null_mut();
        trace!("acx: render circuit removed ({:#x})", status);
        STATUS_SUCCESS
    }
}

// ---------------------------------------------------------------------------
// Render circuit (RenderCircuit.cpp: CodecR_CreateRenderCircuit)
// ---------------------------------------------------------------------------

fn check(status: NTSTATUS, what: &str) -> Result<(), NTSTATUS> {
    if nt_success(status) {
        Ok(())
    } else {
        trace!("acx: {} failed {:#x}", what, status);
        Err(status)
    }
}

/// The circuit's unified-volume objects; null where not created.
#[derive(Clone, Copy)]
struct VolObjects {
    volume: ACXVOLUME,
    mute: ACXMUTE,
    event: ACXEVENT,
}

const NO_VOL: VolObjects = VolObjects { volume: null_mut(), mute: null_mut(), event: null_mut() };

/// A render circuit, ready for AcxDeviceAddCircuit.
struct Built {
    circuit: ACXCIRCUIT,
    jack: ACXJACK,
    vol: VolObjects,
}

/// Unified volume, before AcxCircuitCreate: the category list (ACX's own
/// plus the AVRCP volume class), the property and the method.
unsafe fn assign_avrcp_init(init: PACXCIRCUIT_INIT) -> NTSTATUS {
    unsafe {
        let st = acx!(
            AcxCircuitInitAssignCategoriesTableIndex,
            PFN_ACXCIRCUITINITASSIGNCATEGORIES,
            init,
            RENDER_CATEGORIES.as_ptr(),
            RENDER_CATEGORIES.len() as ULONG
        );
        if !nt_success(st) {
            trace!("avrcp: AcxCircuitInitAssignCategories failed {:#x}", st);
            return st;
        }
        let st = acx!(
            AcxCircuitInitAssignPropertiesTableIndex,
            PFN_ACXCIRCUITINITASSIGNPROPERTIES,
            init,
            AVRCP_PROPERTIES.0.get().cast(),
            1
        );
        if !nt_success(st) {
            trace!("avrcp: AcxCircuitInitAssignProperties failed {:#x}", st);
            return st;
        }
        let st =
            acx!(AcxCircuitInitAssignMethodsTableIndex, PFN_ACXCIRCUITINITASSIGNMETHODS, init, AVRCP_METHODS.0.get().cast(), 1);
        if !nt_success(st) {
            trace!("avrcp: AcxCircuitInitAssignMethods failed {:#x}", st);
        }
        st
    }
}

/// Unified volume, after AcxCircuitCreate and before the pins: the volume
/// and mute elements (ACX connects host pin -> volume -> mute -> bridge pin
/// in the order they are added) and the AVRCP event. Without the volume
/// element nothing else is made and without the event the result lacks it;
/// either way [`add_render_circuit`] then discards the whole circuit and
/// builds the v0.1.1 one, because the category, property and method
/// assigned before `AcxCircuitCreate` cannot be taken back. Without the
/// mute element the circuit is kept and Windows mutes by itself.
unsafe fn add_volume_objects(device: WDFDEVICE, circuit: ACXCIRCUIT) -> VolObjects {
    unsafe {
        let mut out = NO_VOL;
        let link = |obj: WDFOBJECT| {
            let c = link_ctx(obj);
            if !c.is_null() {
                (*c).dev = crate::device_ctx(device.cast());
                (*c).adev = acx_device_ctx(device.cast());
            }
            !c.is_null()
        };

        // ---- volume: BthA2dp's range, two channels, no ramp callback ----
        // (ACX prefers the ramped callback when present; the buds cannot ramp.)
        let mut vcb = ACX_VOLUME_CALLBACKS {
            Size: size_of::<ACX_VOLUME_CALLBACKS>() as ULONG,
            EvtAcxVolumeAssignLevel: Some(evt_volume_assign),
            EvtAcxRampedVolumeAssignLevel: None,
            EvtAcxVolumeRetrieveLevel: Some(evt_volume_retrieve),
            EvtAcxVolumeProcessRequest: None,
        };
        let mut vcfg = ACX_VOLUME_CONFIG {
            Size: size_of::<ACX_VOLUME_CONFIG>() as ULONG,
            Id: ACX_ELEMENT_ID_DEFAULT,
            Name: &raw const KSAUDFNAME_VOLUME_CONTROL,
            Flags: 0,
            ChannelsCount: CHANNELS as ULONG,
            Maximum: LEVEL_MAX,
            Minimum: LEVEL_MIN,
            SteppingDelta: LEVEL_STEP as ULONG,
            Callbacks: &raw mut vcb,
        };
        let mut attr = attributes();
        attr.ParentObject = circuit.cast();
        attr.ContextTypeInfo = &raw const LINK_CTX_INFO.0;
        let mut volume: ACXVOLUME = null_mut();
        let st = acx!(AcxVolumeCreateTableIndex, PFN_ACXVOLUMECREATE, circuit.cast(), &raw mut attr, &raw mut vcfg, &raw mut volume);
        if !nt_success(st) || !link(volume.cast()) {
            trace!("avrcp: AcxVolumeCreate failed {:#x} - no hardware volume", st);
            return NO_VOL;
        }

        // ---- mute -----------------------------------------------------------
        let mut mcb = ACX_MUTE_CALLBACKS {
            Size: size_of::<ACX_MUTE_CALLBACKS>() as ULONG,
            EvtAcxMuteAssignState: Some(evt_mute_assign),
            EvtAcxMuteRetrieveState: Some(evt_mute_retrieve),
            EvtAcxMuteProcessRequest: None,
        };
        let mut mcfg = ACX_MUTE_CONFIG {
            Size: size_of::<ACX_MUTE_CONFIG>() as ULONG,
            Id: ACX_ELEMENT_ID_DEFAULT,
            Name: &raw const KSAUDFNAME_WAVE_MUTE,
            Flags: 0,
            ChannelsCount: CHANNELS as ULONG,
            Callbacks: &raw mut mcb,
        };
        let mut attr = attributes();
        attr.ParentObject = circuit.cast();
        attr.ContextTypeInfo = &raw const LINK_CTX_INFO.0;
        let mut mute: ACXMUTE = null_mut();
        let st = acx!(AcxMuteCreateTableIndex, PFN_ACXMUTECREATE, circuit.cast(), &raw mut attr, &raw mut mcfg, &raw mut mute);
        if !nt_success(st) || !link(mute.cast()) {
            trace!("avrcp: AcxMuteCreate failed {:#x} - no hardware mute", st);
            mute = null_mut();
        }

        let mut elements: [ACXELEMENT; 2] = [volume.cast(), mute.cast()];
        let count = if mute.is_null() { 1 } else { 2 };
        let st = acx!(AcxCircuitAddElementsTableIndex, PFN_ACXCIRCUITADDELEMENTS, circuit, elements.as_mut_ptr(), count);
        if !nt_success(st) {
            trace!("avrcp: AcxCircuitAddElements failed {:#x} - no hardware volume", st);
            return NO_VOL;
        }
        out.volume = volume;
        out.mute = mute;

        // ---- the AVRCP event ------------------------------------------------
        let mut ecb = ACX_EVENT_CALLBACKS {
            Size: size_of::<ACX_EVENT_CALLBACKS>() as ULONG,
            EvtAcxEventEnable: Some(evt_avrcp_event_enable),
            EvtAcxEventDisable: Some(evt_avrcp_event_disable),
            EvtAcxEventProcessRequest: None,
        };
        let mut ecfg = ACX_EVENT_CONFIG {
            Size: size_of::<ACX_EVENT_CONFIG>() as ULONG,
            Flags: 0,
            Set: &raw const AVRCP_VOLUME_SET,
            Id: AVRCP_VOLUME_ID,
            Type: 0,
            Callbacks: &raw mut ecb,
        };
        let mut attr = attributes();
        attr.ParentObject = circuit.cast();
        attr.ContextTypeInfo = &raw const LINK_CTX_INFO.0;
        let mut event: ACXEVENT = null_mut();
        let st = acx!(AcxEventCreateTableIndex, PFN_ACXEVENTCREATE, circuit.cast(), &raw mut attr, &raw mut ecfg, &raw mut event);
        if !nt_success(st) || !link(event.cast()) {
            trace!("avrcp: AcxEventCreate failed {:#x} - no AVRCP event", st);
            return out;
        }
        let st = acx!(AcxCircuitAddEventsTableIndex, PFN_ACXCIRCUITADDEVENTS, circuit, &raw mut event, 1);
        if !nt_success(st) {
            trace!("avrcp: AcxCircuitAddEvents failed {:#x} - no AVRCP event", st);
            return out;
        }
        out.event = event;
        out
    }
}

/// Build the render circuit and add it to the device.
///
/// With `avrcp` every failure deletes the circuit if one was created, so the
/// caller can build the plain one in its place: a missing volume element or
/// AVRCP event counts as a failure (a circuit that advertises the AVRCP
/// interface, property and method without both would be neither the unified
/// endpoint nor the v0.1.1 one), and so does `AcxDeviceAddCircuit`, which is
/// where ACX checks what the feature adds (`AfxDevice::AddCircuit` ->
/// `AfxCircuit::FinishInitializing`: connections through the elements,
/// `FinishCategoryInitializing`; then `AfxCircuit::Register`: one interface
/// per category). A circuit that failed `AcxDeviceAddCircuit` is not in the
/// device's list (AddCircuit unlinks it and drops its reference), and ACX's
/// own cleanup of a circuit (`AfxCircuit::Deinitialize`, run by the delete)
/// unregisters whatever was registered and releases the create-time
/// reference; this is the documented `AcxDeviceDetachCircuit` +
/// `WdfObjectDelete` path.
///
/// Without `avrcp` the behaviour is v0.1.1's: a failed circuit stays
/// parented to the device and goes away with it.
unsafe fn add_render_circuit(device: WDFDEVICE, avrcp: bool) -> Result<Built, NTSTATUS> {
    unsafe {
        let discard = |circuit: ACXCIRCUIT| {
            if avrcp && !circuit.is_null() {
                call_unsafe_wdf_function_binding!(WdfObjectDelete, circuit.cast());
            }
        };
        let built = match create_render_circuit(device, avrcp) {
            Ok(b) => b,
            Err((status, circuit)) => {
                discard(circuit);
                return Err(status);
            }
        };
        if avrcp && (built.vol.volume.is_null() || built.vol.event.is_null()) {
            trace!("avrcp: volume element or AVRCP event missing - discarding the circuit");
            discard(built.circuit);
            return Err(STATUS_UNSUCCESSFUL);
        }
        let status = acx!(AcxDeviceAddCircuitTableIndex, PFN_ACXDEVICEADDCIRCUIT, device, built.circuit);
        if !nt_success(status) {
            trace!("acx: AcxDeviceAddCircuit failed {:#x}", status);
            discard(built.circuit);
            return Err(status);
        }
        Ok(built)
    }
}

/// `Err((status, circuit))`: `circuit` is the created circuit (never
/// added), or null when the failure came before `AcxCircuitCreate`
/// succeeded.
unsafe fn create_render_circuit(device: WDFDEVICE, avrcp: bool) -> Result<Built, (NTSTATUS, ACXCIRCUIT)> {
    unsafe {
        // ---- circuit -------------------------------------------------------
        let mut init = acx!(AcxCircuitInitAllocateTableIndex, PFN_ACXCIRCUITINITALLOCATE, device);
        if init.is_null() {
            trace!("acx: AcxCircuitInitAllocate failed");
            return Err((STATUS_INSUFFICIENT_RESOURCES, null_mut()));
        }
        acx!(AcxCircuitInitSetComponentIdTableIndex, PFN_ACXCIRCUITINITSETCOMPONENTID, init, &raw const RENDER_COMPONENT_ID);
        let status =
            acx!(AcxCircuitInitAssignNameTableIndex, PFN_ACXCIRCUITINITASSIGNNAME, init, &raw const CIRCUIT_NAME_US.0);
        if !nt_success(status) {
            // The sample ignores this status; without the name the INF
            // interfaces cannot match, so treat it as fatal for the circuit.
            trace!("acx: AcxCircuitInitAssignName failed {:#x}", status);
            acx!(AcxCircuitInitFreeTableIndex, PFN_ACXCIRCUITINITFREE, init);
            return Err((status, null_mut()));
        }
        acx!(
            AcxCircuitInitSetCircuitTypeTableIndex,
            PFN_ACXCIRCUITINITSETCIRCUITTYPE,
            init,
            _ACX_CIRCUIT_TYPE::AcxCircuitTypeRender
        );
        // ACX_CIRCUIT_PNPPOWER_CALLBACKS_INIT
        let mut power = ACX_CIRCUIT_PNPPOWER_CALLBACKS {
            Size: size_of::<ACX_CIRCUIT_PNPPOWER_CALLBACKS>() as ULONG,
            EvtAcxCircuitPrepareHardware: None,
            EvtAcxCircuitReleaseHardware: None,
            EvtAcxCircuitPowerUp: Some(evt_circuit_power_up),
            EvtAcxCircuitPowerDown: Some(evt_circuit_power_down),
        };
        acx!(
            AcxCircuitInitSetAcxCircuitPnpPowerCallbacksTableIndex,
            PFN_ACXCIRCUITINITSETACXCIRCUITPNPPOWERCALLBACKS,
            init,
            &raw mut power
        );
        let status = acx!(
            AcxCircuitInitAssignAcxCreateStreamCallbackTableIndex,
            PFN_ACXCIRCUITINITASSIGNACXCREATESTREAMCALLBACK,
            init,
            Some(evt_circuit_create_stream)
        );
        if !nt_success(status) {
            trace!("acx: AcxCircuitInitAssignAcxCreateStreamCallback failed {:#x}", status);
            acx!(AcxCircuitInitFreeTableIndex, PFN_ACXCIRCUITINITFREE, init);
            return Err((status, null_mut()));
        }
        if avrcp {
            let status = assign_avrcp_init(init);
            if !nt_success(status) {
                acx!(AcxCircuitInitFreeTableIndex, PFN_ACXCIRCUITINITFREE, init);
                return Err((status, null_mut()));
            }
        }
        let mut attr = attributes();
        if avrcp {
            // Only with the feature, so the circuit without it is created
            // exactly as in v0.1.1.
            attr.ContextTypeInfo = &raw const LINK_CTX_INFO.0;
        }
        let mut circuit: ACXCIRCUIT = null_mut();
        let status =
            acx!(AcxCircuitCreateTableIndex, PFN_ACXCIRCUITCREATE, device, &raw mut attr, &raw mut init, &raw mut circuit);
        if !nt_success(status) {
            trace!("acx: AcxCircuitCreate failed {:#x}", status);
            // On failure the init is still ours (the sample frees it when the
            // pointer was not consumed, i.e. not nulled).
            if !init.is_null() {
                acx!(AcxCircuitInitFreeTableIndex, PFN_ACXCIRCUITINITFREE, init);
            }
            return Err((status, null_mut()));
        }
        // From here on a failure returns `circuit` (never added): without the
        // feature it stays parented to the device and goes away with it,
        // with it add_render_circuit deletes it.
        let check = |status: NTSTATUS, what: &str| check(status, what).map_err(|s| (s, circuit));
        let vol = if avrcp {
            // The property and method callbacks find the device through it.
            let lc = link_ctx(circuit.cast());
            if !lc.is_null() {
                (*lc).dev = crate::device_ctx(device.cast());
                (*lc).adev = acx_device_ctx(device.cast());
            }
            add_volume_objects(device, circuit)
        } else {
            NO_VOL
        };
        if avrcp {
            trace!(
                "avrcp: circuit has the volume contract (category {{e630310c}}, property/event {{401e566b}}/1, method {{0e62540f}}/6), volume element {}, mute {}, event {}",
                !vol.volume.is_null(),
                !vol.mute.is_null(),
                !vol.event.is_null()
            );
        }

        // ---- pins: [0] host (sink, streaming), [1] bridge (source) ---------
        let mut pins: [ACXPIN; 2] = [null_mut(); 2];

        // ACX_PIN_CALLBACKS_INIT
        let mut pin_callbacks = ACX_PIN_CALLBACKS {
            Size: size_of::<ACX_PIN_CALLBACKS>() as ULONG,
            EvtAcxPinSetDataFormat: Some(evt_pin_set_data_format),
            ..zeroed()
        };
        // ACX_PIN_CONFIG_INIT
        let mut pin_cfg = ACX_PIN_CONFIG {
            Size: size_of::<ACX_PIN_CONFIG>() as ULONG,
            Flags: 0,
            Id: ACX_ID_DEFAULT,
            Type: _ACX_PIN_TYPE::AcxPinTypeSink,
            Communication: _ACX_PIN_COMMUNICATION::AcxPinCommunicationSink,
            Category: &raw const KSCATEGORY_AUDIO,
            PinCallbacks: &raw mut pin_callbacks,
            MaxStreams: ACX_INSTANCE_INDETERMINATE,
            ..zeroed()
        };
        let mut attr = attributes();
        attr.ParentObject = circuit.cast();
        check(
            acx!(AcxPinCreateTableIndex, PFN_ACXPINCREATE, circuit, &raw mut attr, &raw mut pin_cfg, &raw mut pins[0]),
            "AcxPinCreate(host)",
        )?;

        let mut pin_cfg = ACX_PIN_CONFIG {
            Size: size_of::<ACX_PIN_CONFIG>() as ULONG,
            Flags: 0,
            Id: ACX_ID_DEFAULT,
            Type: _ACX_PIN_TYPE::AcxPinTypeSource,
            Communication: _ACX_PIN_COMMUNICATION::AcxPinCommunicationNone,
            Category: &raw const KSNODETYPE_HEADPHONES,
            MaxStreams: ACX_INSTANCE_INDETERMINATE,
            ..zeroed()
        };
        let mut attr = attributes();
        attr.ParentObject = circuit.cast();
        check(
            acx!(AcxPinCreateTableIndex, PFN_ACXPINCREATE, circuit, &raw mut attr, &raw mut pin_cfg, &raw mut pins[1]),
            "AcxPinCreate(bridge)",
        )?;

        // ---- jack on the bridge pin ----------------------------------------
        // With presence detection (target/acx-research.md 4.7; the ACX
        // sample's CircuitHelper.cpp `CreateAudioJack` passes its callbacks
        // the same way, from the stack): ACX asks evt_jack_retrieve_presence
        // for the state and is told about changes with
        // AcxJackChangeStateNotification.
        // ACX_JACK_CALLBACKS_INIT
        let mut jack_callbacks = ACX_JACK_CALLBACKS {
            Size: size_of::<ACX_JACK_CALLBACKS>() as ULONG,
            EvtAcxJackRetrievePresenceState: Some(evt_jack_retrieve_presence),
        };
        // ACX_JACK_CONFIG_INIT
        let mut jack_cfg = ACX_JACK_CONFIG {
            Size: size_of::<ACX_JACK_CONFIG>() as ULONG,
            Id: ACX_ID_DEFAULT,
            Flags: _ACX_JACK_CONFIG_FLAGS::AcxJackConfigJackDetection as ULONG,
            Callbacks: &raw mut jack_callbacks,
            Description: ACX_JACK_DESCRIPTION {
                ChannelMapping: KSAUDIO_SPEAKER_STEREO,
                Color: 0,
                ConnectionType: _ACX_JACK_CONNECTION_TYPE::AcxConnTypeOtherDigital,
                GeoLocation: _ACX_JACK_GEO_LOCATION::AcxGeoLocNotApplicable,
                GenLocation: _ACX_JACK_GEN_LOCATION::AcxGenLocSeparate,
                // A jack, not an integrated device: something that can be
                // absent. Same as the jack of Microsoft's Bluetooth A2DP
                // sample (sysvad a2dphpspeakertoptable.h: OtherDigital,
                // ePortConnJack, IsConnected set at run time).
                PortConnection: _ACX_JACK_PORT_CONNECTION::AcxPortConnJack,
            },
        };
        let mut attr = attributes();
        attr.ParentObject = pins[1].cast();
        attr.ContextTypeInfo = &raw const JACK_CTX_INFO.0;
        let mut jack: ACXJACK = null_mut();
        check(
            acx!(AcxJackCreateTableIndex, PFN_ACXJACKCREATE, pins[1], &raw mut attr, &raw mut jack_cfg, &raw mut jack),
            "AcxJackCreate",
        )?;
        let jc = jack_ctx(jack.cast());
        if jc.is_null() {
            trace!("acx: jack has no context");
            return Err((STATUS_UNSUCCESSFUL, circuit));
        }
        (*jc).dev = crate::device_ctx(device.cast());
        check(acx!(AcxPinAddJacksTableIndex, PFN_ACXPINADDJACKS, pins[1], &raw mut jack, 1), "AcxPinAddJacks")?;

        // ---- formats on the host pin: RAW (and, off by default, DEFAULT) ---
        // ACX creates the RAW list with the pin (the sample fills only that
        // one). See DEFAULT_MODE_LIST for the DEFAULT list and why it is off.
        let list = acx!(AcxPinGetRawDataFormatListTableIndex, PFN_ACXPINGETRAWDATAFORMATLIST, pins[0]);
        if list.is_null() {
            trace!("acx: AcxPinGetRawDataFormatList returned null");
            return Err((STATUS_INSUFFICIENT_RESOURCES, circuit));
        }
        add_formats(device, circuit, list).map_err(|s| (s, circuit))?;
        if DEFAULT_MODE_LIST {
            // Without the DEFAULT list the pin is exactly what 1.0.0 offered
            // (RAW only), which plays; so a failure here is traced, not
            // fatal. A half-filled list was never assigned and goes away
            // with the circuit, its parent.
            match add_default_mode_list(device, circuit, pins[0]) {
                Ok(()) => trace!("acx: host pin modes: RAW, DEFAULT ({} formats each)", FORMATS.len()),
                Err(status) => trace!("acx: no DEFAULT mode list ({:#x}); host pin modes: RAW", status),
            }
        } else {
            trace!("acx: host pin modes: RAW ({} formats)", FORMATS.len());
        }

        // ACX connects host pin -> (volume -> mute ->) bridge pin by default.
        check(acx!(AcxCircuitAddPinsTableIndex, PFN_ACXCIRCUITADDPINS, circuit, pins.as_mut_ptr(), 2), "AcxCircuitAddPins")?;
        Ok(Built { circuit, jack, vol })
    }
}

/// Add [`FORMATS`] to `list`, `FORMATS[0]` as its default. Each list gets
/// its own ACXDATAFORMAT objects, parented to the circuit.
unsafe fn add_formats(device: WDFDEVICE, circuit: ACXCIRCUIT, list: ACXDATAFORMATLIST) -> Result<(), NTSTATUS> {
    unsafe {
        for (i, ks) in FORMATS.iter().enumerate() {
            // ACX_DATAFORMAT_CONFIG_INIT_KS; ACX copies the KS format.
            let mut cfg: ACX_DATAFORMAT_CONFIG = zeroed();
            cfg.Size = size_of::<ACX_DATAFORMAT_CONFIG>() as ULONG;
            cfg.Flags = 0;
            cfg.Type = _ACX_DATAFORMAT_TYPE::AcxDataFormatKsFormat;
            *cfg.u.KsFormat.as_mut() = core::ptr::from_ref(ks).cast_mut().cast();
            let mut attr = attributes();
            attr.ParentObject = circuit.cast();
            let mut format: ACXDATAFORMAT = null_mut();
            check(
                acx!(AcxDataFormatCreateTableIndex, PFN_ACXDATAFORMATCREATE, device, &raw mut attr, &raw mut cfg, &raw mut format),
                "AcxDataFormatCreate",
            )?;
            check(
                acx!(AcxDataFormatListAddDataFormatTableIndex, PFN_ACXDATAFORMATLISTADDDATAFORMAT, list, format),
                "AcxDataFormatListAddDataFormat",
            )?;
            if i == 0 {
                check(
                    acx!(
                        AcxDataFormatListAssignDefaultDataFormatTableIndex,
                        PFN_ACXDATAFORMATLISTASSIGNDEFAULTDATAFORMAT,
                        list,
                        format
                    ),
                    "AcxDataFormatListAssignDefaultDataFormat",
                )?;
            }
        }
        Ok(())
    }
}

/// Give the host pin a list for `AUDIO_SIGNALPROCESSINGMODE_DEFAULT` with
/// the same formats as its RAW list. The list is parented to the circuit;
/// it is assigned to the pin only once it is complete.
unsafe fn add_default_mode_list(device: WDFDEVICE, circuit: ACXCIRCUIT, pin: ACXPIN) -> Result<(), NTSTATUS> {
    unsafe {
        // ACX_DATAFORMAT_LIST_CONFIG_INIT
        let mut cfg = ACX_DATAFORMAT_LIST_CONFIG { Size: size_of::<ACX_DATAFORMAT_LIST_CONFIG>() as ULONG, Flags: 0 };
        let mut attr = attributes();
        attr.ParentObject = circuit.cast();
        let mut list: ACXDATAFORMATLIST = null_mut();
        check(
            acx!(
                AcxDataFormatListCreateTableIndex,
                PFN_ACXDATAFORMATLISTCREATE,
                device,
                &raw mut attr,
                &raw mut cfg,
                &raw mut list
            ),
            "AcxDataFormatListCreate(DEFAULT)",
        )?;
        if list.is_null() {
            trace!("acx: AcxDataFormatListCreate(DEFAULT) returned no list");
            return Err(STATUS_INSUFFICIENT_RESOURCES);
        }
        add_formats(device, circuit, list)?;
        check(
            acx!(
                AcxPinAssignModeDataFormatListTableIndex,
                PFN_ACXPINASSIGNMODEDATAFORMATLIST,
                pin,
                &raw const AUDIO_SIGNALPROCESSINGMODE_DEFAULT,
                list
            ),
            "AcxPinAssignModeDataFormatList(DEFAULT)",
        )
    }
}

/// Name of a signal processing mode for the trace.
unsafe fn mode_name(mode: *const GUID) -> &'static str {
    unsafe {
        let same =
            |a: &GUID, b: &GUID| a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4;
        match mode.as_ref() {
            None => "no",
            Some(m) if same(m, &AUDIO_SIGNALPROCESSINGMODE_RAW) => "RAW",
            Some(m) if same(m, &AUDIO_SIGNALPROCESSINGMODE_DEFAULT) => "DEFAULT",
            Some(_) => "another",
        }
    }
}

// ---------------------------------------------------------------------------
// Unified volume: ACX callbacks (all PASSIVE), see avrcp_vol.rs for the rules
// ---------------------------------------------------------------------------

/// Milliseconds on a monotonic clock (for the AVRCP staleness check).
fn now_ms() -> u64 {
    unsafe {
        let mut freq: LARGE_INTEGER = zeroed();
        let t = KeQueryPerformanceCounter(&raw mut freq).QuadPart as u64;
        let f = (freq.QuadPart as u64).max(1);
        (t / f) * 1000 + (t % f) * 1000 / f
    }
}

fn pid() -> usize {
    unsafe { wdk_sys::ntddk::PsGetCurrentProcessId() as usize }
}

/// 1/65536 dB as "-12.34 dB" without floating point.
struct Db(i32);

impl core::fmt::Display for Db {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let v = i64::from(self.0);
        let sign = if v < 0 { "-" } else { "" };
        let a = v.unsigned_abs();
        let cents = (a * 100 + 32768) / 65536;
        write!(f, "{sign}{}.{:02} dB", cents / 100, cents % 100)
    }
}

/// Queue `jobs` (`VOL_WORK_*`) for [`evt_vol_work`].
unsafe fn vol_work(adev: *mut AcxDeviceCtx, fx: avrcp_vol::Effects) {
    unsafe {
        let mut jobs = 0;
        if fx.notify_volume {
            jobs |= VOL_WORK_LEVEL;
        }
        if fx.notify_mute {
            jobs |= VOL_WORK_MUTE;
        }
        if fx.avrcp_event {
            jobs |= VOL_WORK_EVENT;
        }
        if jobs == 0 || adev.is_null() || (*adev).vol_work.is_null() {
            return;
        }
        (*adev).vol_pending.fetch_or(jobs, Ordering::SeqCst);
        call_unsafe_wdf_function_binding!(WdfWorkItemEnqueue, (*adev).vol_work);
    }
}

/// Work item (PASSIVE): tell Windows / AVRCP what changed. Never called
/// from inside an ACX callback, so none of these DDIs runs while ACX is
/// dispatching a request of the same circuit. `evt_release_hardware`
/// clears the handles and flushes this before the circuit is deleted.
unsafe extern "C" fn evt_vol_work(item: WDFWORKITEM) {
    unsafe {
        let device = call_unsafe_wdf_function_binding!(WdfWorkItemGetParentObject, item);
        let ctx = acx_device_ctx(device);
        if ctx.is_null() {
            return;
        }
        let jobs = (*ctx).vol_pending.swap(0, Ordering::SeqCst);
        if jobs & VOL_WORK_LEVEL != 0 {
            let volume = (*ctx).volume.load(Ordering::SeqCst) as ACXVOLUME;
            if !volume.is_null() {
                acx!(AcxVolumeChangeLevelNotificationTableIndex, PFN_ACXVOLUMECHANGELEVELNOTIFICATION, volume);
            }
        }
        if jobs & VOL_WORK_MUTE != 0 {
            let mute = (*ctx).mute.load(Ordering::SeqCst) as ACXMUTE;
            if !mute.is_null() {
                acx!(AcxMuteChangeStateNotificationTableIndex, PFN_ACXMUTECHANGESTATENOTIFICATION, mute);
            }
        }
        if jobs & VOL_WORK_EVENT != 0 {
            let event = (*ctx).event.load(Ordering::SeqCst) as ACXEVENT;
            if !event.is_null() {
                acx!(AcxEventGenerateEventTableIndex, PFN_ACXEVENTGENERATEEVENT, event);
                audio::volume(crate::device_ctx(device), |v| v.event_generated());
                trace!("avrcp: volume event generated");
            }
        }
    }
}

/// The device behind an object carrying a [`LinkCtx`].
unsafe fn linked(obj: WDFOBJECT) -> Option<(*mut DeviceContext, *mut AcxDeviceCtx)> {
    unsafe {
        let c = link_ctx(obj);
        if c.is_null() || (*c).dev.is_null() {
            None
        } else {
            Some(((*c).dev, (*c).adev))
        }
    }
}

/// Property `{401e566b}` id 1 on the circuit (BthAvrcp's VolumeController).
/// ACX has matched set, id and verb against the item and checked the value
/// size (>= 4) before calling. Completes the request on every path.
unsafe extern "C" fn evt_avrcp_property(object: ACXOBJECT, request: WDFREQUEST) {
    unsafe {
        let mut p: ACX_REQUEST_PARAMETERS = zeroed();
        p.Size = size_of::<ACX_REQUEST_PARAMETERS>() as USHORT;
        acx!(AcxRequestGetParametersTableIndex, PFN_ACXREQUESTGETPARAMETERS, request, &raw mut p);
        let Some((dev, adev)) = linked(object) else {
            return crate::complete(request, STATUS_INVALID_DEVICE_STATE, 0);
        };
        if p.Type != _ACX_REQUEST_TYPE::AcxRequestTypeProperty {
            return crate::complete(request, STATUS_INVALID_DEVICE_REQUEST, 0);
        }
        let prop = p.Parameters.Property.as_ref();
        if prop.Value.is_null() || (prop.ValueCb as usize) < size_of::<LONG>() {
            return crate::complete(request, STATUS_BUFFER_TOO_SMALL, 0);
        }
        let value = prop.Value.cast::<LONG>();
        match prop.Verb {
            _ACX_PROPERTY_VERB::AcxPropertyVerbGet => {
                let level = audio::volume(dev, |v| v.remote_get());
                value.write_unaligned(level);
                trace!(
                    "avrcp: property GET -> {} (AVRCP {}) [pid {}]",
                    Db(level),
                    avrcp_vol::level_to_avrcp(level),
                    pid()
                );
                crate::complete(request, STATUS_SUCCESS, size_of::<LONG>());
            }
            _ACX_PROPERTY_VERB::AcxPropertyVerbSet => {
                let raw = value.read_unaligned();
                let now = now_ms();
                let (fx, muted, window) =
                    audio::volume(dev, |v| (v.remote_set(raw, now), v.muted(), v.connect_window_open()));
                trace!(
                    "avrcp: property SET {} ({:#x}, AVRCP {}) from the buds: slider {}, mute {}, event {}, connect window {}{} [pid {}]",
                    Db(avrcp_vol::clamp_level(raw)),
                    raw as u32,
                    avrcp_vol::level_to_avrcp(raw),
                    if fx.notify_volume { "moved" } else { "kept" },
                    if fx.notify_mute { "cleared" } else if muted { "on" } else { "off" },
                    fx.avrcp_event,
                    if window { "open" } else { "closed" },
                    if raw != avrcp_vol::clamp_level(raw) { " (clamped)" } else { "" },
                    pid()
                );
                vol_work(adev, fx);
                crate::complete(request, STATUS_SUCCESS, 0);
            }
            verb => {
                trace!("avrcp: property verb {} refused", verb);
                crate::complete(request, STATUS_INVALID_DEVICE_REQUEST, 0);
            }
        }
    }
}

/// Method `{0e62540f}` id 6 (`MethodServiceReady`, from BthAvctpSvc).
/// BthA2dp parks it for the life of the service; the sender only traces
/// the result, so it is answered at once.
unsafe extern "C" fn evt_avrcp_method(_object: ACXOBJECT, request: WDFREQUEST) {
    unsafe {
        let mut p: ACX_REQUEST_PARAMETERS = zeroed();
        p.Size = size_of::<ACX_REQUEST_PARAMETERS>() as USHORT;
        acx!(AcxRequestGetParametersTableIndex, PFN_ACXREQUESTGETPARAMETERS, request, &raw mut p);
        let id = if p.Type == _ACX_REQUEST_TYPE::AcxRequestTypeMethod { p.Parameters.Method.as_ref().Id } else { u32::MAX };
        trace!("avrcp: method {{0e62540f}}/{} (MethodServiceReady) - answered with success [pid {}]", id, pid());
        crate::complete(request, STATUS_SUCCESS, 0);
    }
}

/// First subscriber of the AVRCP event (BthAvrcp's IOCTL_KS_ENABLE_EVENT
/// with an event handle; ACX keeps the list and calls this for the first
/// one only).
unsafe extern "C" fn evt_avrcp_event_enable(event: ACXEVENT) -> NTSTATUS {
    unsafe {
        if let Some((dev, _)) = linked(event.cast()) {
            audio::volume(dev, |v| v.subscribe(true));
        }
        trace!("avrcp: volume event enabled (AVRCP subscribed) [pid {}]", pid());
    }
    STATUS_SUCCESS
}

/// Last subscriber gone (its handle closed, or the circuit is going).
unsafe extern "C" fn evt_avrcp_event_disable(event: ACXEVENT) {
    unsafe {
        if let Some((dev, _)) = linked(event.cast()) {
            audio::volume(dev, |v| v.subscribe(false));
        }
        trace!("avrcp: volume event disabled (no AVRCP subscriber) [pid {}]", pid());
    }
}

unsafe extern "C" fn evt_volume_assign(volume: ACXVOLUME, channel: ULONG, level: LONG) -> NTSTATUS {
    unsafe {
        let Some((dev, adev)) = linked(volume.cast()) else {
            return STATUS_INVALID_DEVICE_STATE;
        };
        let now = now_ms();
        match audio::volume(dev, |v| v.windows_level(channel, level, now)) {
            Ok(fx) => {
                trace!(
                    "avrcp: Windows volume ch {} = {} (AVRCP {}){}",
                    channel as i32,
                    Db(avrcp_vol::clamp_level(level)),
                    avrcp_vol::level_to_avrcp(level),
                    if fx.avrcp_event { ", event" } else { "" }
                );
                vol_work(adev, fx);
                STATUS_SUCCESS
            }
            Err(_) => STATUS_INVALID_PARAMETER,
        }
    }
}

unsafe extern "C" fn evt_volume_retrieve(volume: ACXVOLUME, channel: ULONG, level: *mut LONG) -> NTSTATUS {
    unsafe {
        let Some((dev, _)) = linked(volume.cast()) else {
            return STATUS_INVALID_DEVICE_STATE;
        };
        match audio::volume(dev, |v| v.retrieve_level(channel)) {
            Ok(l) if !level.is_null() => {
                *level = l;
                STATUS_SUCCESS
            }
            Ok(_) => STATUS_INVALID_PARAMETER,
            Err(_) => STATUS_INVALID_PARAMETER,
        }
    }
}

unsafe extern "C" fn evt_mute_assign(mute: ACXMUTE, channel: ULONG, state: ULONG) -> NTSTATUS {
    unsafe {
        let Some((dev, adev)) = linked(mute.cast()) else {
            return STATUS_INVALID_DEVICE_STATE;
        };
        let now = now_ms();
        match audio::volume(dev, |v| v.windows_mute(channel, state != 0, now)) {
            Ok(fx) => {
                trace!(
                    "avrcp: Windows mute ch {} = {}{}",
                    channel as i32,
                    state != 0,
                    if fx.avrcp_event { ", event" } else { "" }
                );
                vol_work(adev, fx);
                STATUS_SUCCESS
            }
            Err(_) => STATUS_INVALID_PARAMETER,
        }
    }
}

unsafe extern "C" fn evt_mute_retrieve(mute: ACXMUTE, channel: ULONG, state: *mut ULONG) -> NTSTATUS {
    unsafe {
        let Some((dev, _)) = linked(mute.cast()) else {
            return STATUS_INVALID_DEVICE_STATE;
        };
        match audio::volume(dev, |v| v.retrieve_mute(channel)) {
            Ok(m) if !state.is_null() => {
                *state = ULONG::from(m);
                STATUS_SUCCESS
            }
            Ok(_) => STATUS_INVALID_PARAMETER,
            Err(_) => STATUS_INVALID_PARAMETER,
        }
    }
}

/// EvtAcxJackRetrievePresenceState (PASSIVE): the jack is "plugged in" while
/// the attached service says the buds have an open, configured stream.
/// Unplugged initially and whenever no service is attached (detach clears
/// the flag).
unsafe extern "C" fn evt_jack_retrieve_presence(jack: ACXJACK, is_connected: PBOOLEAN) -> NTSTATUS {
    unsafe {
        let jc = jack_ctx(jack.cast());
        let connected = !jc.is_null() && !(*jc).dev.is_null() && audio::link((*jc).dev);
        if !is_connected.is_null() {
            *is_connected = BOOLEAN::from(connected);
        }
    }
    STATUS_SUCCESS
}

/// The link flag changed: have ACX re-read the jack state. Any IRQL <=
/// DISPATCH (it is called from request cancellation paths).
///
/// `AcxJackChangeStateNotification` carries no IRQL annotation in acxpin.h
/// and its documentation states none, while almost every other ACX DDI is
/// PASSIVE-only. So it is never called directly: a work item does it at
/// PASSIVE. Enqueueing an already queued work item is a no-op, which is
/// fine: the callback reports "changed", ACX then asks for the state.
pub unsafe fn jack_changed(device: WDFDEVICE) {
    unsafe {
        let ctx = acx_device_ctx(device.cast());
        if ctx.is_null() || (*ctx).jack_work.is_null() || (*ctx).jack.load(Ordering::SeqCst) == 0 {
            return;
        }
        call_unsafe_wdf_function_binding!(WdfWorkItemEnqueue, (*ctx).jack_work);
    }
}

/// Work item (PASSIVE). `evt_release_hardware` clears `jack` and flushes
/// this work item before the circuit (and with it the jack) is deleted.
unsafe extern "C" fn evt_jack_work(item: WDFWORKITEM) {
    unsafe {
        let device = call_unsafe_wdf_function_binding!(WdfWorkItemGetParentObject, item);
        let ctx = acx_device_ctx(device);
        if ctx.is_null() {
            return;
        }
        let jack = (*ctx).jack.load(Ordering::SeqCst) as ACXJACK;
        if jack.is_null() {
            return;
        }
        acx!(AcxJackChangeStateNotificationTableIndex, PFN_ACXJACKCHANGESTATENOTIFICATION, jack);
    }
}

unsafe extern "C" fn evt_circuit_power_up(_device: WDFDEVICE, _circuit: ACXCIRCUIT, previous: WDF_POWER_DEVICE_STATE) -> NTSTATUS {
    trace!("acx: circuit power up (from {})", previous);
    STATUS_SUCCESS
}

unsafe extern "C" fn evt_circuit_power_down(_device: WDFDEVICE, _circuit: ACXCIRCUIT, target: WDF_POWER_DEVICE_STATE) -> NTSTATUS {
    trace!("acx: circuit power down (to {})", target);
    STATUS_SUCCESS
}

/// Same as the sample: the device/mixed format cannot be set on the pin.
unsafe extern "C" fn evt_pin_set_data_format(_pin: ACXPIN, _format: ACXDATAFORMAT) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

// ---------------------------------------------------------------------------
// Stream (RenderCircuit.cpp: CodecR_EvtCircuitCreateStream, StreamEngine.cpp)
// ---------------------------------------------------------------------------

unsafe extern "C" fn evt_circuit_create_stream(
    device: WDFDEVICE,
    circuit: ACXCIRCUIT,
    _pin: ACXPIN,
    mut stream_init: PACXSTREAM_INIT,
    format: ACXDATAFORMAT,
    signal_processing_mode: *const GUID,
    _var_arguments: ACXOBJECTBAG,
) -> NTSTATUS {
    unsafe {
        // One more stream? The KS path is open to interactive users and
        // every stream holds nonpaged RT packets, so the number is bounded
        // (rt_limits.rs). The slot is taken before anything is created and
        // is given back on every failure below, or by evt_stream_cleanup.
        let adev = acx_device_ctx(device.cast());
        let slots: *const StreamSlots = if adev.is_null() { core::ptr::null() } else { &raw const (*adev).streams };
        if !slots.is_null() && !(*slots).try_acquire(rt_limits::MAX_STREAMS) {
            trace!("acx: stream refused: {} streams exist already", rt_limits::MAX_STREAMS);
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        let give_back = |status: NTSTATUS| {
            if !slots.is_null() {
                (*slots).release();
            }
            status
        };

        // ACX_STREAM_CALLBACKS_INIT
        let mut callbacks = ACX_STREAM_CALLBACKS {
            Size: size_of::<ACX_STREAM_CALLBACKS>() as ULONG,
            EvtAcxStreamPrepareHardware: Some(evt_stream_prepare_hardware),
            EvtAcxStreamReleaseHardware: Some(evt_stream_release_hardware),
            EvtAcxStreamRun: Some(evt_stream_run),
            EvtAcxStreamPause: Some(evt_stream_pause),
            EvtAcxStreamAssignDrmContentId: None,
        };
        let status = acx!(
            AcxStreamInitAssignAcxStreamCallbacksTableIndex,
            PFN_ACXSTREAMINITASSIGNACXSTREAMCALLBACKS,
            stream_init,
            &raw mut callbacks
        );
        if !nt_success(status) {
            trace!("acx: AcxStreamInitAssignAcxStreamCallbacks failed {:#x}", status);
            return give_back(status);
        }
        // ACX_RT_STREAM_CALLBACKS_INIT
        let mut rt = ACX_RT_STREAM_CALLBACKS {
            Size: size_of::<ACX_RT_STREAM_CALLBACKS>() as ULONG,
            EvtAcxStreamGetHwLatency: Some(evt_stream_get_hw_latency),
            EvtAcxStreamAllocateRtPackets: Some(evt_stream_allocate_rt_packets),
            EvtAcxStreamFreeRtPackets: Some(evt_stream_free_rt_packets),
            EvtAcxStreamSetRenderPacket: Some(evt_stream_set_render_packet),
            EvtAcxStreamGetCurrentPacket: Some(evt_stream_get_current_packet),
            EvtAcxStreamGetCapturePacket: None,
            EvtAcxStreamGetPresentationPosition: Some(evt_stream_get_presentation_position),
        };
        let status = acx!(
            AcxStreamInitAssignAcxRtStreamCallbacksTableIndex,
            PFN_ACXSTREAMINITASSIGNACXRTSTREAMCALLBACKS,
            stream_init,
            &raw mut rt
        );
        if !nt_success(status) {
            trace!("acx: AcxStreamInitAssignAcxRtStreamCallbacks failed {:#x}", status);
            return give_back(status);
        }
        acx!(
            AcxStreamInitSetAcxRtStreamSupportsNotificationsTableIndex,
            PFN_ACXSTREAMINITSETACXRTSTREAMSUPPORTSNOTIFICATIONS,
            stream_init
        );

        let mut attr = attributes();
        attr.ContextTypeInfo = &raw const STREAM_CTX_INFO.0;
        attr.EvtCleanupCallback = Some(evt_stream_cleanup);
        attr.EvtDestroyCallback = Some(evt_stream_destroy);
        let mut stream: ACXSTREAM = null_mut();
        let status = acx!(
            AcxRtStreamCreateTableIndex,
            PFN_ACXRTSTREAMCREATE,
            device,
            circuit,
            &raw mut attr,
            &raw mut stream_init,
            &raw mut stream
        );
        if !nt_success(status) {
            trace!("acx: AcxRtStreamCreate failed {:#x}", status);
            return give_back(status);
        }

        let s = stream_ctx(stream.cast());
        (*s).stream = stream;
        // From here on the stream object owns the slot. Nothing below
        // fails; if a failure is ever added, the stream's cleanup callback
        // gives the slot back, not `give_back`.
        (*s).slots = slots;
        let mut freq: LARGE_INTEGER = zeroed();
        KeQueryPerformanceCounter(&raw mut freq);
        (*s).qpc_freq = (freq.QuadPart as u64).max(1);
        (*s).sample_rate = acx!(AcxDataFormatGetSampleRateTableIndex, PFN_ACXDATAFORMATGETSAMPLERATE, format);
        (*s).bits = acx!(AcxDataFormatGetBitsPerSampleTableIndex, PFN_ACXDATAFORMATGETBITSPERSAMPLE, format);
        (*s).channels = acx!(AcxDataFormatGetChannelsCountTableIndex, PFN_ACXDATAFORMATGETCHANNELSCOUNT, format) as u32;
        (*s).block_align = acx!(AcxDataFormatGetBlockAlignTableIndex, PFN_ACXDATAFORMATGETBLOCKALIGN, format).max(1);
        (*s).bytes_per_sec =
            acx!(AcxDataFormatGetAverageBytesPerSecTableIndex, PFN_ACXDATAFORMATGETAVERAGEBYTESPERSEC, format).max(1);
        let valid_bits =
            acx!(AcxDataFormatGetValidBitsPerSampleTableIndex, PFN_ACXDATAFORMATGETVALIDBITSPERSAMPLE, format);
        // The stream format in the audio contract's terms. A field that does
        // not fit becomes 0, which makes the format invalid: such a stream
        // (none is offered) still plays, into nothing.
        let fmt = PcmFormat {
            sample_rate: (*s).sample_rate,
            channels: u8::try_from((*s).channels).unwrap_or(0),
            container_bits: u8::try_from((*s).bits).unwrap_or(0),
            valid_bits: u8::try_from(valid_bits).unwrap_or(0),
        };
        (*s).dev = crate::device_ctx(device.cast());
        (*s).ring_bytes = ring_bytes(&fmt);
        (*s).known = fmt.is_valid()
            && fmt.frame_bytes() == (*s).block_align as usize
            && audio::stream_created((*s).dev, s as usize, fmt);
        trace!(
            "acx: stream created: {} mode, {} Hz, {} ch, {} valid bits in {}-bit containers, block {} bytes, \
             tap {}, {} of {} streams",
            mode_name(signal_processing_mode),
            (*s).sample_rate,
            (*s).channels,
            valid_bits,
            (*s).bits,
            (*s).block_align,
            if !(*s).known {
                "never"
            } else if audio::feeds_tap((*s).dev, s as usize) {
                "yes"
            } else {
                "not now"
            },
            if slots.is_null() { 0 } else { (*slots).in_use() },
            rt_limits::MAX_STREAMS
        );
        STATUS_SUCCESS
    }
}

/// The ACXSTREAM is being deleted (IRQL <= DISPATCH): the endpoint has no
/// stream any more, as far as the audio contract is concerned.
unsafe extern "C" fn evt_stream_cleanup(obj: WDFOBJECT) {
    unsafe {
        let s = stream_ctx(obj);
        if !s.is_null() && !(*s).dev.is_null() && (*s).known {
            audio::stream_destroyed((*s).dev, s as usize);
        }
        // Give the stream slot back (WDF calls a cleanup callback once).
        if !s.is_null() && !(*s).slots.is_null() {
            let slots = (*s).slots;
            (*s).slots = core::ptr::null();
            (*slots).release();
        }
    }
}

/// Last reference to the ACXSTREAM is gone (IRQL <= DISPATCH); its children,
/// the timer included, are deleted. Normally ReleaseHardware has freed the
/// ring already; this covers a stream that is torn down without it.
///
/// `free_ring` takes the device's audio spinlock through `(*s).dev`, at a
/// time WDF does not pin down. That is safe only because (crash review F8)
/// the ring is normally gone already, so `free_ring` returns before it
/// touches the device, and because a stream is a descendant of the device
/// and is destroyed before the device context is. Do not hold a reference
/// on an ACXSTREAM past the life of its device.
unsafe extern "C" fn evt_stream_destroy(obj: WDFOBJECT) {
    unsafe {
        let s = stream_ctx(obj);
        if !s.is_null() {
            free_ring(s);
        }
    }
}

/// Take the ring storage away from the tap and free it. After
/// [`audio::ring_remove`] returns, no timer pass and no reader touches the
/// memory any more (both only reach it under the audio lock, through the
/// pointer `ring_remove` clears), so the free is safe even if a pass is
/// still running. IRQL <= DISPATCH.
unsafe fn free_ring(s: *mut StreamCtx) {
    unsafe {
        let ring = (*s).ring;
        if ring.is_null() {
            return;
        }
        (*s).ring = null_mut();
        if !(*s).dev.is_null() {
            audio::ring_remove((*s).dev, s as usize, ring);
        }
        ExFreePool(ring.cast());
    }
}

/// QPC -> 100 ns units (KSCONVERT_PERFORMANCE_TIME).
unsafe fn now_hns(s: *const StreamCtx) -> u64 {
    unsafe {
        let t = KeQueryPerformanceCounter(null_mut()).QuadPart as u64;
        let f = (*s).qpc_freq;
        (t / f) * HNS_PER_SEC + ((t % f) * HNS_PER_SEC) / f
    }
}

/// Duration of `packets` packets in 100 ns units.
unsafe fn packets_hns(s: *const StreamCtx, packets: u64) -> u64 {
    unsafe {
        // Split so the product cannot overflow on a long-running stream.
        let one = (*s).packet_size as u64 * HNS_PER_SEC;
        let bps = (*s).bytes_per_sec as u64;
        packets * (one / bps) + packets * (one % bps) / bps
    }
}

unsafe extern "C" fn evt_stream_get_hw_latency(_stream: ACXSTREAM, fifo_size: *mut ULONG, delay: *mut ULONG) -> NTSTATUS {
    unsafe {
        *fifo_size = 128;
        *delay = 0;
    }
    STATUS_SUCCESS
}

/// ACX picks count and size. Each packet is its own page-rounded nonpaged
/// allocation described by an MDL (ACX maps it into the audio engine, hence
/// whole pages: nothing else may share them). Packet 0 is placed at the *end*
/// of its allocation, as in the sample.
unsafe extern "C" fn evt_stream_allocate_rt_packets(
    stream: ACXSTREAM,
    packet_count: ULONG,
    packet_size: ULONG,
    packets_out: *mut PACX_RTPACKET,
) -> NTSTATUS {
    unsafe {
        let s = stream_ctx(stream.cast());
        // Count and size come from the KS client; ACX passes them on with
        // only the count, a non-zero size and the block alignment checked.
        // The size has to hold between 1 ms and 5 s of this stream's audio
        // (rt_limits.rs).
        if !rt_limits::packet_ok(packet_count, MAX_PACKETS as u32, packet_size, (*s).bytes_per_sec) {
            let (min, max) = rt_limits::packet_bounds((*s).bytes_per_sec);
            trace!(
                "acx: AllocateRtPackets({}, {}) rejected: 1..={} packets of {}..={} bytes at {} bytes/s",
                packet_count,
                packet_size,
                MAX_PACKETS,
                min,
                max,
                (*s).bytes_per_sec
            );
            return STATUS_INVALID_PARAMETER;
        }
        // This callback writes the plain fields the timer DPC reads, with no
        // fence of its own. That is safe only through two things together
        // (crash review F6): evt_stream_run, the one place that arms the
        // timer, refuses to start without packets, and ACX (Acx01000.sys
        // 10.0.26100) refuses a second allocation without a free in between
        // (STATUS_ALREADY_INITIALIZED). So this runs only while there are
        // no packets, hence with the timer idle. Only reported here.
        if (*s).running.load(Ordering::SeqCst) || !(*s).packets[0].is_null() {
            trace!("acx: AllocateRtPackets while packets exist or the stream runs (unexpected; ACX is expected to refuse it)");
        }
        let page = PAGE_SIZE as usize;
        let alloc_size = (packet_size as usize).div_ceil(page) * page;
        // Two packets (event-driven client): packet 0 ends on a page boundary.
        // One packet (timer-driven client): ACX maps it twice back to back, so
        // Microsoft requires a page-aligned, page-divisible buffer (pool
        // allocations of a page or more are page-aligned) and offset 0. The
        // sample has no such case.
        let first_offset = if packet_count == 1 { 0 } else { (alloc_size - packet_size as usize) as u32 };
        if packet_count == 1 && !(packet_size as usize).is_multiple_of(page) {
            trace!("acx: single RT packet of {} bytes is not a page multiple (offset 0 used)", packet_size);
        }

        // ExAllocatePool2 zeroes the memory.
        let array = ExAllocatePool2(
            POOL_FLAG_NON_PAGED as POOL_FLAGS,
            (packet_count as usize * size_of::<ACX_RTPACKET>()) as SIZE_T,
            POOL_TAG,
        )
        .cast::<ACX_RTPACKET>();
        if array.is_null() {
            return STATUS_NO_MEMORY;
        }
        for i in 0..packet_count as usize {
            let p = array.add(i);
            // ACX_RTPACKET_INIT (the array is already zeroed)
            (*p).Size = size_of::<ACX_RTPACKET>() as ULONG;
            let buffer = ExAllocatePool2(POOL_FLAG_NON_PAGED as POOL_FLAGS, alloc_size as SIZE_T, POOL_TAG);
            let mdl = if buffer.is_null() { null_mut() } else { IoAllocateMdl(buffer, alloc_size as ULONG, 0, 1, null_mut()) };
            if mdl.is_null() {
                if !buffer.is_null() {
                    ExFreePool(buffer);
                }
                // Entries >= i have a null MDL and are skipped by the free.
                free_rt_packets(array, packet_count);
                (*s).packets = [null_mut(); MAX_PACKETS];
                return STATUS_NO_MEMORY;
            }
            MmBuildMdlForNonPagedPool(mdl);
            // WDF_MEMORY_DESCRIPTOR_INIT_MDL
            (*p).RtPacketBuffer.Type = _WDF_MEMORY_DESCRIPTOR_TYPE::WdfMemoryDescriptorTypeMdl;
            (*p).RtPacketBuffer.u.MdlType.Mdl = mdl;
            (*p).RtPacketBuffer.u.MdlType.BufferLength = alloc_size as ULONG;
            (*p).RtPacketSize = packet_size;
            (*p).RtPacketOffset = if i == 0 { first_offset } else { 0 };
            (*s).packets[i] = buffer.cast();
        }
        (*s).packet_count = packet_count;
        (*s).packet_size = packet_size;
        (*s).first_packet_offset = first_offset;
        *packets_out = array;
        trace!("acx: {} RT packet(s) of {} bytes ({} bytes allocated each)", packet_count, packet_size, alloc_size);
        STATUS_SUCCESS
    }
}

unsafe fn free_rt_packets(array: PACX_RTPACKET, count: ULONG) {
    unsafe {
        for i in 0..count as usize {
            let mdl = (*array.add(i)).RtPacketBuffer.u.MdlType.Mdl;
            if !mdl.is_null() {
                // MmGetMdlVirtualAddress
                let buffer = (*mdl).StartVa.cast::<u8>().add((*mdl).ByteOffset as usize);
                IoFreeMdl(mdl);
                ExFreePool(buffer.cast());
            }
        }
        ExFreePool(array.cast());
    }
}

unsafe extern "C" fn evt_stream_free_rt_packets(stream: ACXSTREAM, packets: PACX_RTPACKET, packet_count: ULONG) {
    unsafe {
        let s = stream_ctx(stream.cast());
        // Normally the timer is gone by now (ReleaseHardware), but Microsoft
        // documents that this ordering is not guaranteed. Stop the timer and
        // wait out a running pass before the buffers it reads go away (no-op
        // when the timer is deleted or idle; this callback is PASSIVE).
        // `(*s).timer` is only cleared by ReleaseHardware. If the stream is
        // deleted without it, the handle here is still good only because
        // ACX calls this callback from inside the stream's own cleanup pass
        // (crash review F5, Acx01000.sys 10.0.26100), where WDF has
        // disposed the child timer but not yet destroyed it. Do not call
        // this from anywhere else.
        stop_timer(s);
        (*s).packets = [null_mut(); MAX_PACKETS];
        (*s).packet_count = 0;
        if !packets.is_null() {
            free_rt_packets(packets, packet_count);
        }
        trace!("acx: RT packets freed");
    }
}

unsafe extern "C" fn evt_stream_prepare_hardware(stream: ACXSTREAM) -> NTSTATUS {
    unsafe {
        let s = stream_ctx(stream.cast());
        match (*s).state.load(Ordering::SeqCst) as i32 {
            _ACX_STREAM_STATE::AcxStreamStatePause => return STATUS_SUCCESS,
            _ACX_STREAM_STATE::AcxStreamStateStop => {}
            _ => return STATUS_INVALID_STATE_TRANSITION,
        }
        // One-shot timer, re-armed by every pass. No automatic serialisation:
        // the DPC only touches atomics and is fenced by `running`.
        let mut cfg = WDF_TIMER_CONFIG {
            Size: size_of::<WDF_TIMER_CONFIG>() as ULONG,
            EvtTimerFunc: Some(evt_stream_timer),
            Period: 0,
            AutomaticSerialization: 0,
            TolerableDelay: 0,
            UseHighResolutionTimer: 1,
            ..zeroed()
        };
        let mut attr = attributes();
        attr.ParentObject = stream.cast();
        let status = call_unsafe_wdf_function_binding!(WdfTimerCreate, &raw mut cfg, &raw mut attr, addr_of_mut!((*s).timer));
        if !nt_success(status) {
            trace!("acx: WdfTimerCreate failed {:#x}", status);
            (*s).timer = null_mut();
            return status;
        }
        // Ring for the PCM tap: RING_MS of this stream's format, nonpaged
        // (it is written by the timer DPC). Every known stream offers one,
        // also while another stream feeds the tap: the tap can move here
        // when this stream runs (audio_core.rs). Without it the stream still
        // plays; its audio is counted as dropped while it feeds the tap.
        if (*s).known && (*s).ring.is_null() && (*s).ring_bytes > 0 {
            let ring = ExAllocatePool2(POOL_FLAG_NON_PAGED as POOL_FLAGS, (*s).ring_bytes as SIZE_T, POOL_TAG).cast::<u8>();
            if ring.is_null() {
                trace!("acx: no memory for the {}-byte tap ring; this stream's audio is dropped", (*s).ring_bytes);
            } else if audio::ring_install((*s).dev, s as usize, ring, (*s).ring_bytes) {
                (*s).ring = ring;
            } else {
                // The session does not take it (cannot happen for a known
                // stream with a valid format); nothing refers to it.
                trace!("acx: tap ring not taken");
                ExFreePool(ring.cast());
            }
        }
        (*s).state.store(_ACX_STREAM_STATE::AcxStreamStatePause as u32, Ordering::SeqCst);
        trace!("acx: stream prepared (tap ring {} bytes)", if (*s).ring.is_null() { 0 } else { (*s).ring_bytes });
        STATUS_SUCCESS
    }
}

/// Stops the timer and waits for a running pass. PASSIVE only.
unsafe fn stop_timer(s: *mut StreamCtx) {
    unsafe {
        (*s).running.store(false, Ordering::SeqCst);
        if !(*s).timer.is_null() {
            let _ = call_unsafe_wdf_function_binding!(WdfTimerStop, (*s).timer, 1);
            // A pass that was already past its `running` check may have
            // re-armed the timer; the next pass would see `running == false`
            // and do nothing, but cancel it anyway.
            let _ = call_unsafe_wdf_function_binding!(WdfTimerStop, (*s).timer, 1);
        }
    }
}

unsafe extern "C" fn evt_stream_release_hardware(stream: ACXSTREAM) -> NTSTATUS {
    unsafe {
        let s = stream_ctx(stream.cast());
        if (*s).state.load(Ordering::SeqCst) as i32 == _ACX_STREAM_STATE::AcxStreamStateStop {
            return STATUS_SUCCESS;
        }
        stop_timer(s);
        if !(*s).timer.is_null() {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, (*s).timer.cast());
            (*s).timer = null_mut();
        }
        KeFlushQueuedDpcs();
        // The timer is stopped and deleted and no DPC is left: the ring can
        // go. What the reader had not taken yet is lost with it.
        free_ring(s);
        print_stats(s, "stopped");
        (*s).current_packet.store(0, Ordering::SeqCst);
        (*s).reported_bytes.store(0, Ordering::SeqCst);
        (*s).shift_hns.store(0, Ordering::SeqCst);
        (*s).state.store(_ACX_STREAM_STATE::AcxStreamStateStop as u32, Ordering::SeqCst);
        STATUS_SUCCESS
    }
}

unsafe extern "C" fn evt_stream_run(stream: ACXSTREAM) -> NTSTATUS {
    unsafe {
        let s = stream_ctx(stream.cast());
        match (*s).state.load(Ordering::SeqCst) as i32 {
            _ACX_STREAM_STATE::AcxStreamStateRun => return STATUS_SUCCESS,
            _ACX_STREAM_STATE::AcxStreamStatePause => {}
            _ => return STATUS_INVALID_STATE_TRANSITION,
        }
        if (*s).timer.is_null() || (*s).packet_count == 0 || (*s).packet_size == 0 {
            trace!("acx: Run without timer/packets");
            return STATUS_INVALID_DEVICE_STATE;
        }
        // Resume where Pause stopped (the sample's m_StartPosition): the part
        // of the current packet that was already played before the pause is
        // not played again. The timer is stopped, so nothing else moves these.
        let size = (*s).packet_size as u64;
        let current = (*s).current_packet.load(Ordering::SeqCst);
        // Saturating: `reported_bytes` is 0 on the first Run.
        let played = (*s).reported_bytes.load(Ordering::SeqCst).saturating_sub(current as u64 * size).min(size);
        let one = packets_hns(s, 1).max(1);
        // played <= packet_size <= 1 GiB, so the product fits.
        let played_hns = (played * HNS_PER_SEC / (*s).bytes_per_sec as u64).min(one);
        (*s).run_base_packet.store(current, Ordering::SeqCst);
        (*s).shift_hns.store(0, Ordering::SeqCst);
        // Backdate the start by the played part: position and packet
        // boundaries continue from the latched position.
        (*s).run_start_hns.store(now_hns(s).saturating_sub(played_hns), Ordering::SeqCst);
        (*s).runs.fetch_add(1, Ordering::Relaxed);
        (*s).state.store(_ACX_STREAM_STATE::AcxStreamStateRun as u32, Ordering::SeqCst);
        (*s).running.store(true, Ordering::SeqCst);
        // The current packet completes after its remaining part. It reaches
        // the tap then, whole and once: a packet is only ever copied when it
        // completes, so Pause/Run in the middle of one neither repeats nor
        // loses frames.
        let _ = call_unsafe_wdf_function_binding!(WdfTimerStart, (*s).timer, -((one - played_hns).max(1) as i64));
        // The tap follows the running stream: the last one to run takes it.
        let tap = (*s).known && audio::stream_running((*s).dev, s as usize, true);
        trace!("acx: stream running ({} Hz, feeds the tap: {})", (*s).sample_rate, tap);
        STATUS_SUCCESS
    }
}

unsafe extern "C" fn evt_stream_pause(stream: ACXSTREAM) -> NTSTATUS {
    unsafe {
        let s = stream_ctx(stream.cast());
        match (*s).state.load(Ordering::SeqCst) as i32 {
            _ACX_STREAM_STATE::AcxStreamStatePause => return STATUS_SUCCESS,
            _ACX_STREAM_STATE::AcxStreamStateRun => {}
            _ => return STATUS_INVALID_STATE_TRANSITION,
        }
        stop_timer(s);
        // Latch the position while the timeline is still valid (state is
        // still Run) and no pass can move `current_packet` any more. Run
        // resumes from this value.
        let _ = position_bytes(s);
        (*s).state.store(_ACX_STREAM_STATE::AcxStreamStatePause as u32, Ordering::SeqCst);
        // Nothing is flushed: frames already in the tap stay for the reader,
        // unless another stream is running; the tap then moves to that one.
        if (*s).known && !audio::stream_running((*s).dev, s as usize, false) {
            trace!("acx: stream paused; the tap is with another stream");
        }
        print_stats(s, "paused");
        STATUS_SUCCESS
    }
}

/// Bytes played so far: time-based inside the current packet, never past the
/// packet that is being played, never backwards.
unsafe fn position_bytes(s: *const StreamCtx) -> u64 {
    unsafe {
        let size = (*s).packet_size as u64;
        let done = (*s).current_packet.load(Ordering::SeqCst) as u64 * size;
        let mut pos = done;
        if (*s).state.load(Ordering::SeqCst) as i32 == _ACX_STREAM_STATE::AcxStreamStateRun {
            let start = (*s).run_start_hns.load(Ordering::SeqCst) + (*s).shift_hns.load(Ordering::SeqCst);
            let elapsed = now_hns(s).saturating_sub(start);
            let base = (*s).run_base_packet.load(Ordering::SeqCst) as u64 * size;
            let timed = base + elapsed.min(u64::MAX / (*s).bytes_per_sec as u64) * (*s).bytes_per_sec as u64 / HNS_PER_SEC;
            pos = timed.clamp(done, done + size);
        }
        pos -= pos % (*s).block_align as u64;
        (*s).reported_bytes.fetch_max(pos, Ordering::SeqCst).max(pos)
    }
}

unsafe extern "C" fn evt_stream_get_presentation_position(
    stream: ACXSTREAM,
    position_in_blocks: PULONGLONG,
    qpc_position: PULONGLONG,
) -> NTSTATUS {
    unsafe {
        let s = stream_ctx(stream.cast());
        *position_in_blocks = position_bytes(s) / (*s).block_align as u64;
        *qpc_position = KeQueryPerformanceCounter(null_mut()).QuadPart as u64;
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn evt_stream_get_current_packet(stream: ACXSTREAM, current_packet: PULONG) -> NTSTATUS {
    unsafe {
        *current_packet = (*stream_ctx(stream.cast())).current_packet.load(Ordering::SeqCst);
    }
    STATUS_SUCCESS
}

/// The audio engine announces that it finished writing `packet`. It must be
/// the one after the packet being played (same checks as the sample).
unsafe extern "C" fn evt_stream_set_render_packet(stream: ACXSTREAM, packet: ULONG, _flags: ULONG, _eos_packet_length: ULONG) -> NTSTATUS {
    unsafe {
        let s = stream_ctx(stream.cast());
        let current = (*s).current_packet.load(Ordering::SeqCst);
        if packet <= current {
            (*s).set_packet_late.fetch_add(1, Ordering::Relaxed);
            STATUS_DATA_LATE_ERROR
        } else if packet > current.wrapping_add(1) {
            (*s).set_packet_overrun.fetch_add(1, Ordering::Relaxed);
            STATUS_DATA_OVERRUN
        } else {
            STATUS_SUCCESS
        }
    }
}

/// Timer DPC (DISPATCH_LEVEL): the packet being played is finished. Consume
/// it, tell ACX, schedule the next boundary.
unsafe extern "C" fn evt_stream_timer(timer: WDFTIMER) {
    unsafe {
        let stream = call_unsafe_wdf_function_binding!(WdfTimerGetParentObject, timer);
        let s = stream_ctx(stream);
        if s.is_null() {
            return;
        }
        let mut burst = 0u32;
        loop {
            if !(*s).running.load(Ordering::SeqCst) {
                return;
            }
            // Before the packet counter moves and before ACX is told: once
            // the audio engine hears that this packet completed it starts
            // overwriting the same buffer with the packet after next.
            consume_packet(s, burst >= FRESH_PACKETS_PER_PASS);
            let completed = (*s).current_packet.fetch_add(1, Ordering::SeqCst);
            (*s).packets_done.fetch_add(1, Ordering::Relaxed);
            let qpc = KeQueryPerformanceCounter(null_mut()).QuadPart as u64;
            let _ = acx!(
                AcxRtStreamNotifyPacketCompleteTableIndex,
                PFN_ACXRTSTREAMNOTIFYPACKETCOMPLETE,
                (*s).stream,
                completed as u64,
                qpc
            );

            // When does the packet that just started end?
            let since_run = completed.wrapping_add(1).wrapping_sub((*s).run_base_packet.load(Ordering::SeqCst)) as u64;
            let due = (*s).run_start_hns.load(Ordering::SeqCst)
                + (*s).shift_hns.load(Ordering::SeqCst)
                + packets_hns(s, since_run + 1);
            let now = now_hns(s);
            if due > now {
                if (*s).running.load(Ordering::SeqCst) {
                    let _ = call_unsafe_wdf_function_binding!(WdfTimerStart, timer, -((due - now) as i64));
                }
                return;
            }
            // Late: that packet is already over too.
            let late = now - due;
            (*s).late_passes.fetch_add(1, Ordering::Relaxed);
            (*s).max_late_hns.fetch_max(late, Ordering::Relaxed);
            burst += 1;
            if burst >= MAX_CATCH_UP {
                // Too far behind (stall, debugger): stop catching up and move
                // the timeline so the next packet ends one duration from now.
                let one = packets_hns(s, 1).max(1);
                (*s).shift_hns.fetch_add(late + one, Ordering::SeqCst);
                (*s).timeline_shifts.fetch_add(1, Ordering::Relaxed);
                if (*s).running.load(Ordering::SeqCst) {
                    let _ = call_unsafe_wdf_function_binding!(WdfTimerStart, timer, -(one as i64));
                }
                return;
            }
        }
    }
}

/// The packet being played has finished: hand it to the PCM tap. DISPATCH.
///
/// Which packet holds valid audio now: packet number `current_packet`, in
/// buffer `current_packet % packet_count`. `current_packet` counts completed
/// packets, so it is also the number of the packet that was being played up
/// to this timer pass; the audio engine finished writing it one period
/// earlier (it writes packet N+1 while N plays and announces it with
/// SetRenderPacket, which is why that callback only accepts
/// `current_packet + 1`). This is the same choice as the sample's
/// `CRenderStreamEngine::ProcessPacket` (StreamEngine.cpp:
/// `m_Packets[m_CurrentPacket % m_PacketsCount]`, read before
/// `InterlockedIncrement(&m_CurrentPacket)` and before
/// `AcxRtStreamNotifyPacketComplete`). Packet 0 starts at
/// `first_packet_offset` in its allocation.
///
/// First packet after the first Run: packet 0, which the audio engine
/// pre-rolls while the stream is paused; if it did not, the buffer is still
/// the zeros it was allocated with. End of stream (SetRenderPacket's
/// EosPacketLength) is not tracked, like in the sample: a last, partly
/// filled packet is taken whole.
///
/// `stale`: this is the third or later packet of one late timer pass. The
/// audio engine cannot have written it (see [`FRESH_PACKETS_PER_PASS`]);
/// the buffer still holds the audio of two packets ago. The tap gets
/// silence of the same length instead, so its frame count keeps following
/// the endpoint's clock and old audio is not played twice.
unsafe fn consume_packet(s: *const StreamCtx, stale: bool) {
    unsafe {
        let count = (*s).packet_count;
        if count == 0 {
            return;
        }
        let index = ((*s).current_packet.load(Ordering::SeqCst) % count) as usize;
        let mut p = (*s).packets[index];
        if p.is_null() {
            return;
        }
        if index == 0 {
            p = p.add((*s).first_packet_offset as usize);
        }
        let size = (*s).packet_size as usize;
        if stale {
            (*s).stale_packets.fetch_add(1, Ordering::Relaxed);
        } else if core::slice::from_raw_parts(p, size).iter().any(|&b| b != 0) {
            (*s).nonsilent_packets.fetch_add(1, Ordering::Relaxed);
        }
        if (*s).known {
            audio::render((*s).dev, s as usize, p, size, stale);
        }
    }
}

unsafe fn print_stats(s: *const StreamCtx, what: &str) {
    unsafe {
        let packets = (*s).packets_done.load(Ordering::Relaxed);
        let frames = packets * ((*s).packet_size / (*s).block_align) as u64;
        trace!(
            "acx: stream {}: {} Hz/{}-bit, runs {}, packets {} ({} with signal), frames {}",
            what,
            (*s).sample_rate,
            (*s).bits,
            (*s).runs.load(Ordering::Relaxed),
            packets,
            (*s).nonsilent_packets.load(Ordering::Relaxed),
            frames
        );
        trace!(
            "acx: stream {}: late passes {} (max {} us), timeline shifts {}, SetRenderPacket late {} / overrun {}",
            what,
            (*s).late_passes.load(Ordering::Relaxed),
            (*s).max_late_hns.load(Ordering::Relaxed) / 10,
            (*s).timeline_shifts.load(Ordering::Relaxed),
            (*s).set_packet_late.load(Ordering::Relaxed),
            (*s).set_packet_overrun.load(Ordering::Relaxed)
        );
        if !(*s).dev.is_null() {
            let (state, silence) = audio::stats((*s).dev);
            trace!(
                "acx: tap (device totals): rendered {} frames, dropped {}, silence for stale packets {} ({} packets of this stream), attached {}, link {}",
                state.frames_rendered,
                state.frames_dropped,
                silence,
                (*s).stale_packets.load(Ordering::Relaxed),
                state.attached(),
                state.link()
            );
        }
    }
}
