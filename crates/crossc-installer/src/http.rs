//! HTTP GET through WinHTTP (the system's HTTP stack: its TLS, its certificate store, its proxy settings), one
//! request at a time and with nothing done behind the caller's back: redirects are not followed, no cookies are
//! kept and no credentials are answered. What to ask, with which headers and where to go after a redirect is
//! decided by `release` and `update`; this file only moves bytes.

use std::ffi::c_void;
use std::io::{self, Read};
use std::rc::Rc;
use std::time::Duration;

use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY,
    WINHTTP_DISABLE_AUTHENTICATION, WINHTTP_DISABLE_COOKIES, WINHTTP_DISABLE_REDIRECTS,
    WINHTTP_FLAG_SECURE, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3,
    WINHTTP_OPEN_REQUEST_FLAGS, WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_OPTION_SECURE_PROTOCOLS,
    WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_CUSTOM, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect,
    WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::release::Url;

/// What a server answered, with the body still to be read.
pub struct Response {
    pub status: u32,
    /// The `Location` header of a redirect.
    pub location: Option<String>,
    pub content_length: Option<u64>,
    /// GitHub's `X-RateLimit-Remaining` header.
    pub rate_remaining: Option<String>,
    pub body: Box<dyn Read>,
}

/// Something that can do one GET. `http::WinHttp` in the program; tests also use a scripted one.
pub trait Transport {
    /// Sends the request and returns the response whatever its status; it never follows a redirect. `Err` is a
    /// failure to get any response (no network, TLS failure, timeout).
    fn get(&self, url: &Url, headers: &[(&'static str, String)]) -> Result<Response, String>;
}

/// A WinHTTP handle, closed on drop.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

fn opened(handle: *mut c_void, what: &str) -> Result<Handle, String> {
    if handle.is_null() {
        Err(format!("{what}: {}", windows::core::Error::from_thread()))
    } else {
        Ok(Handle(handle))
    }
}

/// A WinHTTP session.
pub struct WinHttp {
    session: Rc<Handle>,
}

impl WinHttp {
    /// `timeout` applies to each step on its own (resolving the name, connecting, sending, and every wait for
    /// more data), not to a download as a whole. `direct`: do not look for a proxy (tests talk to localhost).
    pub fn new(timeout: Duration, direct: bool) -> Result<WinHttp, String> {
        let agent = HSTRING::from(concat!("crossc-installer/", env!("CARGO_PKG_VERSION")));
        let access = if direct {
            WINHTTP_ACCESS_TYPE_NO_PROXY
        } else {
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY
        };
        let session = opened(
            unsafe { WinHttpOpen(&agent, access, PCWSTR::null(), PCWSTR::null(), 0) },
            "cannot start WinHTTP",
        )?;
        let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        unsafe {
            WinHttpSetTimeouts(session.0, ms, ms, ms, ms)
                .map_err(|e| format!("cannot set the network timeouts: {e}"))?;
            // TLS 1.2 or newer. A system that does not know TLS 1.3 refuses the pair; then 1.2 alone.
            let both = WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3;
            if set_u32(session.0, WINHTTP_OPTION_SECURE_PROTOCOLS, both).is_err() {
                set_u32(
                    session.0,
                    WINHTTP_OPTION_SECURE_PROTOCOLS,
                    WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2,
                )
                .map_err(|e| format!("cannot require TLS 1.2: {e}"))?;
            }
        }
        Ok(WinHttp {
            session: Rc::new(session),
        })
    }
}

unsafe fn set_u32(handle: *mut c_void, option: u32, value: u32) -> windows::core::Result<()> {
    unsafe {
        WinHttpSetOption(
            Some(handle.cast_const()),
            option,
            Some(&value.to_ne_bytes()),
        )
    }
}

impl Transport for WinHttp {
    fn get(&self, url: &Url, headers: &[(&'static str, String)]) -> Result<Response, String> {
        let failed = |what: &str, e: windows::core::Error| {
            format!("{what} {}: {}", url.host, network_error_text(&e))
        };
        unsafe {
            let connect = opened(
                WinHttpConnect(
                    self.session.0,
                    &HSTRING::from(url.host.as_str()),
                    url.port,
                    0,
                ),
                "cannot open a connection",
            )?;
            let flags = if url.secure {
                WINHTTP_FLAG_SECURE
            } else {
                WINHTTP_OPEN_REQUEST_FLAGS(0)
            };
            let request = opened(
                WinHttpOpenRequest(
                    connect.0,
                    w!("GET"),
                    &HSTRING::from(url.path.as_str()),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    std::ptr::null(),
                    flags,
                ),
                "cannot create the request",
            )?;
            // One call per feature: redirects are the caller's decision (host allow-list, no token on the next
            // hop), and neither cookies nor automatic logon have any business here.
            for feature in [
                WINHTTP_DISABLE_REDIRECTS,
                WINHTTP_DISABLE_COOKIES,
                WINHTTP_DISABLE_AUTHENTICATION,
            ] {
                set_u32(request.0, WINHTTP_OPTION_DISABLE_FEATURE, feature)
                    .map_err(|e| format!("cannot configure the request: {e}"))?;
            }
            let header_text: String = headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect();
            let header_text: Vec<u16> = header_text.encode_utf16().collect();
            WinHttpSendRequest(
                request.0,
                (!header_text.is_empty()).then_some(header_text.as_slice()),
                None,
                0,
                0,
                0,
            )
            .map_err(|e| failed("cannot reach", e))?;
            WinHttpReceiveResponse(request.0, std::ptr::null_mut())
                .map_err(|e| failed("no answer from", e))?;

            let mut status = 0u32;
            let mut size = size_of::<u32>() as u32;
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&raw mut status).cast()),
                &mut size,
                std::ptr::null_mut(),
            )
            .map_err(|e| failed("unreadable answer from", e))?;
            Ok(Response {
                status,
                location: header(request.0, WINHTTP_QUERY_LOCATION, None),
                content_length: header(request.0, WINHTTP_QUERY_CONTENT_LENGTH, None)
                    .and_then(|v| v.trim().parse().ok()),
                rate_remaining: header(
                    request.0,
                    WINHTTP_QUERY_CUSTOM,
                    Some("X-RateLimit-Remaining"),
                ),
                body: Box::new(Body {
                    request,
                    _connect: connect,
                    _session: self.session.clone(),
                }),
            })
        }
    }
}

/// One response header as text; `None` when the response does not have it.
unsafe fn header(request: *mut c_void, query: u32, name: Option<&str>) -> Option<String> {
    let name = name.map(HSTRING::from);
    let name_ptr = name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
    unsafe {
        // First call: how many bytes the value needs (the call fails with "insufficient buffer").
        let mut bytes = 0u32;
        let _ = WinHttpQueryHeaders(
            request,
            query,
            name_ptr,
            None,
            &mut bytes,
            std::ptr::null_mut(),
        );
        if bytes == 0 || bytes > 64 * 1024 {
            return None;
        }
        let mut buf = vec![0u16; (bytes as usize).div_ceil(2) + 1];
        let mut bytes = (buf.len() * 2) as u32;
        WinHttpQueryHeaders(
            request,
            query,
            name_ptr,
            Some(buf.as_mut_ptr().cast()),
            &mut bytes,
            std::ptr::null_mut(),
        )
        .ok()?;
        let len = (bytes as usize / 2).min(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

/// The response body. Keeps the handles it reads through alive (a request handle is only valid while its
/// connection and session are open).
struct Body {
    request: Handle,
    _connect: Handle,
    _session: Rc<Handle>,
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let want = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let mut got = 0u32;
        unsafe { WinHttpReadData(self.request.0, buf.as_mut_ptr().cast(), want, &mut got) }
            .map_err(|e| io::Error::other(network_error_text(&e)))?;
        Ok(got as usize)
    }
}

/// WinHTTP's own error codes have no message in the system table; name the ones a user can do something about.
fn network_error_text(e: &windows::core::Error) -> String {
    const FACILITY_WIN32: i32 = 0x8007_0000u32 as i32;
    let code = e.code().0;
    let win32 = if code & 0xFFFF_0000u32 as i32 == FACILITY_WIN32 {
        (code & 0xFFFF) as u32
    } else {
        0
    };
    match win32 {
        12002 => "the request timed out".into(),
        12007 => "the server name could not be resolved (no internet connection?)".into(),
        12029 => "could not connect (no internet connection, or a firewall is in the way)".into(),
        12030 | 12152 => "the connection was closed unexpectedly".into(),
        12157 => "the secure connection failed (TLS error)".into(),
        12175 => {
            "the server's TLS certificate was not accepted (wrong system clock, or something is intercepting \
             HTTPS)"
                .into()
        }
        12000..=12999 => format!("network error {win32}"),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winhttp_error_codes_are_named() {
        let error = |code: u32| {
            windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(code))
        };
        assert!(network_error_text(&error(12002)).contains("timed out"));
        assert!(network_error_text(&error(12007)).contains("could not be resolved"));
        assert!(network_error_text(&error(12029)).contains("could not connect"));
        assert!(network_error_text(&error(12175)).contains("certificate was not accepted"));
        assert_eq!(network_error_text(&error(12044)), "network error 12044");
        // An ordinary Win32 error keeps the system's message.
        assert!(!network_error_text(&error(5)).starts_with("network error"));
    }

    #[test]
    fn nothing_listens_is_an_error_not_a_hang() {
        // A port nobody listens on, found by binding and closing one.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let http = WinHttp::new(Duration::from_secs(5), true).unwrap();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/x")).unwrap();
        let started = std::time::Instant::now();
        let err = http.get(&url, &[]).err().expect("no server, no response");
        assert!(err.contains("cannot reach 127.0.0.1"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }
}
