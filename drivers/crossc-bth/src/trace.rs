//! DbgPrintEx tracing with a fixed "crossc: " prefix.
//!
//! Uses component DPFLTR_IHVDRIVER_ID (77) at DPFLTR_ERROR_LEVEL (0), which the
//! kernel debug-print filter passes by default, so DebugView ("Capture Kernel")
//! shows it without editing the Debug Print Filter registry key. Formatting
//! happens into a 256-byte stack buffer (no allocation), so it is callable at
//! IRQL <= DISPATCH_LEVEL.

use core::fmt::{self, Write};

const DPFLTR_IHVDRIVER_ID: u32 = 77;
const DPFLTR_ERROR_LEVEL: u32 = 0;

struct Buf {
    b: [u8; 256],
    n: usize,
}

impl Write for Buf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &c in s.as_bytes() {
            // keep 2 bytes for "\n\0"; drop interior NULs
            if self.n + 2 >= self.b.len() {
                break;
            }
            if c != 0 {
                self.b[self.n] = c;
                self.n += 1;
            }
        }
        Ok(())
    }
}

pub fn print(args: fmt::Arguments) {
    let mut w = Buf { b: [0; 256], n: 0 };
    let _ = w.write_str("crossc: ");
    let _ = fmt::write(&mut w, args);
    w.b[w.n] = b'\n';
    w.b[w.n + 1] = 0;
    // SAFETY: NUL-terminated buffer, passed through "%s" so its contents are
    // never interpreted as a format string.
    unsafe {
        wdk_sys::ntddk::DbgPrintEx(DPFLTR_IHVDRIVER_ID, DPFLTR_ERROR_LEVEL, c"%s".as_ptr().cast(), w.b.as_ptr());
    }
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::trace::print(format_args!($($arg)*)) };
}
