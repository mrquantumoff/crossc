//! The `struct media_codec` callbacks PipeWire's media sink drives.

use core::ffi::{CStr, c_char, c_int, c_void};
use core::ptr;
use core::slice;
use std::panic::{AssertUnwindSafe, catch_unwind};

use ssc_enc::{FRAME_SAMPLES, PcmFrame};

use crate::log::{error, info};
use crate::pod;
use crate::slot;
use crate::ssc::{self, Abr, Preferences, Rung, SscMode};
use crate::sys;

const EINVAL: c_int = 22;
const ENOTSUP: c_int = 95;
const EIO: c_int = 5;
const ENOSPC: c_int = 28;

const CHANNELS: usize = 2;
const POSITIONS: [u32; CHANNELS] = [sys::SPA_AUDIO_CHANNEL_FL, sys::SPA_AUDIO_CHANNEL_FR];
const SAMPLES: usize = FRAME_SAMPLES * CHANNELS;

/// A panic must not unwind into PipeWire: it becomes an error return.
fn guard<R>(on_panic: R, f: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(on_panic)
}

unsafe fn bytes<'a>(p: *const c_void, len: usize) -> &'a [u8] {
    if p.is_null() {
        &[]
    } else {
        unsafe { slice::from_raw_parts(p as *const u8, len) }
    }
}

unsafe fn lookup<'a>(dict: *const sys::spa_dict, key: &CStr) -> Option<&'a str> {
    let d = unsafe { dict.as_ref() }?;
    if d.n_items == 0 || d.items.is_null() {
        return None;
    }
    let items = unsafe { slice::from_raw_parts(d.items, d.n_items as usize) };
    items
        .iter()
        .filter(|i| !i.key.is_null() && !i.value.is_null())
        .find(|i| unsafe { CStr::from_ptr(i.key) } == key)
        .and_then(|i| unsafe { CStr::from_ptr(i.value) }.to_str().ok())
}

fn atob(s: &str) -> bool {
    s == "true" || s == "1"
}

/// Writes the codec element, zero-filled up to `len`: PipeWire takes a
/// configuration only when it is as long as the capabilities it answers.
unsafe fn write_config(out: *mut u8, flags: u8, len: usize) -> c_int {
    if out.is_null() || !(ssc::ELEMENT_LEN..=sys::A2DP_MAX_CAPS_SIZE as usize).contains(&len) {
        return -EINVAL;
    }
    let e = ssc::element(flags);
    unsafe {
        ptr::copy_nonoverlapping(e.as_ptr(), out, e.len());
        ptr::write_bytes(out.add(e.len()), 0, len - e.len());
    }
    len as c_int
}

fn config_mode(config: &[u8]) -> Result<&'static SscMode, c_int> {
    let flags = ssc::parse_element(config).ok_or(-ENOTSUP)?;
    ssc::mode_from_config(flags).ok_or(-ENOTSUP)
}

fn sample_format(m: &SscMode) -> u32 {
    if m.hires {
        sys::SPA_AUDIO_FORMAT_S32
    } else {
        sys::SPA_AUDIO_FORMAT_S16
    }
}

unsafe extern "C" fn fill_caps(
    _codec: *const sys::media_codec,
    _flags: u32,
    _settings: *const sys::spa_dict,
    caps: *mut u8,
) -> c_int {
    guard(-EIO, || unsafe {
        write_config(caps, ssc::LOCAL_CAPS, ssc::ELEMENT_LEN)
    })
}

unsafe extern "C" fn select_config(
    _codec: *const sys::media_codec,
    _flags: u32,
    caps: *const c_void,
    caps_size: usize,
    info: *const sys::media_codec_audio_info,
    settings: *const sys::spa_dict,
    config: *mut u8,
    _config_data: *mut *mut c_void,
) -> c_int {
    guard(-EIO, || unsafe {
        let Some(remote) = ssc::parse_element(bytes(caps, caps_size)) else {
            return -ENOTSUP;
        };
        let rate = info
            .as_ref()
            .map_or(sys::A2DP_CODEC_DEFAULT_RATE, |i| i.rate);
        let prefs = Preferences {
            mode: lookup(settings, c"bluez5.a2dp.ssc.mode").and_then(ssc::mode_by_name),
            uhq_alt_config: lookup(settings, c"bluez5.a2dp.ssc.uhq-alt-config").is_some_and(atob),
        };
        match ssc::select_config(remote, rate, prefs) {
            Some(conf) => write_config(config, conf, caps_size),
            None => -ENOTSUP,
        }
    })
}

