//! The service's side of the status feed ([`crossc_status`]): what a session
//! knows, converted into the snapshot's types and put into the [`Hub`]; the
//! log's warnings and errors as the "last event"; and the pipe server that
//! sends the hub's state to the tray app.
//!
//! Nothing here may disturb the audio. A session thread only ever calls
//! [`Hub::set_device_keeping_event`] (a short lock, one value replaced, an
//! event set) and it does so at most every [`PUBLISH_EVERY`] while audio
//! streams; the pipe and its clients belong to the server's own thread. A
//! feed that cannot be started or is not trusted is said once in the log and
//! the service goes on without it.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossc_core::driver::{Driver, LinkProbe};
use crossc_core::log::{Level, Log};
use crossc_core::pace::Clock;
use crossc_core::plan::SscMode;
use crossc_core::session::{Phase, Session, SessionStatus};
use crossc_core::volume;
use crossc_status::{
    CodecMode, Counters, DeviceStatus, DriverInfo, Event, EventLevel, Hub, LinkPhase, LinkStatus,
    PcmFormat, ServiceInfo, VolumeMode, VolumeStatus,
};

/// While audio streams a session's status goes into the hub this often
/// (and at once when its phase, bitrate or stream configuration changes).
pub const PUBLISH_EVERY: Duration = Duration::from_millis(250);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The hub of a service that writes its log to `log_dir` (empty: it logs to
/// the console).
pub fn new_hub(log_dir: String) -> Hub {
    Hub::new(ServiceInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_unix: unix_now(),
        log_dir,
        last_event: None,
    })
}

fn saturate<T: TryFrom<u128>>(v: u128, max: T) -> T {
    T::try_from(v).unwrap_or(max)
}

/// Where the snapshot says the session is (INTEGRATION.md 1c).
fn link_phase(st: &SessionStatus) -> LinkPhase {
    match st.phase {
        Phase::WaitDriver => LinkPhase::WaitingDriver,
        // The buds are there and the stream is not: a setup that failed or
        // a link that went down.
        Phase::WaitLink if st.connected == Some(true) => LinkPhase::Reconnecting,
        Phase::WaitLink => LinkPhase::WaitingBuds,
        Phase::Ready => LinkPhase::Ready,
        Phase::Streaming => LinkPhase::Streaming,
        Phase::Stopped => LinkPhase::Stopped,
    }
}

