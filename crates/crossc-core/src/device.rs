//! Windows side: find the crossc device interface, open it overlapped, and
//! issue IOCTLs with timeouts (CancelIoEx on expiry), or leave them pending
//! ([`Pending`]) where the driver completes them much later. On top of that:
//! L2CAP channels ([`Channel`]) and the audio IOCTLs.

use std::sync::Arc;
use std::time::Duration;

use crossc_ioctl as ioc;
use crossc_ioctl::audio;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CR_BUFFER_SMALL, CR_SUCCESS,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_NETNAME_DELETED, GENERIC_READ,
    GENERIC_WRITE, HANDLE, WAIT_TIMEOUT, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::{GUID, HSTRING, PCWSTR};

use crate::channel::{ChanError, SduChannel};
use crate::driver::PcmChunk;

fn guid() -> GUID {
    let g = ioc::DEVICE_INTERFACE_GUID;
    GUID {
        data1: g.data1,
        data2: g.data2,
        data3: g.data3,
        data4: g.data4,
    }
}

/// Split a REG_MULTI_SZ-style UTF-16 list.
pub fn split_multi_sz(buf: &[u16]) -> Vec<String> {
    buf.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// All present crossc device interfaces (one per bound A2DP node).
pub fn list_interfaces() -> Result<Vec<String>, ChanError> {
    let g = guid();
    for _ in 0..4 {
        let mut len = 0u32;
        let cr = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut len,
                &g,
                PCWSTR::null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if cr != CR_SUCCESS {
            return Err(ChanError::Os {
                code: cr.0,
                what: "CM_Get_Device_Interface_List_Size (CONFIGRET)",
            });
        }
        let mut buf = vec![0u16; len as usize + 1];
        let cr = unsafe {
            CM_Get_Device_Interface_ListW(
                &g,
                PCWSTR::null(),
                &mut buf,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if cr == CR_BUFFER_SMALL {
            continue; // list grew; retry
        }
        if cr != CR_SUCCESS {
            return Err(ChanError::Os {
                code: cr.0,
                what: "CM_Get_Device_Interface_List (CONFIGRET)",
            });
        }
        return Ok(split_multi_sz(&buf));
    }
    Err(ChanError::Protocol(
        "device interface list kept changing".into(),
    ))
}

pub(crate) struct OwnedHandle(pub(crate) HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}
// SAFETY: kernel handles are usable from any thread.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

/// An open handle to the driver. Channels opened through it die with it.
#[derive(Clone)]
pub struct Device {
    h: Arc<OwnedHandle>,
    pub path: String,
}

/// Whether a failed `GetOverlappedResultEx` means "not completed in time":
/// WAIT_TIMEOUT after a wait, ERROR_IO_INCOMPLETE for a zero wait (which a
/// timeout below one millisecond rounds down to). The request is then still
/// in flight, and so are its buffers.
fn still_pending(code: u32) -> bool {
    code == WAIT_TIMEOUT.0 || code == ERROR_IO_INCOMPLETE.0
}

pub(crate) fn last_err(e: &windows::core::Error) -> u32 {
    WIN32_ERROR::from_error(e)
        .map(|w| w.0)
        .unwrap_or(e.code().0 as u32)
}

impl Device {
    pub fn open(path: &str) -> Result<Self, ChanError> {
        let h = unsafe {
            CreateFileW(
                &HSTRING::from(path),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(|e| ChanError::Os {
            code: last_err(&e),
            what: "CreateFile",
        })?;
        Ok(Self {
            h: Arc::new(OwnedHandle(h)),
            path: path.to_string(),
        })
    }

    /// Open the `index`-th interface (or the explicit `path`).
    pub fn find(index: usize, path: Option<&str>) -> Result<Self, ChanError> {
        if let Some(p) = path {
            return Self::open(p);
        }
        let list = list_interfaces()?;
        let p = list.get(index).ok_or_else(|| {
            ChanError::Protocol(format!(
                "no crossc device interface #{index} (found {}). Is crossc_bth installed on the buds' A2DP node and the buds connected?",
                list.len()
            ))
        })?;
        Self::open(p)
    }

    /// DeviceIoControl with timeout. Returns bytes returned.
    pub fn ioctl(
        &self,
        code: u32,
        input: &[u8],
        output: &mut [u8],
        timeout: Option<Duration>,
    ) -> Result<usize, ChanError> {
        let ev = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(|e| {
            ChanError::Os {
                code: last_err(&e),
                what: "CreateEvent",
            }
        })?;
        let _ev = OwnedHandle(ev);
        let mut ov = OVERLAPPED {
            hEvent: ev,
            ..Default::default()
        };
        let h = self.h.0;
        let r = unsafe {
            DeviceIoControl(
                h,
                code,
                (!input.is_empty()).then_some(input.as_ptr().cast()),
                input.len() as u32,
                (!output.is_empty()).then_some(output.as_mut_ptr().cast()),
                output.len() as u32,
                None,
                Some(&mut ov),
            )
        };
        if let Err(e) = r
            && last_err(&e) != ERROR_IO_PENDING.0
        {
            return Err(map_err(last_err(&e), "DeviceIoControl"));
        }
        let ms = timeout.map_or(u32::MAX, |t| t.as_millis().min(u32::MAX as u128 - 1) as u32);
        let mut n = 0u32;
        match unsafe { GetOverlappedResultEx(h, &ov, &mut n, ms, false) } {
            Ok(()) => Ok(n as usize),
            Err(e) if still_pending(last_err(&e)) => {
                // Cancel and wait for the driver to complete it; buffers stay
                // borrowed until then. It may have completed with data anyway.
                let _ = unsafe { CancelIoEx(h, Some(&ov)) };
                match unsafe { GetOverlappedResult(h, &ov, &mut n, true) } {
                    Ok(()) => Ok(n as usize),
                    Err(_) => Err(ChanError::Timeout),
                }
            }
            Err(e) => Err(map_err(last_err(&e), "IOCTL")),
        }
    }

    pub fn info(&self) -> Result<ioc::InfoReply, ChanError> {
        let mut out = [0u8; ioc::InfoReply::SIZE];
        let n = self.ioctl(
            ioc::IOCTL_GET_INFO,
            &[],
            &mut out,
            Some(Duration::from_secs(5)),
        )?;
        ioc::InfoReply::decode(&out[..n])
            .map_err(|_| ChanError::Protocol("short GET_INFO reply".into()))
    }

    pub fn open_channel(
        &self,
        req: ioc::OpenChannelRequest,
        timeout: Duration,
    ) -> Result<Channel, ChanError> {
        let mut out = [0u8; ioc::OpenChannelReply::SIZE];
        let n = self.ioctl(
            ioc::IOCTL_OPEN_CHANNEL,
            &req.encode(),
            &mut out,
            Some(timeout),
        )?;
        let r = ioc::OpenChannelReply::decode(&out[..n])
            .map_err(|_| ChanError::Protocol("short OPEN reply".into()))?;
        Ok(Channel {
            dev: self.clone(),
            id: r.channel_id,
            out_mtu: r.out_mtu,
            in_mtu: r.in_mtu,
            closed: false,
        })
    }
}

pub(crate) fn map_err(code: u32, what: &'static str) -> ChanError {
    if code == ERROR_NETNAME_DELETED.0 {
        ChanError::Disconnected
    } else {
        ChanError::Os { code, what }
    }
}

/// One open L2CAP channel. Closed on drop.
pub struct Channel {
    dev: Device,
    pub id: u32,
    pub out_mtu: u16,
    pub in_mtu: u16,
    closed: bool,
}

impl Channel {
    pub fn close(&mut self) -> Result<(), ChanError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let req = ioc::ChannelRequest {
            channel_id: self.id,
        }
        .encode();
        self.dev
            .ioctl(
                ioc::IOCTL_CLOSE_CHANNEL,
                &req,
                &mut [],
                Some(Duration::from_secs(10)),
            )
            .map(|_| ())
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl SduChannel for Channel {
    fn send(&mut self, sdu: &[u8]) -> Result<(), ChanError> {
        let mut input = ioc::ChannelRequest {
            channel_id: self.id,
        }
        .encode()
        .to_vec();
        input.extend_from_slice(sdu);
        let mut out = [0u8; ioc::WriteReply::SIZE];
        self.dev.ioctl(
            ioc::IOCTL_WRITE,
            &input,
            &mut out,
            Some(Duration::from_secs(5)),
        )?;
        Ok(())
    }
    fn recv(&mut self, timeout: Duration) -> Result<Vec<u8>, ChanError> {
        let req = ioc::ChannelRequest {
            channel_id: self.id,
        }
        .encode();
        // METHOD_BUFFERED: the output buffer must be at least as large as the input.
        let mut out = vec![
            0u8;
            usize::from(self.in_mtu)
                .max(ioc::ChannelRequest::SIZE)
                .max(1024)
        ];
        let n = self
            .dev
            .ioctl(ioc::IOCTL_READ, &req, &mut out, Some(timeout))?;
        out.truncate(n);
        Ok(out)
    }
    fn out_mtu(&self) -> usize {
        usize::from(self.out_mtu)
    }
}

// ---------------------------------------------------------------------------
// Requests that stay pending
// ---------------------------------------------------------------------------

/// How long a cancelled request is given to complete before its buffers are
/// abandoned (see [`Pending`]'s `Drop`).
const CANCEL_WAIT_MS: u32 = 10_000;

/// An IOCTL that was sent and may complete much later (ATTACH, WAIT_EVENT,
/// READ_PCM, a channel READ). Owns everything the kernel writes to until the
/// request completes: the OVERLAPPED, its event and the output buffer.
///
/// Dropping a request that is still in flight cancels it and waits for the
/// driver to complete it. Should the driver not do so in time, the memory
/// the kernel may still write to is leaked instead of freed.
pub struct Pending {
    h: Arc<OwnedHandle>,
    ov: Box<OVERLAPPED>,
    ev: OwnedHandle,
    out: Vec<u8>,
    /// Kept until completion (the I/O manager has copied it, but a request
    /// is cheaper to reason about when nothing it was given goes away).
    input: Vec<u8>,
    in_flight: bool,
}

// SAFETY: the raw pointers inside OVERLAPPED are only handed to the kernel;
// the request can be finished or cancelled from any thread.
unsafe impl Send for Pending {}

impl Pending {
    /// Event that is signalled when the request completes.
    pub fn event(&self) -> HANDLE {
        self.ev.0
    }

    /// Whether the request has completed and its result was taken.
    pub fn is_done(&self) -> bool {
        !self.in_flight
    }

    fn result(&mut self, r: windows::core::Result<()>, n: u32) -> Result<Vec<u8>, ChanError> {
        self.in_flight = false;
        match r {
            Ok(()) => {
                let mut out = std::mem::take(&mut self.out);
                out.truncate(n as usize);
                Ok(out)
            }
            Err(e) => Err(map_err(last_err(&e), "IOCTL")),
        }
    }

    /// Wait up to `timeout` for the request to complete. `None`: still
    /// pending (it stays in flight). `Some`: the reply bytes or the error;
    /// the request is then done and must not be polled again.
    pub fn poll(&mut self, timeout: Duration) -> Option<Result<Vec<u8>, ChanError>> {
        if !self.in_flight {
            return Some(Err(ChanError::Protocol("request already finished".into())));
        }
        let ms = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
        let mut n = 0u32;
        match unsafe { GetOverlappedResultEx(self.h.0, &*self.ov, &mut n, ms, false) } {
            Ok(()) => Some(self.result(Ok(()), n)),
            Err(e) => {
                let code = last_err(&e);
                if still_pending(code) {
                    None
                } else {
                    Some(self.result(Err(e), 0))
                }
            }
        }
    }

    /// Cancel the request and wait for it to end. Returns the reply if it
    /// completed anyway.
    pub fn cancel(&mut self) -> Result<Vec<u8>, ChanError> {
        if !self.in_flight {
            return Err(ChanError::Protocol("request already finished".into()));
        }
        let _ = unsafe { CancelIoEx(self.h.0, Some(&*self.ov)) };
        let mut n = 0u32;
        match unsafe { GetOverlappedResultEx(self.h.0, &*self.ov, &mut n, CANCEL_WAIT_MS, false) } {
            Ok(()) => self.result(Ok(()), n),
            Err(e) => {
                let code = last_err(&e);
                if still_pending(code) {
                    // Still in flight: Drop will abandon the buffers.
                    Err(ChanError::Timeout)
                } else {
                    self.result(Err(e), 0)
                }
            }
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.in_flight {
            return;
        }
        if matches!(self.cancel(), Err(ChanError::Timeout)) && self.in_flight {
            // The driver did not complete the cancelled request. The kernel
            // may still write the OVERLAPPED and the output buffer, so they
            // must outlive the request: leak them.
            std::mem::forget(std::mem::replace(
                &mut self.ov,
                Box::new(OVERLAPPED::default()),
            ));
            std::mem::forget(std::mem::take(&mut self.out));
            std::mem::forget(std::mem::take(&mut self.input));
        }
    }
}

impl Device {
    /// Send an IOCTL without waiting for it. `Err` if the driver fails it
    /// at once.
    pub fn start(&self, code: u32, input: &[u8], out_len: usize) -> Result<Pending, ChanError> {
        let ev = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(|e| {
            ChanError::Os {
                code: last_err(&e),
                what: "CreateEvent",
            }
        })?;
        let mut p = Pending {
            h: self.h.clone(),
            ov: Box::new(OVERLAPPED {
                hEvent: ev,
                ..Default::default()
            }),
            ev: OwnedHandle(ev),
            out: vec![0u8; out_len],
            input: input.to_vec(),
            in_flight: true,
        };
        let r = unsafe {
            DeviceIoControl(
                p.h.0,
                code,
                (!p.input.is_empty()).then_some(p.input.as_ptr().cast()),
                p.input.len() as u32,
                (!p.out.is_empty()).then_some(p.out.as_mut_ptr().cast()),
                p.out.len() as u32,
                None,
                Some(&mut *p.ov),
            )
        };
        if let Err(e) = r
            && last_err(&e) != ERROR_IO_PENDING.0
        {
            // Never queued: nothing will complete, nothing to cancel.
            p.in_flight = false;
            return Err(map_err(last_err(&e), "DeviceIoControl"));
        }
        Ok(p)
    }

    // -- audio path (crossc_ioctl::audio, protocol v2) ----------------------

    /// AUDIO_GET_STATE. Works on any handle, attached or not.
    pub fn audio_state(&self) -> Result<audio::AudioState, ChanError> {
        let mut out = [0u8; audio::AudioState::SIZE];
        let n = self.ioctl(
            audio::IOCTL_AUDIO_GET_STATE,
            &[],
            &mut out,
            Some(Duration::from_secs(5)),
        )?;
        audio::AudioState::decode(&out[..n])
            .map_err(|_| ChanError::Protocol("short AUDIO_GET_STATE reply".into()))
    }

    /// AUDIO_ATTACH: returned still pending; the session lasts until the
    /// request completes, is cancelled or is dropped.
    pub fn attach(&self) -> Result<Pending, ChanError> {
        self.start(audio::IOCTL_AUDIO_ATTACH, &[], 0)
    }

    /// AUDIO_SET_LINK (attached handle only).
    pub fn set_link(&self, connected: bool) -> Result<(), ChanError> {
        self.ioctl(
            audio::IOCTL_AUDIO_SET_LINK,
            &audio::SetLinkRequest { connected }.encode(),
            &mut [],
            Some(Duration::from_secs(5)),
        )
        .map(|_| ())
    }

    /// AUDIO_WAIT_EVENT, left pending: completes once the state's sequence
    /// number differs from `seq`. Decode the reply with [`decode_state`].
    pub fn start_wait_event(&self, seq: u32) -> Result<Pending, ChanError> {
        self.start(
            audio::IOCTL_AUDIO_WAIT_EVENT,
            &audio::WaitEventRequest { seq }.encode(),
            audio::AudioState::SIZE,
        )
    }

    /// AUDIO_WAIT_EVENT, waiting up to `timeout` ([`ChanError::Timeout`]).
    pub fn wait_event(&self, seq: u32, timeout: Duration) -> Result<audio::AudioState, ChanError> {
        let mut out = [0u8; audio::AudioState::SIZE];
        let n = self.ioctl(
            audio::IOCTL_AUDIO_WAIT_EVENT,
            &audio::WaitEventRequest { seq }.encode(),
            &mut out,
            Some(timeout),
        )?;
        decode_state(&out[..n])
    }

    /// AUDIO_READ_PCM, left pending: completes with the next chunk. Decode
    /// the reply with [`decode_pcm`].
    pub fn start_read_pcm(&self) -> Result<Pending, ChanError> {
        self.start(audio::IOCTL_AUDIO_READ_PCM, &[], audio::PCM_READ_BUFFER)
    }

    /// AUDIO_READ_PCM, waiting up to `timeout` ([`ChanError::Timeout`]).
    pub fn read_pcm(&self, timeout: Duration) -> Result<PcmChunk, ChanError> {
        let mut out = vec![0u8; audio::PCM_READ_BUFFER];
        let n = self.ioctl(audio::IOCTL_AUDIO_READ_PCM, &[], &mut out, Some(timeout))?;
        decode_pcm(&out[..n])
    }

    /// A channel READ, left pending (one SDU).
    pub fn start_read(&self, channel_id: u32, in_mtu: u16) -> Result<Pending, ChanError> {
        // METHOD_BUFFERED: the output buffer must be at least as large as the input.
        let len = usize::from(in_mtu).max(ioc::ChannelRequest::SIZE).max(1024);
        self.start(
            ioc::IOCTL_READ,
            &ioc::ChannelRequest { channel_id }.encode(),
            len,
        )
    }

    /// One SDU on a channel given by id (what [`Channel::send`] does).
    pub fn write(&self, channel_id: u32, sdu: &[u8]) -> Result<(), ChanError> {
        let mut input = ioc::ChannelRequest { channel_id }.encode().to_vec();
        input.extend_from_slice(sdu);
        let mut out = [0u8; ioc::WriteReply::SIZE];
        self.ioctl(
            ioc::IOCTL_WRITE,
            &input,
            &mut out,
            Some(Duration::from_secs(5)),
        )?;
        Ok(())
    }

    /// OPEN_CHANNEL, returning the raw reply (no [`Channel`] wrapper).
    pub fn open_channel_raw(
        &self,
        req: ioc::OpenChannelRequest,
        timeout: Duration,
    ) -> Result<ioc::OpenChannelReply, ChanError> {
        let mut out = [0u8; ioc::OpenChannelReply::SIZE];
        let n = self.ioctl(
            ioc::IOCTL_OPEN_CHANNEL,
            &req.encode(),
            &mut out,
            Some(timeout),
        )?;
        ioc::OpenChannelReply::decode(&out[..n])
            .map_err(|_| ChanError::Protocol("short OPEN reply".into()))
    }

    /// CLOSE_CHANNEL by id.
    pub fn close_channel(&self, channel_id: u32) -> Result<(), ChanError> {
        self.ioctl(
            ioc::IOCTL_CLOSE_CHANNEL,
            &ioc::ChannelRequest { channel_id }.encode(),
            &mut [],
            Some(Duration::from_secs(10)),
        )
        .map(|_| ())
    }
}

/// Decode a GET_STATE / WAIT_EVENT reply.
pub fn decode_state(reply: &[u8]) -> Result<audio::AudioState, ChanError> {
    audio::AudioState::decode(reply)
        .map_err(|_| ChanError::Protocol("short audio state reply".into()))
}

/// Decode a READ_PCM reply.
pub fn decode_pcm(reply: &[u8]) -> Result<PcmChunk, ChanError> {
    let (header, data) = audio::PcmHeader::split(reply).ok_or_else(|| {
        ChanError::Protocol(format!(
            "malformed AUDIO_READ_PCM reply ({} bytes)",
            reply.len()
        ))
    })?;
    if header.frames == 0 {
        return Err(ChanError::Protocol("empty AUDIO_READ_PCM reply".into()));
    }
    Ok(PcmChunk {
        header,
        data: data.to_vec(),
    })
}

/// FSCTL_REQUEST_OPLOCK_LEVEL_1: on a file with a single open handle
/// this request pends until someone else opens the file, and it can be
/// cancelled. That makes an ordinary file a stand-in for a device with
/// long-pending IOCTLs, so [`Pending`] is exercised against the real
/// I/O manager.
#[cfg(test)]
pub(crate) const FSCTL_REQUEST_OPLOCK_LEVEL_1: u32 = 0x0009_0000;

/// A temp file, its handle and a pending oplock request on it; `None`
/// where the file system has no oplocks.
#[cfg(test)]
pub(crate) fn oplock_file(tag: &str) -> Option<(std::path::PathBuf, Device, Pending)> {
    let path = std::env::temp_dir().join(format!("crossc-pending-{tag}-{}", std::process::id()));
    std::fs::write(&path, b"x").unwrap();
    let dev = Device::open(path.to_str().unwrap()).unwrap();
    match dev.start(FSCTL_REQUEST_OPLOCK_LEVEL_1, &[], 0) {
        Ok(p) => Some((path, dev, p)),
        // A file system without oplocks (invalid function / not supported):
        // nothing to test with.
        Err(ChanError::Os {
            code: code @ (1 | 50),
            ..
        }) => {
            eprintln!("skipped: no oplocks on the temp directory's file system (error {code})");
            let _ = std::fs::remove_file(&path);
            None
        }
        Err(e) => panic!("oplock request on {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_sz_lists() {
        let v: Vec<u16> = "a\0bc\0\0".encode_utf16().collect();
        assert_eq!(split_multi_sz(&v), vec!["a".to_string(), "bc".to_string()]);
        assert!(split_multi_sz(&[0, 0]).is_empty());
    }

    #[test]
    fn listing_interfaces_needs_no_privilege() {
        // Whatever is installed on this machine, the query itself works.
        let l = list_interfaces().unwrap();
        assert!(
            l.iter()
                .all(|p| p.to_ascii_lowercase().contains("433f9eba"))
        );
    }

    #[test]
    fn pcm_replies() {
        let h = audio::PcmHeader {
            first_frame: 9,
            frames: 2,
            flags: audio::PCM_FLAG_DISCONTINUITY,
            format: audio::PcmFormat {
                sample_rate: 48000,
                channels: 2,
                container_bits: 16,
                valid_bits: 16,
            },
        };
        let mut reply = h.encode().to_vec();
        reply.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let c = decode_pcm(&reply).unwrap();
        assert_eq!((c.header, c.data.len()), (h, 8));
        assert!(decode_pcm(&reply[..reply.len() - 1]).is_err(), "short");
        assert!(decode_pcm(&[]).is_err());
        let empty = audio::PcmHeader { frames: 0, ..h }.encode();
        assert!(decode_pcm(&empty).is_err());
        assert!(decode_state(&[0; 39]).is_err());
        assert_eq!(
            decode_state(&[0; 40]).unwrap(),
            audio::AudioState::default()
        );
    }

    #[test]
    fn a_missing_device_is_an_error_not_a_panic() {
        let e = Device::open(r"\\?\crossc-no-such-device").err().unwrap();
        assert!(matches!(e, ChanError::Os { .. }), "{e}");
        assert!(Device::find(9999, None).is_err());
    }

    #[test]
    fn pending_request_completes_when_the_kernel_completes_it() {
        let Some((path, dev, mut p)) = oplock_file("complete") else {
            return;
        };
        assert!(!p.is_done());
        assert!(p.poll(Duration::ZERO).is_none(), "still in flight");
        assert!(p.poll(Duration::from_millis(30)).is_none());
        // A second open breaks the oplock; it blocks until our handle
        // acknowledges, which closing it does.
        let path2 = path.clone();
        let opener = std::thread::spawn(move || std::fs::File::open(path2).is_ok());
        let r = p.poll(Duration::from_secs(20));
        assert!(matches!(r, Some(Ok(_))), "{r:?}");
        assert!(p.is_done());
        assert!(matches!(p.poll(Duration::ZERO), Some(Err(_))), "only once");
        drop(p);
        drop(dev);
        assert!(opener.join().unwrap());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pending_request_can_be_cancelled_dropped_and_timed_out() {
        let Some((path, dev, mut p)) = oplock_file("cancel") else {
            return;
        };
        // Cancel: the driver completes it as aborted.
        let e = p.cancel().unwrap_err();
        assert!(matches!(e, ChanError::Os { code: 995, .. }), "{e}");
        assert!(p.is_done());
        assert!(p.cancel().is_err(), "nothing left to cancel");
        drop(p);
        // Drop while in flight: cancelled and waited for, promptly.
        let p = dev.start(FSCTL_REQUEST_OPLOCK_LEVEL_1, &[], 0).unwrap();
        let t0 = std::time::Instant::now();
        drop(p);
        assert!(t0.elapsed() < Duration::from_secs(5));
        // The blocking form with a timeout cancels too, and the handle is
        // usable afterwards.
        let t0 = std::time::Instant::now();
        let r = dev.ioctl(
            FSCTL_REQUEST_OPLOCK_LEVEL_1,
            &[],
            &mut [],
            Some(Duration::from_millis(50)),
        );
        assert!(matches!(r, Err(ChanError::Timeout)), "{r:?}");
        // (Windows may end a wait up to one timer tick early.)
        assert!(t0.elapsed() >= Duration::from_millis(30));
        let mut again = dev.start(FSCTL_REQUEST_OPLOCK_LEVEL_1, &[], 0).unwrap();
        assert!(again.poll(Duration::ZERO).is_none());
        // A request the driver refuses outright is an error from `start`.
        let refused = dev.start(audio::IOCTL_AUDIO_ATTACH, &[], 0);
        assert!(matches!(refused, Err(ChanError::Os { .. })));
        drop(again);
        drop(dev);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_timeout_below_one_millisecond_still_cancels_the_request() {
        // Such a timeout rounds down to a zero wait, which the system
        // reports as "incomplete" rather than as a timeout. Either way the
        // request must be over when `ioctl` returns: it owns the buffers.
        for timeout in [Duration::from_micros(500), Duration::ZERO] {
            let Some((path, dev, first)) = oplock_file("submilli") else {
                return;
            };
            drop(first); // cancelled; the file has no oplock request now
            let r = dev.ioctl(FSCTL_REQUEST_OPLOCK_LEVEL_1, &[], &mut [], Some(timeout));
            assert!(matches!(r, Err(ChanError::Timeout)), "{timeout:?}: {r:?}");
            // Were that request still in flight, a second one would be
            // refused (ERROR_OPLOCK_NOT_GRANTED) instead of pending.
            let mut again = dev
                .start(FSCTL_REQUEST_OPLOCK_LEVEL_1, &[], 0)
                .unwrap_or_else(|e| panic!("{timeout:?}: the first request was left pending: {e}"));
            assert!(again.poll(Duration::ZERO).is_none());
            drop(again);
            drop(dev);
            let _ = std::fs::remove_file(&path);
        }
    }
}
