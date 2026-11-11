//! Transport abstraction so the AVDTP logic can be tested without a device.

use std::time::Duration;

/// Errors from the channel layer.
#[derive(Debug)]
pub enum ChanError {
    Timeout,
    /// The remote closed the L2CAP channel (driver: STATUS_CONNECTION_DISCONNECTED).
    Disconnected,
    /// Anything else (Win32 error code + context).
    Os {
        code: u32,
        what: &'static str,
    },
    Protocol(String),
}

impl std::fmt::Display for ChanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChanError::Timeout => f.write_str("timed out"),
            ChanError::Disconnected => f.write_str("remote disconnected the channel"),
            ChanError::Os { code, what } => write!(
                f,
                "{what} failed: Win32 error {code} ({})",
                win32_hint(*code)
            ),
            ChanError::Protocol(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for ChanError {}

/// Human hints for the Win32 errors the driver's NTSTATUSes map to.
pub fn win32_hint(code: u32) -> &'static str {
    match code {
        2 => "device interface not found",
        5 => "access denied - run elevated",
        6 => "invalid/stale channel id",
        21 => "device not ready (not in D0 or remote address unknown)",
        64 => "remote disconnected",
        87 => "invalid parameter",
        121 => {
            "timeout (L2CAP connect timed out, or on CLOSE: driver gave up after 8 s and the slot frees itself later)"
        }
        122 => "buffer too small",
        995 => "operation aborted (cancelled)",
        1167 => "device not connected",
        1450 => "no free channel slot / resources",
        1236 => "connection aborted by local system",
        1225 => "connection refused by remote",
        _ => "see winerror.h",
    }
}

/// One L2CAP channel: SDU in, SDU out.
pub trait SduChannel {
    fn send(&mut self, sdu: &[u8]) -> Result<(), ChanError>;
    fn recv(&mut self, timeout: Duration) -> Result<Vec<u8>, ChanError>;
    /// Peer's MTU for our outbound SDUs.
    fn out_mtu(&self) -> usize;
}
