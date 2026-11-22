//! The one SPA pod this codec builds: a raw audio `Format` object. The SPA
//! builder functions are `static inline` in C, so the bytes are laid out here
//! and appended to the caller's builder the way `spa_pod_builder_raw` does.

use core::ffi::c_void;
use core::ptr;

use crate::sys;

const ALIGN: usize = 8;
const ENOSPC: i32 = 28;

struct Writer(Vec<u8>);

impl Writer {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_ne_bytes());
    }
    fn pad(&mut self) {
        self.0.resize(self.0.len().next_multiple_of(ALIGN), 0);
    }
    fn id(&mut self, v: u32) {
        self.u32(4);
        self.u32(sys::SPA_TYPE_Id);
        self.u32(v);
        self.pad();
    }
    fn int(&mut self, v: i32) {
        self.u32(4);
        self.u32(sys::SPA_TYPE_Int);
        self.u32(v as u32);
        self.pad();
    }
    /// Enum choice of ids; the first value is the default.
    fn id_enum(&mut self, default: u32, alternatives: &[u32]) {
        let n = 1 + alternatives.len() as u32;
        self.u32(16 + 4 * n);
        self.u32(sys::SPA_TYPE_Choice);
        self.u32(sys::SPA_CHOICE_Enum);
        self.u32(0);
        self.u32(4);
        self.u32(sys::SPA_TYPE_Id);
        self.u32(default);
        alternatives.iter().for_each(|&v| self.u32(v));
        self.pad();
    }
    fn id_array(&mut self, values: &[u32]) {
        self.u32(8 + 4 * values.len() as u32);
        self.u32(sys::SPA_TYPE_Array);
        self.u32(4);
        self.u32(sys::SPA_TYPE_Id);
        values.iter().for_each(|&v| self.u32(v));
        self.pad();
    }
    fn prop(&mut self, key: u32, value: impl FnOnce(&mut Self)) {
        self.u32(key);
        self.u32(0);
        value(self);
    }
}

pub struct AudioRaw<'a> {
    /// The first is the preferred one.
    pub formats: &'a [u32],
    pub rate: u32,
    pub positions: &'a [u32],
}

pub fn format_object(id: u32, f: &AudioRaw<'_>) -> Vec<u8> {
    let mut w = Writer(Vec::with_capacity(256));
    w.u32(0); // size, set below
    w.u32(sys::SPA_TYPE_Object);
    w.u32(sys::SPA_TYPE_OBJECT_Format);
    w.u32(id);
    w.prop(sys::SPA_FORMAT_mediaType, |w| {
        w.id(sys::SPA_MEDIA_TYPE_audio)
    });
    w.prop(sys::SPA_FORMAT_mediaSubtype, |w| {
        w.id(sys::SPA_MEDIA_SUBTYPE_raw)
    });
    w.prop(sys::SPA_FORMAT_AUDIO_format, |w| match f.formats {
        [only] => w.id(*only),
        [first, ..] => w.id_enum(*first, f.formats),
        [] => unreachable!("a format list is never empty"),
    });
    w.prop(sys::SPA_FORMAT_AUDIO_rate, |w| w.int(f.rate as i32));
    w.prop(sys::SPA_FORMAT_AUDIO_channels, |w| {
        w.int(f.positions.len() as i32)
    });
    w.prop(sys::SPA_FORMAT_AUDIO_position, |w| w.id_array(f.positions));
    let body = (w.0.len() - 8) as u32;
    w.0[..4].copy_from_slice(&body.to_ne_bytes());
    w.0
}

