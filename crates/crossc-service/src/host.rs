//! The real system behind the supervisor: listing the crossc device
//! interfaces, and one thread per interface running a session on the real
//! driver, publishing its status to the hub of the status feed. Also the
//! device part of `status`.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread::JoinHandle;

use crossc_core::device::{self, Device};
use crossc_core::driver::LinkProbe;
use crossc_core::format_watch::FormatWatch;
use crossc_core::log::{Level, Log};
use crossc_core::pace::WallClock;
use crossc_core::real::{BluetoothLink, RealDriver, device_name};
use crossc_core::session::Session;
use crossc_core::settings::Settings;
use crossc_core::stop::StopSignal;
use crossc_ioctl as ioc;
use crossc_ioctl::audio::{AudioState, StreamState};
use crossc_status::Hub;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};

use crate::status::run_published;
use crate::supervisor::{Host, Worker, short_name};

pub struct RealHost {
    settings: Settings,
    log: Arc<dyn Log>,
    /// Where the session threads leave their status.
    hub: Hub,
}

impl RealHost {
    pub fn new(settings: Settings, log: Arc<dyn Log>, hub: Hub) -> Self {
        RealHost { settings, log, hub }
    }
}

struct ThreadWorker(Option<JoinHandle<()>>);

impl Worker for ThreadWorker {
    fn finished(&self) -> bool {
        self.0.as_ref().is_none_or(JoinHandle::is_finished)
    }
    fn join(mut self: Box<Self>) {
        if let Some(h) = self.0.take() {
            // The thread catches its own panics; a join error has nothing
            // more to tell.
            let _ = h.join();
        }
    }
}

/// The body of a session thread: runs the session until `stop`, with its
/// status in `hub` under the session's log prefix meanwhile.
fn session_thread(path: String, stop: StopSignal, settings: Settings, log: Arc<dyn Log>, hub: Hub) {
    let name = short_name(&path);
    // Audio is paced by this thread: keep it ahead of ordinary work.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
    let run = AssertUnwindSafe(|| {
        let driver = RealDriver::new(&path, stop.clone());
        // The format picked for the buds in the sound settings, watched on
        // a helper thread; the driver's waits end when it changes.
        let watch = match FormatWatch::start(&path, log.clone(), &name) {
            Ok(w) => {
                driver.set_wake(w.wake_event());
                Some(w)
            }
            Err(e) => {
                log.log(
                    Level::Error,
                    &format!("[{name}] the Windows device format is not followed: {e}"),
                );
                None
            }
        };
        let mut session = Session::new(
            driver,
            BluetoothLink,
            WallClock::new(),
            settings,
            log.clone(),
            stop.clone(),
            &name,
        );
        if let Some(w) = watch {
            session.set_format_source(Box::new(w));
        }
        // The buds' Bluetooth name, as Windows has it (asked at the start
        // and when the buds have been seen, never while audio streams).
        let addr = addr_from_path(&path);
        let bt_name = || addr.and_then(device_name).unwrap_or_default();
        run_published(&mut session, &hub, &name, bt_name);
    });
    // The session does not panic by design. Should it ever, the handles are
    // closed by the unwind (the driver then detaches by itself) and the
    // supervisor starts a new session.
    if catch_unwind(run).is_err() {
        // The unwind skipped the end of `run_published`.
        hub.remove_device(&name);
        log.log(
            Level::Error,
            &format!("[{name}] the session thread panicked; it will be restarted"),
        );
    }
}

impl Host for RealHost {
    fn list(&self) -> Result<Vec<String>, String> {
        device::list_interfaces().map_err(|e| e.to_string())
    }