/// A session's status as the snapshot's device entry. `id` is the session's
/// log prefix, `name` the buds' Bluetooth name (empty: not known). The last
/// event is left out: the hub keeps the one the log gave it.
pub fn to_device_status(id: &str, name: &str, st: &SessionStatus) -> DeviceStatus {
    let phase = link_phase(st);
    let link = st.link.as_ref().map(|l| LinkStatus {
        mode: match l.plan.mode {
            SscMode::Uhq => CodecMode::Uhq,
            SscMode::Hifi => CodecMode::Hifi,
            SscMode::Ssc48 | SscMode::Ssc44 => CodecMode::Ssc,
        },
        config: l.plan.config,
        caps: l.caps,
        stream_rate: l.plan.mode.sample_rate(),
        stream_bits: if l.plan.mode.hires() { 24 } else { 16 },
        bitrate_kbps: l.rate.kbps(),
        top_kbps: l.ladder.top().kbps(),
        adaptive: l.ladder.is_adaptive(),
        ladder_kbps: l.ladder.rungs().iter().map(|r| r.kbps()).collect(),
        rung: l.rung as u32,
        frames_per_packet: l.rate.frames_per_packet as u32,
        packet_bytes: l.rate.packet_bytes(l.plan.encoder) as u32,
        mtu_limit: saturate(l.mtu_limit as u128, u32::MAX),
        mtu_reason: l.mtu_why.clone(),
        sink_delay_tenth_ms: st.sink_delay.map(u32::from),
        streaming_ms: l.streaming_for.map(|d| saturate(d.as_millis(), u64::MAX)),
    });
    // Known once the driver has been asked, which attaching does.
    let volume = st.driver.map(|_| {
        let muted = st.audio.muted();
        VolumeStatus {
            mode: match st.volume.mode {
                volume::VolumeMode::Buds => VolumeMode::Buds,
                volume::VolumeMode::Software => VolumeMode::Software,
                volume::VolumeMode::Windows => VolumeMode::Windows,
            },
            // The driver's AVRCP value (0..=127) of the slider. It is 0
            // while muted, and without hardware volume Windows does not
            // say where the slider is.
            level_percent: (st.audio.hw_volume() && !muted).then(|| {
                let avrcp = u32::from(st.audio.volume.avrcp).min(127);
                ((avrcp * 100 + 63) / 127) as u8
            }),
            muted,
        }
    });
    let t = &st.totals;
    DeviceStatus {
        id: id.to_string(),
        name: name.to_string(),
        phase,
        phase_reason: match phase {
            LinkPhase::Ready | LinkPhase::Streaming => String::new(),
            _ => st.reason.clone(),
        },
        endpoint: st.endpoint.map(|f| PcmFormat {
            sample_rate: f.sample_rate,
            bits: f.valid_bits,
            channels: f.channels,
        }),
        link,
        volume,
        counters: Counters {
            packets: t.packets,
            pcm_frames: t.frames_in,
            dropped_frames: t.dropped_frames,
            discontinuities: t.discontinuities,
            write_errors: t.write_errors,
            write_avg_us: saturate(
                t.write_total.as_micros() / u128::from(t.write_count.max(1)),
                u32::MAX,
            ),
            write_max_us: saturate(t.write_max.as_micros(), u32::MAX),
            late_packets: t.late_packets,
            reconnects: t.reconnects,
        },
        driver: st.driver.map(|(v, protocol)| DriverInfo {
            version: format!("{}.{}.{}", v >> 24, (v >> 16) & 0xFF, v & 0xFFFF),
            protocol,
        }),
        last_event: None,
    }
}

/// What makes a status worth publishing at once.
type Key = (
    Phase,
    Option<crossc_core::abr::Rate>,
    Option<crossc_core::plan::Plan>,
);

/// Puts one session's status into the hub as the session goes: at once
/// when something of substance changed, and at most every
/// [`PUBLISH_EVERY`] while audio streams (a step then returns for every
/// chunk of PCM). Outside streaming a step is rare, and each is published.
pub struct Publisher<'a, N: FnMut() -> String> {
    hub: &'a Hub,
    id: &'a str,
    /// Looks the buds' Bluetooth name up.
    lookup: N,
    name: String,
    /// The session's clock at the last publication.
    last_at: Duration,
    last_key: Option<Key>,
    last_phase: LinkPhase,
}

impl<'a, N: FnMut() -> String> Publisher<'a, N> {
    /// Publishes the session's status for the first time: the device must
    /// be in the hub before the session logs its first warning, or that
    /// warning has no device to belong to.
    pub fn start<D: Driver, P: LinkProbe, K: Clock>(
        hub: &'a Hub,
        id: &'a str,
        mut lookup: N,
        session: &Session<D, P, K>,
    ) -> Self {
        let name = lookup();
        let mut p = Publisher {
            hub,
            id,
            lookup,
            name,
            last_at: Duration::ZERO,
            last_key: None,
            last_phase: LinkPhase::Unknown,
        };
        p.publish(session);
        p
    }

    fn publish<D: Driver, P: LinkProbe, K: Clock>(&mut self, session: &Session<D, P, K>) {
        let st = session.status();
        let phase = link_phase(&st);
        // The name may be new once the buds have been seen: ask again when
        // the wait for the driver or for them ends.
        let waited = |p: LinkPhase| matches!(p, LinkPhase::WaitingDriver | LinkPhase::WaitingBuds);
        if waited(self.last_phase) && !waited(phase) {
            self.name = (self.lookup)();
        }
        self.last_phase = phase;
        self.last_at = session.now();
        self.last_key = Some((st.phase, session.rate(), session.plan()));
        self.hub
            .set_device_keeping_event(to_device_status(self.id, &self.name, &st));
    }