unsafe extern "C" fn enum_config(
    _codec: *const sys::media_codec,
    _flags: u32,
    caps: *const c_void,
    caps_size: usize,
    id: u32,
    idx: u32,
    builder: *mut sys::spa_pod_builder,
    param: *mut *mut sys::spa_pod,
) -> c_int {
    guard(-EIO, || unsafe {
        let m = match config_mode(bytes(caps, caps_size)) {
            Ok(m) => m,
            Err(e) => return e,
        };
        if idx > 0 {
            return 0;
        }
        if builder.is_null() || param.is_null() {
            return -EINVAL;
        }
        let formats: &[u32] = if m.hires {
            &[sys::SPA_AUDIO_FORMAT_S32, sys::SPA_AUDIO_FORMAT_S16]
        } else {
            &[sys::SPA_AUDIO_FORMAT_S16]
        };
        let obj = pod::format_object(
            id,
            &pod::AudioRaw {
                formats,
                rate: m.rate(),
                positions: &POSITIONS,
            },
        );
        let p = pod::append(builder, &obj);
        *param = p as *mut sys::spa_pod;
        if p.is_null() { -EIO } else { 1 }
    })
}

unsafe extern "C" fn validate_config(
    _codec: *const sys::media_codec,
    _flags: u32,
    caps: *const c_void,
    caps_size: usize,
    info: *mut sys::spa_audio_info,
) -> c_int {
    guard(-EIO, || unsafe {
        let m = match config_mode(bytes(caps, caps_size)) {
            Ok(m) => m,
            Err(e) => return e,
        };
        if let Some(info) = info.as_mut() {
            *info = core::mem::zeroed();
            info.media_type = sys::SPA_MEDIA_TYPE_audio;
            info.media_subtype = sys::SPA_MEDIA_SUBTYPE_raw;
            let raw = &mut info.info.raw;
            raw.format = sample_format(m);
            raw.rate = m.rate();
            raw.channels = CHANNELS as u32;
            raw.position[..CHANNELS].copy_from_slice(&POSITIONS);
        }
        0
    })
}

struct Props {
    max_bps: Option<u32>,
    adaptive: bool,
}

unsafe extern "C" fn init_props(
    _codec: *const sys::media_codec,
    _flags: u32,
    settings: *const sys::spa_dict,
) -> *mut c_void {
    guard(ptr::null_mut(), || unsafe {
        let props = Props {
            max_bps: lookup(settings, c"bluez5.a2dp.ssc.max-bitrate")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .map(|kbps| kbps.saturating_mul(1000)),
            adaptive: lookup(settings, c"bluez5.a2dp.ssc.adaptive").is_none_or(atob),
        };
        Box::into_raw(Box::new(props)) as *mut c_void
    })
}

unsafe extern "C" fn clear_props(props: *mut c_void) {
    if !props.is_null() {
        drop(unsafe { Box::from_raw(props as *mut Props) });
    }
}

enum Input {
    S32(Box<[i32; SAMPLES]>),
    S16(Box<[i16; SAMPLES]>),
}

struct Stream {
    mode: &'static SscMode,
    input: Input,
    enc: Box<ssc_enc::Encoder>,
    abr: Abr,
    /// Rate of the packet being built; a change waits for the next packet.
    rung: Rung,
    packets: u32,
    frames_in_packet: u8,
}

impl Stream {
    fn block_size(&self) -> usize {
        SAMPLES
            * if matches!(self.input, Input::S32(_)) {
                4
            } else {
                2
            }
    }
}