    fn spawn(&self, path: &str, stop: StopSignal) -> Box<dyn Worker> {
        let (p, settings, log) = (path.to_string(), self.settings, self.log.clone());
        let hub = self.hub.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("session-{}", short_name(path)))
            .spawn(move || session_thread(p, stop, settings, log, hub));
        match spawned {
            Ok(h) => Box::new(ThreadWorker(Some(h))),
            Err(e) => {
                self.log.log(
                    Level::Error,
                    &format!("[{}] cannot start a session thread: {e}", short_name(path)),
                );
                // Counts as ended, so the next rescan tries again.
                Box::new(ThreadWorker(None))
            }
        }
    }

    fn sessions_stopped(&self) {
        // Each session thread took its device out as it ended; this is for
        // one that could not.
        self.hub.clear_devices();
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// GET_INFO as one line.
pub fn fmt_info(i: &ioc::InfoReply) -> String {
    let mut a = [0u8; 17];
    ioc::format_bd_addr(i.remote_addr, &mut a);
    format!(
        "driver {}.{}.{}, protocol v{}, buds {}, {}/{} channels open, {}",
        i.driver_version >> 24,
        (i.driver_version >> 16) & 0xFF,
        i.driver_version & 0xFFFF,
        i.protocol_version,
        if i.flags & ioc::INFO_FLAG_ADDR_VALID != 0 {
            String::from_utf8_lossy(&a).into_owned()
        } else {
            "address unknown".into()
        },
        i.channels_open,
        i.channels_max,
        if i.flags & ioc::INFO_FLAG_D0 != 0 {
            "D0"
        } else {
            "not in D0"
        }
    )
}

/// AUDIO_GET_STATE as one line.
pub fn fmt_audio(s: &AudioState) -> String {
    let stream = match s.stream {
        StreamState::None => "none".to_string(),
        st => format!(
            "{} ({} Hz, {} ch, {}-bit in {})",
            if st == StreamState::Running {
                "running"
            } else {
                "paused"
            },
            s.format.sample_rate,
            s.format.channels,
            s.format.valid_bits,
            s.format.container_bits
        ),
    };
    let default = if s.default_format.is_valid() {
        format!(
            ", default format {} Hz/{}",
            s.default_format.sample_rate, s.default_format.valid_bits
        )
    } else {
        String::new()
    };
    format!(
        "endpoint {}{default}, service attached: {}, link: {}, stream: {stream}, frames rendered {} / dropped {}; volume: {}",
        if s.acx() { "on" } else { "off (ACX disabled)" },
        yes_no(s.attached()),
        if s.link() { "up" } else { "down" },
        s.frames_rendered,
        s.frames_dropped,
        crossc_core::volume::describe(s)
    )
}

/// The Bluetooth address in an interface path, if it has one.
fn addr_from_path(path: &str) -> Option<u64> {
    let name = short_name(path);
    (name.len() == 12)
        .then(|| u64::from_str_radix(&name, 16).ok())
        .flatten()
}

fn fmt_connected(addr: u64) -> String {
    match BluetoothLink.connected(addr) {
        Ok(c) => format!(
            "buds connected to this PC: {} (as Windows reports it)",
            yes_no(c)
        ),
        Err(e) => format!("buds connection state unknown: {e}"),
    }
}

/// The state of every crossc device, as far as this process may see it.
pub fn device_status() -> Vec<String> {
    let paths = match device::list_interfaces() {
        Ok(p) => p,
        Err(e) => return vec![format!("devices: cannot list them: {e}")],
    };
    if paths.is_empty() {
        return vec!["devices: no crossc device interface present".into()];
    }
    let mut out = Vec::new();
    for path in &paths {
        out.push(format!("device {}: {path}", short_name(path)));
        let opened = Device::open(path).and_then(|d| d.info().map(|i| (d, i)));
        let (dev, info) = match opened {
            Ok(v) => v,
            Err(e) => {
                out.push(format!("  state not accessible: {e}"));
                if let Some(addr) = addr_from_path(path) {
                    out.push(format!("  {}", fmt_connected(addr)));
                }
                continue;
            }
        };
        out.push(format!("  {}", fmt_info(&info)));
        if info.protocol_version < 2 {
            out.push("  audio: this driver has no audio path (protocol v2 needed)".into());
        } else {
            match dev.audio_state() {
                Ok(s) => out.push(format!("  audio: {}", fmt_audio(&s))),
                Err(e) => out.push(format!("  audio: AUDIO_GET_STATE failed: {e}")),
            }
        }
        if info.flags & ioc::INFO_FLAG_ADDR_VALID != 0 {
            out.push(format!("  {}", fmt_connected(info.remote_addr)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossc_ioctl::audio::{AUDIO_FLAG_ACX, AUDIO_FLAG_ATTACHED, AUDIO_FLAG_LINK, PcmFormat};

    #[test]
    fn info_and_audio_lines() {
        let i = ioc::InfoReply {
            protocol_version: 2,
            driver_version: 1 << 16,
            remote_addr: 0xAABB_CCDD_EEFF,
            channels_open: 2,
            channels_max: 4,
            flags: ioc::INFO_FLAG_D0 | ioc::INFO_FLAG_ADDR_VALID,
        };
        assert_eq!(
            fmt_info(&i),
            "driver 0.1.0, protocol v2, buds AA:BB:CC:DD:EE:FF, 2/4 channels open, D0"
        );
        assert!(fmt_info(&ioc::InfoReply::default()).contains("address unknown"));
        let s = AudioState {
            seq: 9,
            flags: AUDIO_FLAG_ACX | AUDIO_FLAG_ATTACHED | AUDIO_FLAG_LINK,
            stream: StreamState::Running,
            format: PcmFormat {
                sample_rate: 96000,
                channels: 2,
                container_bits: 32,
                valid_bits: 24,
            },
            frames_rendered: 1000,
            frames_dropped: 7,
            volume: Default::default(),
            default_format: crossc_ioctl::audio::OFFERED_FORMATS[1],
        };
        assert_eq!(
            fmt_audio(&s),
            "endpoint on, default format 96000 Hz/24, service attached: yes, link: up, stream: running (96000 Hz, 2 ch, 24-bit in 32), frames rendered 1000 / dropped 7; volume: no hardware volume (Windows scales the PCM)"
        );
        let idle = fmt_audio(&AudioState::default());
        assert!(
            idle.contains(
                "endpoint off (ACX disabled), service attached: no, link: down, stream: none"
            ),
            "{idle}"
        );
    }

    #[test]
    fn address_from_the_interface_path() {
        let p = r"\\?\BTHENUM#{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013#b&1a2b3c4d&0&AABBCCDDEEFF_C00000000#{433f9eba-036f-480d-826f-e0ec953319a9}";
        assert_eq!(addr_from_path(p), Some(0xAABB_CCDD_EEFF));
        assert_eq!(addr_from_path(r"\\?\ROOT#crossc#0000#{guid}"), None);
    }

    #[test]
    fn device_status_never_fails() {
        // With or without devices, elevated or not: lines, not a panic.
        let lines = device_status();
        assert!(!lines.is_empty());
        assert!(lines[0].starts_with("device"), "{lines:?}");
    }

    #[test]
    fn a_session_thread_ends_when_asked_even_without_a_device() {
        let stop = StopSignal::new();
        let log: Arc<dyn Log> = Arc::new(crossc_core::log::NullLog);
        let hub = crate::status::new_hub(String::new());
        let host = RealHost::new(Settings::default(), log, hub.clone());
        let path = r"\\?\crossc-no-such-device";
        let w = host.spawn(path, stop.clone());
        // Its status is in the hub meanwhile, under the log prefix: there
        // from the start, with the reason once the first attempt has failed.
        let t0 = std::time::Instant::now();
        let failed = |hub: &Hub| {
            let d = hub.snapshot().devices;
            d.first().is_some_and(|d| !d.phase_reason.is_empty())
        };
        while !failed(&hub) && t0.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!w.finished(), "it keeps retrying the device");
        let devices = hub.snapshot().devices;
        assert_eq!(devices.len(), 1, "{devices:?}");
        let d = &devices[0];
        assert_eq!(d.id, short_name(path));
        assert_eq!(d.phase, crossc_status::LinkPhase::WaitingDriver);
        assert!(
            d.phase_reason.contains("cannot open the device"),
            "{}",
            d.phase_reason
        );
        assert_eq!((d.name.as_str(), &d.link, &d.driver), ("", &None, &None));
        stop.request();
        w.join();
        assert!(hub.snapshot().devices.is_empty(), "gone with its session");
        host.sessions_stopped();
    }
}