    /// One step of the session, then its status if it is due.
    pub fn step<D: Driver, P: LinkProbe, K: Clock>(&mut self, session: &mut Session<D, P, K>) {
        session.step();
        let phase = session.phase();
        let due = phase != Phase::Streaming
            || self.last_key != Some((phase, session.rate(), session.plan()))
            || session.now().saturating_sub(self.last_at) >= PUBLISH_EVERY;
        if due {
            self.publish(session);
        }
    }

    /// The session has ended: its device leaves the hub.
    pub fn finish(self) {
        self.hub.remove_device(self.id);
    }
}

/// Run `session` until its stop request has been honoured, keeping its
/// status in `hub` under `id` meanwhile. `lookup` gives the buds' Bluetooth
/// name (empty: not known).
pub fn run_published<D: Driver, P: LinkProbe, K: Clock>(
    session: &mut Session<D, P, K>,
    hub: &Hub,
    id: &str,
    lookup: impl FnMut() -> String,
) {
    let mut publisher = Publisher::start(hub, id, lookup, session);
    while session.phase() != Phase::Stopped {
        publisher.step(session);
    }
    publisher.finish();
}

/// A log that also keeps the last warning or error for the status feed:
/// a line of a device (`[<id>] ...`) as that device's last event, any other
/// (and one of a device that is not in the hub) as the service's. Every
/// line goes to the wrapped log unchanged.
pub struct StatusLog {
    inner: Arc<dyn Log>,
    hub: Hub,
}

impl StatusLog {
    pub fn new(inner: Arc<dyn Log>, hub: Hub) -> Self {
        StatusLog { inner, hub }
    }
}

impl Log for StatusLog {
    fn log(&self, level: Level, msg: &str) {
        self.inner.log(level, msg);
        let level = match level {
            Level::Warn => EventLevel::Warning,
            Level::Error => EventLevel::Error,
            _ => return,
        };
        let event = |text: &str| Event {
            level,
            time_unix: unix_now(),
            text: text.to_string(),
        };
        // A device the hub does not know (its session thread could not be
        // started, or has just ended in a panic and taken the device with
        // it) has no entry to show the line under: it is the service's
        // then, with the prefix that says which device it is about.
        let placed = msg
            .strip_prefix('[')
            .and_then(|l| l.split_once("] "))
            .is_some_and(|(id, text)| self.hub.set_device_event(id, event(text)));
        if !placed {
            self.hub.set_service_event(Some(event(msg)));
        }
    }
    fn enabled(&self, level: Level) -> bool {
        self.inner.enabled(level)
    }
}

#[cfg(windows)]
pub use win::start_server;

#[cfg(windows)]
mod win {
    use super::*;
    use crossc_status::pipe::{Connection, Held, Problem, Security, Server, ServerOptions, Trust};

    /// How often, and how far apart, the pipe is looked for after the
    /// server was started (it makes the pipe on its own thread).
    const CHECK_ATTEMPTS: u32 = 10;
    const CHECK_GAP: Duration = Duration::from_millis(500);

    /// Open the status pipe `name` the way a client with `trust` does,
    /// waiting for it to appear for at most `attempts` times `gap`.
    /// Nothing is read from it.
    pub fn check_pipe(
        name: &str,
        trust: Trust,
        attempts: u32,
        gap: Duration,
    ) -> Result<(), Problem> {
        let mut last = Problem::NotRunning;
        for k in 0..attempts.max(1) {
            if k > 0 {
                std::thread::sleep(gap);
            }
            match Connection::open(name, trust) {
                Ok(_) => return Ok(()),
                // Not made yet, or every place taken at this moment.
                Err(p @ (Problem::NotRunning | Problem::Busy)) => last = p,
                Err(p) => return Err(p),
            }
        }
        Err(last)
    }

