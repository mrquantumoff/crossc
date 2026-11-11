//! The real [`Driver`]: one crossc device interface, with the long-lived
//! requests (ATTACH, WAIT_EVENT, READ_PCM, the signaling channel's READ) kept
//! pending and waited for together. Also the real [`LinkProbe`].
//!
//! A `RealDriver` belongs to one thread (the session's); it is not `Send`.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use crossc_ioctl as ioc;
use crossc_ioctl::audio::AudioState;
use windows::Win32::Devices::Bluetooth::{
    BLUETOOTH_DEVICE_INFO, BLUETOOTH_FIND_RADIO_PARAMS, BluetoothFindFirstRadio,
    BluetoothFindNextRadio, BluetoothFindRadioClose, BluetoothGetDeviceInfo,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::WaitForMultipleObjects;

use crate::channel::ChanError;
use crate::device::{Device, Pending, decode_pcm, decode_state};
use crate::driver::{Driver, Interest, LinkProbe, Wake};
use crate::stop::StopSignal;

struct Inner {
    path: String,
    stop: StopSignal,
    dev: Option<Device>,
    attach: Option<Pending>,
    state: Option<Pending>,
    pcm: Option<Pending>,
    /// The pending READ and the channel it is for.
    sig: Option<(u32, Pending)>,
}

/// See the module docs.
#[derive(Clone)]
pub struct RealDriver(Rc<RefCell<Inner>>);

fn not_open() -> ChanError {
    ChanError::Protocol("the device is not open".into())
}

/// What one handle in a multi-handle wait stands for.
#[derive(Clone, Copy)]
enum Source {
    Stop,
    Attach,
    Signaling,
    State,
    Pcm,
}

impl RealDriver {
    /// A driver for the device interface `path`. The device is opened by the
    /// first [`Driver::reopen`]. `stop` interrupts every [`Driver::wait`].
    pub fn new(path: &str, stop: StopSignal) -> Self {
        RealDriver(Rc::new(RefCell::new(Inner {
            path: path.to_string(),
            stop,
            dev: None,
            attach: None,
            state: None,
            pcm: None,
            sig: None,
        })))
    }

    pub fn path(&self) -> String {
        self.0.borrow().path.clone()
    }

    fn dev(&self) -> Result<Device, ChanError> {
        self.0.borrow().dev.clone().ok_or_else(not_open)
    }
}

impl Inner {
    /// Drop (cancel and wait for) every pending request, the ATTACH last:
    /// the session it stands for ends only once nothing else of ours is in
    /// flight. (Cancelled first, the driver would sweep the other audio
    /// requests itself, and they would be cancelled a second time here.)
    fn drop_pending(&mut self) {
        self.state = None;
        self.pcm = None;
        self.sig = None;
        self.attach = None;
    }

    /// Make sure a READ is pending on channel `id`.
    fn arm_read(&mut self, dev: &Device, id: u32) -> Result<(), ChanError> {
        if matches!(&self.sig, Some((cur, _)) if *cur == id) {
            return Ok(());
        }
        self.sig = None; // a READ for another channel: cancel it
        // The driver's reported in_mtu is not reliable; the largest SDU an
        // AVDTP signaling peer sends fits the default L2CAP MTU many times
        // over in this buffer.
        let p = dev.start_read(id, 4096)?;
        self.sig = Some((id, p));
        Ok(())
    }
}

impl Driver for RealDriver {
    fn reopen(&self) -> Result<(), ChanError> {
        let mut i = self.0.borrow_mut();
        i.drop_pending();
        i.dev = None;
        let dev = Device::open(&i.path)?;
        i.dev = Some(dev);
        Ok(())
    }

    fn info(&self) -> Result<ioc::InfoReply, ChanError> {
        self.dev()?.info()
    }

    fn audio_state(&self) -> Result<AudioState, ChanError> {
        self.dev()?.audio_state()
    }

    fn attach(&self) -> Result<(), ChanError> {
        let dev = self.dev()?;
        let mut i = self.0.borrow_mut();
        i.drop_pending();
        let mut p = dev.attach()?;
        // A refusal (busy, no audio path) may also arrive as an immediate
        // completion rather than a failed call.
        match p.poll(Duration::ZERO) {
            None => {
                i.attach = Some(p);
                Ok(())
            }
            Some(Err(e)) => Err(e),
            Some(Ok(_)) => Err(ChanError::Protocol(
                "AUDIO_ATTACH completed at once instead of pending".into(),
            )),
        }
    }

    fn detach(&self) {
        self.0.borrow_mut().drop_pending();
    }

    fn set_link(&self, connected: bool) -> Result<(), ChanError> {
        self.dev()?.set_link(connected)
    }

    fn open_channel(
        &self,
        req: ioc::OpenChannelRequest,
        timeout: Duration,
    ) -> Result<ioc::OpenChannelReply, ChanError> {
        self.dev()?.open_channel_raw(req, timeout)
    }

    fn close_channel(&self, id: u32) -> Result<(), ChanError> {
        let dev = {
            let mut i = self.0.borrow_mut();
            if matches!(&i.sig, Some((cur, _)) if *cur == id) {
                i.sig = None;
            }
            i.dev.clone().ok_or_else(not_open)?
        };
        dev.close_channel(id)
    }

    fn write(&self, id: u32, sdu: &[u8]) -> Result<(), ChanError> {
        self.dev()?.write(id, sdu)
    }

    fn read(&self, id: u32, timeout: Duration) -> Result<Vec<u8>, ChanError> {
        let dev = self.dev()?;
        let mut i = self.0.borrow_mut();
        i.arm_read(&dev, id)?;
        let Some((_, p)) = &mut i.sig else {
            return Err(not_open());
        };
        match p.poll(timeout) {
            None => Err(ChanError::Timeout),
            Some(r) => {
                i.sig = None;
                r
            }
        }
    }

    fn flush_pcm(&self) {
        // Cancels a READ_PCM in flight; a completed one is discarded.
        self.0.borrow_mut().pcm = None;
    }

    fn wait(&self, interest: &Interest, timeout: Duration) -> Wake {
        let mut i = self.0.borrow_mut();
        if i.stop.is_set() {
            return Wake::Stop;
        }
        let Some(dev) = i.dev.clone() else {
            let stop = i.stop.clone();
            drop(i);
            return if stop.wait(timeout) {
                Wake::Stop
            } else {
                Wake::Timeout
            };
        };
        // Arm what is asked for and not yet pending. The audio requests are
        // only valid while attached.
        if i.attach.is_some() {
            if let Some(seq) = interest.seq
                && i.state.is_none()
            {
                match dev.start_wait_event(seq) {
                    Ok(p) => i.state = Some(p),
                    Err(e) => return Wake::StateError(e),
                }
            }
            if interest.pcm && i.pcm.is_none() {
                match dev.start_read_pcm() {
                    Ok(p) => i.pcm = Some(p),
                    Err(e) => return Wake::Pcm(Err(e)),
                }
            }
        }
        if let Some(id) = interest.signaling
            && let Err(e) = i.arm_read(&dev, id)
        {
            return Wake::Signaling(Err(e));
        }
        let mut handles: Vec<HANDLE> = Vec::with_capacity(5);
        let mut sources: Vec<Source> = Vec::with_capacity(5);
        if let Some(h) = i.stop.event() {
            handles.push(h);
            sources.push(Source::Stop);
        }
        if let Some(p) = &i.attach {
            handles.push(p.event());
            sources.push(Source::Attach);
        }
        if let (Some(id), Some((cur, p))) = (interest.signaling, &i.sig)
            && id == *cur
        {
            handles.push(p.event());
            sources.push(Source::Signaling);
        }
        if let (Some(_), Some(p)) = (interest.seq, &i.state) {
            handles.push(p.event());
            sources.push(Source::State);
        }
        if let (true, Some(p)) = (interest.pcm, &i.pcm) {
            handles.push(p.event());
            sources.push(Source::Pcm);
        }
        if handles.is_empty() {
            let stop = i.stop.clone();
            drop(i);
            return if stop.wait(timeout) {
                Wake::Stop
            } else {
                Wake::Timeout
            };
        }
        let ms = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
        let w = unsafe { WaitForMultipleObjects(&handles, false, ms) };
        if w == WAIT_TIMEOUT {
            return Wake::Timeout;
        }
        let index = w.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
        let Some(source) = sources.get(index).copied() else {
            // WAIT_FAILED or an abandoned handle: do not spin on it.
            let stop = i.stop.clone();
            drop(i);
            stop.wait(timeout.min(Duration::from_millis(200)));
            return Wake::Timeout;
        };
        // The event is signalled, so the request has completed: collect it.
        let finish = |p: Option<Pending>| -> Result<Vec<u8>, ChanError> {
            match p {
                Some(mut p) => p.poll(Duration::ZERO).unwrap_or(Err(ChanError::Timeout)),
                None => Err(ChanError::Protocol("no such request".into())),
            }
        };
        match source {
            Source::Stop => Wake::Stop,
            Source::Attach => {
                let r = finish(i.attach.take());
                // The driver has cancelled the session's other requests.
                i.drop_pending();
                Wake::Detached(match r {
                    Err(e) => e,
                    Ok(_) => ChanError::Protocol("the driver completed AUDIO_ATTACH".into()),
                })
            }
            Source::Signaling => Wake::Signaling(finish(i.sig.take().map(|(_, p)| p))),
            Source::State => match finish(i.state.take()).and_then(|b| decode_state(&b)) {
                Ok(s) => Wake::State(s),
                Err(e) => Wake::StateError(e),
            },
            Source::Pcm => Wake::Pcm(finish(i.pcm.take()).and_then(|b| decode_pcm(&b))),
        }
    }
}

/// [`LinkProbe`] over the Windows Bluetooth API: `fConnected` of
/// `BluetoothGetDeviceInfo` for the buds' address. Only reads state Windows
/// already has; never causes any traffic to the buds.
#[derive(Default)]
pub struct BluetoothLink;

fn device_info(radio: Option<HANDLE>, addr: u64) -> Result<BLUETOOTH_DEVICE_INFO, u32> {
    let mut di = BLUETOOTH_DEVICE_INFO {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
        ..Default::default()
    };
    di.Address.Anonymous.ullLong = addr;
    match unsafe { BluetoothGetDeviceInfo(radio, &mut di) } {
        0 => Ok(di),
        e => Err(e),
    }
}

/// What Windows knows about the device at `addr`: asked of every local
/// radio at once, and of each radio in turn if that fails.
fn device_info_any(addr: u64) -> Result<BLUETOOTH_DEVICE_INFO, u32> {
    // With no radio handle the API asks every local radio.
    let first = match device_info(None, addr) {
        Ok(di) => return Ok(di),
        Err(e) => e,
    };
    let mut result = Err(first);
    for r in radios() {
        if result.is_err() {
            result = device_info(Some(r), addr);
        }
        let _ = unsafe { CloseHandle(r) };
    }
    result
}

/// The name in a `szName` field: UTF-16 up to the first NUL.
fn name_from_utf16(raw: &[u16]) -> String {
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end]).trim().to_string()
}

