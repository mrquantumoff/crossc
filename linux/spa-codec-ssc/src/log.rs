//! Logging through the host's `spa_log`, under this plugin's log topic.
//!
//! Bitrate changes are logged from PipeWire's real-time data thread, so a
//! message is formatted into a stack buffer, never the heap. PipeWire changes
//! log levels from its main thread while that thread reads them, so the level
//! fields are read atomically.

use core::ffi::{c_char, c_void};
use core::fmt::{self, Write};
use core::ptr;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use crate::slot;
use crate::sys;

const _: () = assert!(size_of::<sys::spa_log_level>() == size_of::<u32>());

static LOG: AtomicPtr<sys::spa_log> = AtomicPtr::new(ptr::null_mut());

/// The host writes the topic's level (through `spa_log_topic_enum`).
#[repr(transparent)]
pub struct Topic(UnsafeCell<sys::spa_log_topic>);
unsafe impl Sync for Topic {}

impl Topic {
    pub const fn get(&self) -> *mut sys::spa_log_topic {
        self.0.get()
    }
}

pub static TOPIC: Topic = Topic(UnsafeCell::new(sys::spa_log_topic {
    version: sys::SPA_VERSION_LOG_TOPIC,
    topic: slot::LOG_TOPIC.as_ptr(),
    level: sys::SPA_LOG_LEVEL_NONE,
    has_custom_level: false,
}));

/// # Safety
/// `log` is null or a `spa_log` that outlives the plugin, as the codec loader guarantees.
pub unsafe fn set(log: *mut sys::spa_log) {
    if !log.is_null() {
        unsafe {
            let methods = (*log).iface.cb.funcs as *const sys::spa_log_methods;
            if let Some(m) = methods.as_ref()
                && m.version >= 1
                && let Some(topic_init) = m.topic_init
            {
                topic_init((*log).iface.cb.data, TOPIC.get());
            }
        }
    }
    LOG.store(log, Ordering::Release);
}

unsafe fn load_u32(p: *const u32) -> u32 {
    unsafe { AtomicU32::from_ptr(p as *mut u32) }.load(Ordering::Relaxed)
}

pub fn enabled(level: sys::spa_log_level) -> bool {
    let log = LOG.load(Ordering::Acquire);
    if log.is_null() {
        return false;
    }
    unsafe {
        let topic = TOPIC.get();
        let custom =
            AtomicBool::from_ptr(&raw mut (*topic).has_custom_level).load(Ordering::Relaxed);
        let max = if custom {
            load_u32(&raw const (*topic).level)
        } else {
            load_u32(&raw const (*log).level)
        };
        level <= max
    }
}

/// A NUL-terminated message in a fixed buffer; longer messages are cut.
struct Line {
    buf: [u8; 256],
    len: usize,
}

impl Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.len + 1 >= self.buf.len() {
                break;
            }
            self.buf[self.len] = if b == 0 { b'?' } else { b };
            self.len += 1;
        }
        Ok(())
    }
}

pub fn emit(level: sys::spa_log_level, args: fmt::Arguments<'_>) {
    let log = LOG.load(Ordering::Acquire);
    if log.is_null() {
        return;
    }
    let mut line = Line {
        buf: [0; 256],
        len: 0,
    };
    let _ = line.write_fmt(args);
    line.buf[line.len] = 0;
    let (file, func, fmt): (*const c_char, *const c_char, *const c_char) = (
        c"a2dp-codec-ssc.rs".as_ptr(),
        c"ssc".as_ptr(),
        c"%s".as_ptr(),
    );
    unsafe {
        let Some(m) = ((*log).iface.cb.funcs as *const sys::spa_log_methods).as_ref() else {
            return;
        };
        let data: *mut c_void = (*log).iface.cb.data;
        let msg = line.buf.as_ptr() as *const c_char;
        if m.version >= 1
            && let Some(logt) = m.logt
        {
            logt(data, level, TOPIC.get(), file, 0, func, fmt, msg);
        } else if let Some(l) = m.log {
            l(data, level, file, 0, func, fmt, msg);
        }
    }
}

macro_rules! log_at {
    ($level:expr, $($arg:tt)*) => {
        if $crate::log::enabled($level) {
            $crate::log::emit($level, format_args!($($arg)*));
        }
    };
}
macro_rules! error { ($($arg:tt)*) => { $crate::log::log_at!($crate::sys::SPA_LOG_LEVEL_ERROR, $($arg)*) }; }
macro_rules! info { ($($arg:tt)*) => { $crate::log::log_at!($crate::sys::SPA_LOG_LEVEL_INFO, $($arg)*) }; }
pub(crate) use {error, info, log_at};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_lines_are_cut_and_terminated() {
        let mut line = Line {
            buf: [0; 256],
            len: 0,
        };
        let _ = write!(line, "{}\0{}", "a".repeat(10), "b".repeat(400));
        assert_eq!(line.len, 255);
        assert_eq!(
            line.buf[10], b'?',
            "an embedded NUL cannot end the message early"
        );
    }
}