    /// What the log says once about the pipe `name`, given whose it is and
    /// (for the service's own) what opening it like the tray gives.
    pub fn verdict(
        name: &str,
        whose: Held,
        open: impl FnOnce() -> Result<(), Problem>,
    ) -> Option<(Level, String)> {
        match whose {
            Held::Ours => match open() {
                Ok(()) => Some((Level::Info, format!("status pipe {name} is up"))),
                Err(Problem::Untrusted) => Some((
                    Level::Warn,
                    format!(
                        "the status pipe {name} is not owned by SYSTEM or Administrators: the tray app will not trust it (\"Status not trusted\")"
                    ),
                )),
                Err(Problem::NotRunning) => None,
                Err(p) => Some((
                    Level::Warn,
                    format!("the status pipe {name} could not be checked: {p}"),
                )),
            },
            // The server has said that it cannot create the pipe and that
            // it keeps trying. What that means for the user is said here.
            Held::Taken => Some((
                Level::Warn,
                format!(
                    "a pipe named {name} exists that is not this service's: another program created it before the service could. The tray app shows \"Status not trusted\" (or what that program sends, if it runs elevated) until that program is closed; the service takes the name within seconds after that. Audio is not affected"
                ),
            )),
            // Reported by the server (with the reason), or still not tried
            // after all that time: nothing to add.
            Held::Failed | Held::NotYet => None,
        }
    }

    /// Start the status pipe for `hub`. `log` is the plain log, not the
    /// [`StatusLog`]: a line about the feed must not become a change of
    /// what the feed sends.
    ///
    /// Never holds the service up and never fails it: a server that cannot
    /// be started is logged once and `None` returned. Once the server has
    /// the pipe it is opened the way the tray opens it, and the log says
    /// once whether the tray will believe it (it reads only a pipe owned by
    /// SYSTEM or Administrators, which is what LocalSystem's and an
    /// elevated console's objects are owned by). A pipe of that name that
    /// another program made first is not opened at all: it is not the
    /// service's, whoever owns it, and the log says that instead.
    pub fn start_server(hub: &Hub, security: Security, log: Arc<dyn Log>) -> Option<Server> {
        start_server_on(hub, crossc_status::PIPE_NAME, security, log)
    }

    /// [`start_server`] on the pipe `name`.
    pub fn start_server_on(
        hub: &Hub,
        name: &str,
        security: Security,
        log: Arc<dyn Log>,
    ) -> Option<Server> {
        let report_log = log.clone();
        let options = ServerOptions {
            pipe_name: name.to_string(),
            security,
            report: Some(Arc::new(move |line: &str| {
                report_log.log(Level::Warn, line)
            })),
            ..ServerOptions::default()
        };
        let name = name.to_string();
        let server = match Server::start(hub.clone(), options) {
            Ok(s) => s,
            Err(e) => {
                log.log(
                    Level::Warn,
                    &format!(
                        "the status pipe {name} cannot be started ({e}); the tray app will show no status"
                    ),
                );
                return None;
            }
        };
        let held = server.pipe_held();
        let check = move || {
            // Whose pipe the name is comes first: only the service's own
            // is worth opening. (Opened without asking, a pipe another
            // program made first would be judged as if it were ours.)
            let mut whose = held.get();
            for _ in 0..CHECK_ATTEMPTS {
                if whose != Held::NotYet {
                    break;
                }
                std::thread::sleep(CHECK_GAP);
                whose = held.get();
            }
            let open = || check_pipe(&name, Trust::Service, CHECK_ATTEMPTS, CHECK_GAP);
            if let Some((level, line)) = verdict(&name, whose, open) {
                log.log(level, &line);
            }
        };
        // Its own thread: the service does not wait for the pipe. Without
        // the thread there is no check, and nothing else is lost.
        let _ = std::thread::Builder::new()
            .name("status-check".into())
            .spawn(check);
        Some(server)
    }
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod tests;