unsafe extern "C" fn init(
    _codec: *const sys::media_codec,
    _flags: u32,
    config: *mut c_void,
    config_size: usize,
    info: *const sys::spa_audio_info,
    props: *mut c_void,
    mtu: usize,
) -> *mut c_void {
    guard(ptr::null_mut(), || unsafe {
        let m = match config_mode(bytes(config, config_size)) {
            Ok(m) => m,
            Err(_) => return ptr::null_mut(),
        };
        let Some(info) = info.as_ref() else {
            return ptr::null_mut();
        };
        let raw = &info.info.raw;
        if info.media_type != sys::SPA_MEDIA_TYPE_audio
            || info.media_subtype != sys::SPA_MEDIA_SUBTYPE_raw
            || raw.rate != m.rate()
            || raw.channels != CHANNELS as u32
        {
            error!(
                "SSC {}: unexpected stream format {} Hz, {} channels",
                m.name, raw.rate, raw.channels
            );
            return ptr::null_mut();
        }
        let input = match raw.format {
            sys::SPA_AUDIO_FORMAT_S32 => Input::S32(Box::new([0; SAMPLES])),
            sys::SPA_AUDIO_FORMAT_S16 => Input::S16(Box::new([0; SAMPLES])),
            _ => return ptr::null_mut(),
        };
        let props = (props as *const Props).as_ref();
        let adaptive = props.is_none_or(|p| p.adaptive);
        let rungs = ssc::ladder(m, mtu, props.and_then(|p| p.max_bps));
        let Some(abr) = Abr::new(rungs, adaptive) else {
            error!("SSC {}: no bitrate fits the media MTU {mtu}", m.name);
            return ptr::null_mut();
        };
        let Ok(enc) = ssc_enc::Encoder::new_boxed(m.enc, CHANNELS) else {
            return ptr::null_mut();
        };
        let s = Stream {
            mode: m,
            input,
            enc,
            rung: abr.current(),
            abr,
            packets: 0,
            frames_in_packet: 0,
        };
        info!(
            "SSC {} {} Hz, {}-bit input, {} kb/s{} (mtu {mtu}, plugin slot {})",
            m.name,
            m.rate(),
            if matches!(s.input, Input::S32(_)) {
                32
            } else {
                16
            },
            s.abr.top().bitrate.bps() / 1000,
            if s.abr.is_adaptive() { " adaptive" } else { "" },
            slot::SLOT,
        );
        Box::into_raw(Box::new(s)) as *mut c_void
    })
}

unsafe extern "C" fn deinit(data: *mut c_void) {
    if !data.is_null() {
        drop(unsafe { Box::from_raw(data as *mut Stream) });
    }
}

unsafe fn stream<'a>(data: *mut c_void) -> Option<&'a mut Stream> {
    unsafe { (data as *mut Stream).as_mut() }
}

unsafe extern "C" fn get_block_size(data: *mut c_void) -> c_int {
    unsafe { stream(data) }.map_or(-EINVAL, |s| s.block_size() as c_int)
}

fn kbps(r: Rung) -> c_int {
    (r.bitrate.bps() / 1000) as c_int
}

unsafe extern "C" fn reduce_bitpool(data: *mut c_void) -> c_int {
    guard(-EIO, || {
        let Some(s) = (unsafe { stream(data) }) else {
            return -EINVAL;
        };
        if let Some(r) = s.abr.congested() {
            info!("SSC {} bitrate down to {} kb/s", s.mode.name, kbps(r));
        }
        kbps(s.abr.current())
    })
}

unsafe extern "C" fn increase_bitpool(data: *mut c_void) -> c_int {
    guard(-EIO, || {
        let Some(s) = (unsafe { stream(data) }) else {
            return -EINVAL;
        };
        if let Some(r) = s.abr.clean() {
            info!("SSC {} bitrate up to {} kb/s", s.mode.name, kbps(r));
        }
        kbps(s.abr.current())
    })
}