/// Appends `pod` (whole, already padded) to `b` and returns it in the
/// builder's memory, or null if the builder had no room.
///
/// The builder is only touched through the raw pointer: its overflow callback
/// (PipeWire's dynamic builder) reallocates `data` and grows `size` through its
/// own alias, so both are read again after it returns.
///
/// # Safety
/// `b` is null or a valid, initialised `spa_pod_builder`.
pub unsafe fn append(b: *mut sys::spa_pod_builder, pod: &[u8]) -> *mut c_void {
    if b.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        let offset = (*b).state.offset;
        let size = pod.len() as u32;
        let total = u64::from(offset) + u64::from(size);
        if total > u64::from(u32::MAX) {
            return ptr::null_mut();
        }
        let mut res = 0;
        if total > u64::from((*b).size) {
            res = -ENOSPC;
            let cb = (*b).callbacks.funcs as *const sys::spa_pod_builder_callbacks;
            if offset <= (*b).size
                && let Some(cb) = cb.as_ref()
                && let Some(overflow) = cb.overflow
            {
                res = overflow((*b).callbacks.data, total as u32);
            }
        }
        let data = (*b).data as *mut u8;
        let fits = res == 0 && total <= u64::from((*b).size) && !data.is_null();
        if fits {
            ptr::copy_nonoverlapping(pod.as_ptr(), data.add(offset as usize), pod.len());
        }
        (*b).state.offset = total as u32;
        let mut f = (*b).state.frame;
        while !f.is_null() {
            (*f).pod.size += size;
            f = (*f).parent;
        }
        if fits {
            data.add(offset as usize) as *mut c_void
        } else {
            ptr::null_mut()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(b: &[u8]) -> Vec<u32> {
        b.chunks(4)
            .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
            .collect()
    }

    #[test]
    fn single_format_object_layout() {
        let pod = format_object(
            7,
            &AudioRaw {
                formats: &[sys::SPA_AUDIO_FORMAT_S16],
                rate: 48000,
                positions: &[3, 4],
            },
        );
        assert_eq!(pod.len() % 8, 0);
        let w = words(&pod);
        assert_eq!(
            w[0] as usize,
            pod.len() - 8,
            "object size covers the padded body"
        );
        assert_eq!(
            &w[1..4],
            &[sys::SPA_TYPE_Object, sys::SPA_TYPE_OBJECT_Format, 7]
        );
        // mediaType prop: key, flags, Id pod (size 4), value, pad.
        assert_eq!(
            &w[4..10],
            &[
                sys::SPA_FORMAT_mediaType,
                0,
                4,
                sys::SPA_TYPE_Id,
                sys::SPA_MEDIA_TYPE_audio,
                0
            ]
        );
        // position array is last: size 8 + 2*4, child Id, FL, FR.
        assert_eq!(
            &w[w.len() - 6..],
            &[16, sys::SPA_TYPE_Array, 4, sys::SPA_TYPE_Id, 3, 4]
        );
    }

    #[test]
    fn append_writes_into_builder_and_grows_frames() {
        let pod = format_object(
            3,
            &AudioRaw {
                formats: &[1, 2],
                rate: 96000,
                positions: &[3, 4],
            },
        );
        let mut buf = vec![0u64; 64];
        let mut frame: sys::spa_pod_frame = Default::default();
        let mut b = sys::spa_pod_builder {
            data: buf.as_mut_ptr() as *mut c_void,
            size: 512,
            ..Default::default()
        };
        b.state.frame = &mut frame;
        let p = unsafe { append(&mut b, &pod) };
        assert_eq!(p, buf.as_mut_ptr() as *mut c_void);
        assert_eq!(b.state.offset as usize, pod.len());
        assert_eq!(frame.pod.size as usize, pod.len());
        let mut small = sys::spa_pod_builder {
            data: buf.as_mut_ptr() as *mut c_void,
            size: 16,
            ..Default::default()
        };
        assert!(unsafe { append(&mut small, &pod) }.is_null());
    }

    #[repr(C)]
    struct Dynamic {
        b: sys::spa_pod_builder,
        store: Vec<u64>,
    }

    unsafe extern "C" fn grow(data: *mut c_void, size: u32) -> i32 {
        let d = data as *mut Dynamic;
        unsafe {
            (*d).store = vec![0u64; (size as usize).div_ceil(8)];
            (*d).b.data = (*d).store.as_mut_ptr() as *mut c_void;
            (*d).b.size = size;
        }
        0
    }

    #[test]
    fn append_follows_a_reallocating_overflow_callback() {
        static CALLBACKS: sys::spa_pod_builder_callbacks = sys::spa_pod_builder_callbacks {
            version: 0,
            overflow: Some(grow),
        };
        let pod = format_object(
            3,
            &AudioRaw {
                formats: &[1],
                rate: 48000,
                positions: &[3, 4],
            },
        );
        let mut d = Box::new(Dynamic {
            b: Default::default(),
            store: vec![0u64; 2],
        });
        d.b.data = d.store.as_mut_ptr() as *mut c_void;
        d.b.size = 16;
        d.b.callbacks.funcs = &CALLBACKS as *const _ as *const c_void;
        d.b.callbacks.data = &mut *d as *mut Dynamic as *mut c_void;
        let p = unsafe { append(&raw mut d.b, &pod) };
        assert_eq!(
            p,
            d.store.as_mut_ptr() as *mut c_void,
            "written to the reallocated buffer"
        );
        assert_eq!(unsafe { slice_of(p, pod.len()) }, &pod[..]);
    }

    unsafe fn slice_of<'a>(p: *mut c_void, n: usize) -> &'a [u8] {
        unsafe { std::slice::from_raw_parts(p as *const u8, n) }
    }
}