/// The Bluetooth name Windows has for the device at `addr` (empty if it has
/// none), or `None` if it does not know the device. Like the link probe,
/// this only reads what Windows already has.
pub fn device_name(addr: u64) -> Option<String> {
    device_info_any(addr)
        .ok()
        .map(|di| name_from_utf16(&di.szName))
}

/// Handles of all local Bluetooth radios (to be closed by the caller).
fn radios() -> Vec<HANDLE> {
    let params = BLUETOOTH_FIND_RADIO_PARAMS {
        dwSize: std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32,
    };
    let mut out = Vec::new();
    let mut h = HANDLE::default();
    let Ok(find) = (unsafe { BluetoothFindFirstRadio(&params, &mut h) }) else {
        return out;
    };
    out.push(h);
    // Bounded: a machine has a handful of radios at most.
    while out.len() < 16 {
        let mut h = HANDLE::default();
        if unsafe { BluetoothFindNextRadio(find, &mut h) }.is_err() {
            break;
        }
        out.push(h);
    }
    let _ = unsafe { BluetoothFindRadioClose(find) };
    out
}

impl LinkProbe for BluetoothLink {
    fn connected(&mut self, addr: u64) -> Result<bool, String> {
        device_info_any(addr)
            .map(|di| di.fConnected.as_bool())
            .map_err(|e| format!("BluetoothGetDeviceInfo failed: Win32 error {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_driver_without_a_device_fails_cleanly_and_honours_stop() {
        let stop = StopSignal::new();
        let d = RealDriver::new(r"\\?\crossc-no-such-device", stop.clone());
        assert!(d.reopen().is_err());
        assert!(d.info().is_err());
        assert!(d.audio_state().is_err());
        assert!(d.attach().is_err());
        assert!(d.set_link(true).is_err());
        assert!(d.write(1, &[0]).is_err());
        assert!(d.read(1, Duration::from_millis(1)).is_err());
        assert!(d.close_channel(1).is_err());
        d.detach();
        d.flush_pcm();
        let all = Interest {
            signaling: Some(1),
            seq: Some(0),
            pcm: true,
        };
        assert!(matches!(
            d.wait(&all, Duration::from_millis(5)),
            Wake::Timeout
        ));
        stop.request();
        assert!(matches!(d.wait(&all, Duration::from_secs(30)), Wake::Stop));
        assert_eq!(d.path(), r"\\?\crossc-no-such-device");
    }

    /// A driver whose long-lived requests are oplock requests on temp files
    /// (see `device::oplock_file`): real pending I/O that completes when the
    /// file is opened a second time. Returns the driver and the files.
    fn pending_driver(
        stop: &StopSignal,
        test: &str,
    ) -> Option<(RealDriver, Vec<std::path::PathBuf>)> {
        let file = |what: &str| crate::device::oplock_file(&format!("{test}-{what}"));
        let d = RealDriver::new("unused", stop.clone());
        let mut paths = Vec::new();
        let mut i = d.0.borrow_mut();
        let (path, dev, attach) = file("attach")?;
        paths.push(path);
        i.dev = Some(dev);
        i.attach = Some(attach);
        let (path, _, sig) = file("sig")?;
        paths.push(path);
        i.sig = Some((7, sig));
        let (path, _, state) = file("state")?;
        paths.push(path);
        i.state = Some(state);
        let (path, _, pcm) = file("pcm")?;
        paths.push(path);
        i.pcm = Some(pcm);
        drop(i);
        Some((d, paths))
    }

    /// Open `path` on another thread shortly (completes the request pending
    /// on it). The open itself blocks until the request's owner lets go.
    fn open_later(path: &std::path::Path) -> std::thread::JoinHandle<()> {
        let path = path.to_path_buf();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            let _ = std::fs::File::open(path);
        })
    }