unsafe extern "C" fn start_encode(
    data: *mut c_void,
    dst: *mut c_void,
    dst_size: usize,
    seqnum: u16,
    _timestamp: u32,
) -> c_int {
    guard(-EIO, || unsafe {
        let Some(s) = stream(data) else {
            return -EINVAL;
        };
        if dst.is_null() || dst_size < ssc::RTP_HEADER_LEN {
            return -EINVAL;
        }
        s.rung = s.abr.current();
        s.frames_in_packet = 0;
        let h = ssc::rtp_header(seqnum, s.packets);
        s.packets = s.packets.wrapping_add(1);
        ptr::copy_nonoverlapping(h.as_ptr(), dst as *mut u8, h.len());
        h.len() as c_int
    })
}

unsafe extern "C" fn encode(
    data: *mut c_void,
    src: *const c_void,
    src_size: usize,
    dst: *mut c_void,
    dst_size: usize,
    dst_out: *mut usize,
    need_flush: *mut c_int,
) -> c_int {
    guard(-EIO, || unsafe {
        let Some(s) = stream(data) else {
            return -EINVAL;
        };
        if dst.is_null() || dst_out.is_null() || need_flush.is_null() {
            return -EINVAL;
        }
        *dst_out = 0;
        *need_flush = sys::NEED_FLUSH_NO as c_int;
        let block = s.block_size();
        if src.is_null() || src_size < block {
            return 0;
        }
        if dst_size < ssc::frame_bytes(s.mode, s.rung) {
            return -ENOSPC;
        }
        // media-sink may hand over its byte buffer, which is not aligned for i32.
        let src = bytes(src, block);
        let pcm = match &mut s.input {
            Input::S32(buf) => {
                for (d, c) in buf.iter_mut().zip(src.as_chunks::<4>().0) {
                    *d = i32::from_ne_bytes(*c);
                }
                PcmFrame::S32 {
                    samples: &buf[..],
                    bits: 32,
                }
            }
            Input::S16(buf) => {
                for (d, c) in buf.iter_mut().zip(src.as_chunks::<2>().0) {
                    *d = i16::from_ne_bytes(*c);
                }
                PcmFrame::I16(&buf[..])
            }
        };
        let out = slice::from_raw_parts_mut(dst as *mut u8, dst_size);
        match s.enc.encode_frame(pcm, s.rung.bitrate, out) {
            Ok(n) => *dst_out = n,
            Err(e) => {
                error!("SSC encoder: {e:?}");
                return -EIO;
            }
        }
        s.frames_in_packet += 1;
        if usize::from(s.frames_in_packet) >= usize::from(s.rung.frames_per_packet) {
            *need_flush = sys::NEED_FLUSH_ALL as c_int;
        }
        block as c_int
    })
}

unsafe extern "C" fn set_log(log: *mut sys::spa_log) {
    unsafe { crate::log::set(log) };
}

#[repr(transparent)]
pub struct Codec(pub sys::media_codec);
unsafe impl Sync for Codec {}

pub static CODEC: Codec = Codec(sys::media_codec {
    id: slot::CODEC_ENUM,
    kind: sys::MEDIA_CODEC_A2DP,
    codec_id: sys::A2DP_CODEC_VENDOR as u8,
    vendor: sys::a2dp_vendor_codec_t {
        vendor_id: ssc::VENDOR_ID,
        codec_id: ssc::CODEC_ID,
    },
    name: c"ssc".as_ptr(),
    description: c"SSC (Samsung Seamless)".as_ptr() as *const c_char,
    fill_caps: Some(fill_caps),
    select_config: Some(select_config),
    enum_config: Some(enum_config),
    validate_config: Some(validate_config),
    init_props: Some(init_props),
    clear_props: Some(clear_props),
    init: Some(init),
    deinit: Some(deinit),
    get_block_size: Some(get_block_size),
    start_encode: Some(start_encode),
    encode: Some(encode),
    reduce_bitpool: Some(reduce_bitpool),
    increase_bitpool: Some(increase_bitpool),
    set_log: Some(set_log),
    // SAFETY: every other field is a pointer, an Option of a function pointer, or an integer.
    ..unsafe { core::mem::zeroed() }
});