    #[test]
    fn wait_wakes_for_exactly_the_request_that_completed() {
        let stop = StopSignal::new();
        let Some((d, paths)) = pending_driver(&stop, "wake") else {
            return;
        };
        let all = Interest {
            signaling: Some(7),
            seq: Some(1),
            pcm: true,
        };
        // Nothing completes: a real timeout.
        let t0 = std::time::Instant::now();
        assert!(matches!(
            d.wait(&all, Duration::from_millis(40)),
            Wake::Timeout
        ));
        assert!(t0.elapsed() >= Duration::from_millis(25));
        let mut openers = Vec::new();
        // The signaling READ completes (with an empty reply here).
        openers.push(open_later(&paths[1]));
        let w = d.wait(&all, Duration::from_secs(20));
        assert!(
            matches!(&w, Wake::Signaling(Ok(sdu)) if sdu.is_empty()),
            "{w:?}"
        );
        assert!(d.0.borrow().sig.is_none(), "collected");
        // The PCM read completes; an empty reply is not a valid chunk.
        let audio = Interest {
            signaling: None,
            ..all
        };
        openers.push(open_later(&paths[3]));
        let w = d.wait(&audio, Duration::from_secs(20));
        assert!(matches!(w, Wake::Pcm(Err(ChanError::Protocol(_)))), "{w:?}");
        // Not interested in the state: its completion does not wake us.
        openers.push(open_later(&paths[2]));
        let quiet = Interest::default();
        assert!(matches!(
            d.wait(&quiet, Duration::from_millis(150)),
            Wake::Timeout
        ));
        // Interested: it is reported at once (an empty reply is too short).
        let only_state = Interest {
            seq: Some(1),
            ..Interest::default()
        };
        let w = d.wait(&only_state, Duration::from_secs(20));
        assert!(matches!(w, Wake::StateError(_)), "{w:?}");
        // The ATTACH request ends: detached, whatever the interest, and
        // every other pending request is dropped with it.
        openers.push(open_later(&paths[0]));
        let w = d.wait(&quiet, Duration::from_secs(20));
        assert!(matches!(w, Wake::Detached(_)), "{w:?}");
        {
            let i = d.0.borrow();
            assert!(i.attach.is_none() && i.state.is_none() && i.pcm.is_none());
        }
        // Closing the handles lets the openers finish.
        d.0.borrow_mut().dev = None;
        drop(d);
        for o in openers {
            o.join().unwrap();
        }
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn wait_is_interrupted_by_the_stop_request_and_detach_cancels() {
        let stop = StopSignal::new();
        let Some((d, paths)) = pending_driver(&stop, "stop") else {
            return;
        };
        let all = Interest {
            signaling: Some(7),
            seq: Some(1),
            pcm: true,
        };
        let s2 = stop.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            s2.request();
        });
        let t0 = std::time::Instant::now();
        assert!(matches!(d.wait(&all, Duration::from_secs(30)), Wake::Stop));
        assert!(t0.elapsed() < Duration::from_secs(10));
        t.join().unwrap();
        // A blocking channel read on the pending READ: times out, the
        // request stays for the next call.
        assert!(matches!(
            d.read(7, Duration::from_millis(20)),
            Err(ChanError::Timeout)
        ));
        assert!(d.0.borrow().sig.is_some());
        // flush_pcm and detach cancel what is in flight, promptly.
        let t0 = std::time::Instant::now();
        d.flush_pcm();
        assert!(d.0.borrow().pcm.is_none());
        d.detach();
        {
            let i = d.0.borrow();
            assert!(i.attach.is_none() && i.state.is_none() && i.sig.is_none());
        }
        assert!(t0.elapsed() < Duration::from_secs(5));
        drop(d);
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn names_end_at_the_first_nul() {
        let mut raw = [0u16; 248];
        for (i, c) in "  Some Buds ".encode_utf16().enumerate() {
            raw[i] = c;
        }
        raw[20] = u16::from(b'x'); // after the terminator: not part of it
        assert_eq!(name_from_utf16(&raw), "Some Buds");
        assert_eq!(name_from_utf16(&[0u16; 248]), "");
        // No terminator at all: the whole field.
        assert_eq!(name_from_utf16(&[u16::from(b'a'); 4]), "aaaa");
        // An address nothing is paired under: no name, or an empty one.
        let n = device_name(0x0000_1234_5678_9ABC);
        assert!(n.as_deref().is_none_or(str::is_empty), "{n:?}");
    }

    #[test]
    fn link_probe_answers_for_an_unknown_address() {
        // An address nothing is paired under: either "not connected" or an
        // error (no radio), never a panic and never "connected".
        let r = BluetoothLink.connected(0x0000_1234_5678_9ABC);
        assert_ne!(r, Ok(true), "{r:?}");
    }
}
