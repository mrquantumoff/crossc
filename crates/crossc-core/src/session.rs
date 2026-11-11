//! The session: one crossc device, from "is the driver there" to an SSC
//! stream that follows what Windows plays into the endpoint.
//!
//! ```text
//!  WaitDriver --attach ok--> WaitLink --buds connected, configured--> Ready
//!      ^                        ^  ^                                  |  ^
//!      |                        |  |              START (audio runs)  v  | SUSPEND (idle 3 s,
//!      |  detached / audio      |  +--- sink CLOSE / ABORT, --- Streaming-+  or the sink's own)
//!      |  path error            |       channel lost, write error
//!      +------------------------+------ (from any state)
//! ```
//!
//! * **WaitDriver**: GET_INFO (protocol >= 2, remote address known),
//!   AUDIO_GET_STATE (ACX on), AUDIO_ATTACH. Anything missing is logged once
//!   and retried every [`DRIVER_RETRY`].
//! * **WaitLink**: every [`LINK_POLL`] ask the [`LinkProbe`] whether the buds
//!   are connected; no L2CAP channel is opened while the answer is no. When
//!   yes (and the backoff of an earlier failure has passed): open signaling,
//!   DISCOVER + capabilities, pick the SSC SEP, choose the [`Plan`] for the
//!   endpoint's current (or last, or default 48 kHz/24) format,
//!   SET_CONFIGURATION, OPEN, open the media channel, AUDIO_SET_LINK(true).
//! * **Ready**: the stream is open and the endpoint shows as plugged in.
//!   When the audio engine starts rendering: START.
//! * **Streaming**: read PCM, assemble 864-sample frames, encode, send one
//!   RTP packet per frame as it completes. PCM the driver still holds from
//!   before the START is skipped, unless the endpoint started rendering
//!   less than half a second earlier (then it is the start of the audio). [`IDLE_SUSPEND`] after rendering
//!   stops: SUSPEND, back to Ready. A sink SUSPEND is honoured, and START is
//!   retried every [`START_RETRY`] while audio is still playing. A START the
//!   sink sends is accepted and counts like an accepted START of ours: the
//!   stream is Streaming (and is suspended again if nothing plays).
//! * A format change that needs another stream configuration: CLOSE,
//!   SET_CONFIGURATION, OPEN, new media channel (START again if rendering).
//! * Sink CLOSE / ABORT, a lost channel or a failed media write:
//!   AUDIO_SET_LINK(false), close the channels, back to WaitLink, retrying
//!   after 1 s, 2 s, 5 s, then every 10 s.
//! * The ATTACH request ending, or the audio requests failing: back to
//!   WaitDriver.
//! * Stop request (any state): CLOSE the stream, close the channels,
//!   AUDIO_SET_LINK(false), cancel ATTACH.
//!
//! Commands the sink sends on the signaling channel are answered in every
//! state that has the channel open ([`crate::peer`]). Nothing here panics:
//! every failure is logged and leads to one of the states above.

use std::sync::Arc;
use std::time::Duration;

use avdtp::caps;
use crossc_ioctl as ioc;
use crossc_ioctl::audio::{AudioState, PCM_FLAG_DISCONTINUITY, PcmFormat, StreamState};

use crate::driver::{Driver, DriverChannel, Interest, LinkProbe, PcmChunk, Wake};
use crate::log::{Level, Log, RateLimit};
use crate::media::{self, Choice, INT_SEID, StepError};
use crate::pace::Clock;
use crate::peer::{LocalStream, PeerEvent};
use crate::plan::{Plan, select_plan};
use crate::rtp::media_mtu_limit;
use crate::settings::Settings;
use crate::signaling::{self, Signaling};
use crate::ssc::{SscSep, choose_ssc, local_caps};
use crate::stop::StopSignal;
use crate::stream::{PipeError, Pipeline};

/// How often the driver is retried while it cannot be attached.
pub const DRIVER_RETRY: Duration = Duration::from_secs(10);
/// How often the buds' connection state is checked.
pub const LINK_POLL: Duration = Duration::from_secs(2);
/// Rendering has to be stopped this long before the stream is suspended.
pub const IDLE_SUSPEND: Duration = Duration::from_secs(3);
/// Between START attempts after the sink suspended or refused.
pub const START_RETRY: Duration = Duration::from_secs(2);
/// Stream statistics are logged this often while streaming.
pub const STATS_EVERY: Duration = Duration::from_secs(60);
/// Delays before reconnecting after a link failure; the last one repeats.
pub const BACKOFF: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];
/// A link that stayed up this long starts the backoff from the beginning.
pub const STABLE_LINK: Duration = Duration::from_secs(10);
/// Retry interval when the buds have no SSC mode we can send.
pub const UNSUPPORTED_RETRY: Duration = Duration::from_secs(60);
/// START refusals in a row (without the sink having suspended) before the
/// link is rebuilt.
pub const MAX_START_FAILURES: u32 = 5;
/// The same error is logged at most once per this period.
pub const ERROR_LOG_PERIOD: Duration = Duration::from_secs(60);
/// in_mtu requested for both channels: what `crossc-cli play` uses, so the
/// media MTU rule sees the values it was validated with.
pub const REQUESTED_IN_MTU: u16 = 1021;
/// L2CAP connect timeout. The buds are connected when we open, so this is
/// short of the driver's own paging timeout.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
/// Format assumed while no render stream has existed yet.
pub const DEFAULT_FORMAT: PcmFormat = PcmFormat {
    sample_rate: 48000,
    channels: 2,
    container_bits: 32,
    valid_bits: 24,
};
/// Longest single wait (bounds how late a timer can be noticed).
const MAX_WAIT: Duration = Duration::from_secs(5);

/// Where the session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    WaitDriver,
    WaitLink,
    Ready,
    Streaming,
    Stopped,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::WaitDriver => "wait-driver",
            Phase::WaitLink => "wait-link",
            Phase::Ready => "ready",
            Phase::Streaming => "streaming",
            Phase::Stopped => "stopped",
        }
    }
}

/// Counters of the current statistics interval.
#[derive(Clone, Copy, Debug, Default)]
struct Stats {
    packets: u64,
    frames_in: u64,
    discontinuities: u64,
    write_total: Duration,
    write_max: Duration,
    /// The driver's dropped-frames counter at the start of the interval.
    dropped_base: u64,
}

/// An open, configured stream to the buds.
struct Link<D: Driver> {
    sig: Signaling<DriverChannel<D>>,
    media: DriverChannel<D>,
    sep: SscSep,
    plan: Plan,
    /// The endpoint format `plan` and `pipe` are for.
    fmt: PcmFormat,
    pipe: Pipeline,
    mtu_limit: usize,
    started: bool,
    /// PCM rendered before this position predates the current START and is
    /// not sent (see [`Session::start`]).
    skip_before: u64,
    /// Frames skipped for that reason since START (for the log).
    skipped: u64,
    /// Since when the link is up (for the backoff).
    since: Duration,
    /// Rendering stopped at this time while the stream was started.
    idle_since: Option<Duration>,
    /// Not before this time is START tried again.
    next_start: Duration,
    start_failures: u32,
    sink_suspended: bool,
    stats: Stats,
    next_stats: Duration,
}

/// What the sink last did to the stream's state on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SinkMove {
    Started,
    Suspended,
}

/// What a step decided about the link.
enum Flow {
    Keep,
    /// Tear the link down and reconnect. `release`: the stream is still
    /// ours to CLOSE.
    Down {
        reason: String,
        release: bool,
    },
    /// The driver ended the session.
    Detached(String),
}

fn down(reason: impl Into<String>, release: bool) -> Flow {
    Flow::Down {
        reason: reason.into(),
        release,
    }
}

/// Why setting up the link failed.
struct Fail {
    msg: String,
    /// The buds cannot take any mode we send: retry rarely.
    unsupported: bool,
}

fn fail(msg: impl Into<String>) -> Fail {
    Fail {
        msg: msg.into(),
        unsupported: false,
    }
}

fn fmt_format(f: PcmFormat) -> String {
    format!(
        "{} Hz, {} ch, {}-bit in {}",
        f.sample_rate, f.channels, f.valid_bits, f.container_bits
    )
}

fn fmt_addr(addr: u64) -> String {
    let mut a = [0u8; 17];
    ioc::format_bd_addr(addr, &mut a);
    String::from_utf8_lossy(&a).into_owned()
}

/// The session state machine. Single-threaded: [`Self::run`] drives it until
/// the stop request; [`Self::step`] performs one wait-and-react iteration.
pub struct Session<D: Driver, P: LinkProbe, K: Clock> {
    drv: D,
    probe: P,
    clock: K,
    settings: Settings,
    log: Arc<dyn Log>,
    stop: StopSignal,
    /// Prefix of every log line (which device this is).
    name: String,
    phase: Phase,
    limiter: RateLimit,
    /// Remote BD_ADDR from GET_INFO.
    addr: u64,
    /// Last audio state seen.
    audio: AudioState,
    /// Last valid stream format seen.
    last_format: Option<PcmFormat>,
    /// Render position (in `frames_rendered` numbering) at which the
    /// endpoint was last seen to start rendering.
    run_start: u64,
    next_driver_try: Duration,
    /// Index into [`BACKOFF`] for the next link failure.
    backoff: usize,
    /// No link attempt before this time.
    retry_at: Duration,
    next_probe: Duration,
    /// Last answer of the link probe (for logging changes only).
    was_connected: Option<bool>,
    link: Option<Link<D>>,
    /// Last delay the sink reported, 1/10 ms.
    last_delay: Option<u16>,
    stats_every: Duration,
}

impl<D: Driver, P: LinkProbe, K: Clock> Session<D, P, K> {
    pub fn new(
        drv: D,
        probe: P,
        clock: K,
        settings: Settings,
        log: Arc<dyn Log>,
        stop: StopSignal,
        name: &str,
    ) -> Self {
        Session {
            drv,
            probe,
            clock,
            settings,
            log,
            stop,
            name: name.to_string(),
            phase: Phase::WaitDriver,
            limiter: RateLimit::new(ERROR_LOG_PERIOD),
            addr: 0,
            audio: AudioState::default(),
            last_format: None,
            run_start: 0,
            next_driver_try: Duration::ZERO,
            backoff: 0,
            retry_at: Duration::ZERO,
            next_probe: Duration::ZERO,
            was_connected: None,
            link: None,
            last_delay: None,
            stats_every: STATS_EVERY,
        }
    }

    /// Log stream statistics this often instead of every [`STATS_EVERY`].
    pub fn set_stats_period(&mut self, period: Duration) {
        self.stats_every = period.max(Duration::from_millis(100));
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The plan of the current link, if there is one.
    pub fn plan(&self) -> Option<Plan> {
        self.link.as_ref().map(|l| l.plan)
    }

    /// Last delay the sink reported, in 1/10 ms.
    pub fn sink_delay(&self) -> Option<u16> {
        self.last_delay
    }

    /// Run until the stop request has been honoured.
    pub fn run(&mut self) {
        while self.phase != Phase::Stopped {
            self.step();
        }
    }

    /// One iteration: wait for the next thing and react to it.
    pub fn step(&mut self) {
        if self.phase == Phase::Stopped {
            return;
        }
        if self.stop.is_set() {
            self.shutdown();
            return;
        }
        match self.phase {
            Phase::WaitDriver => self.step_wait_driver(),
            Phase::WaitLink => self.step_wait_link(),
            Phase::Ready | Phase::Streaming => self.step_linked(),
            Phase::Stopped => {}
        }
    }

    // -- logging ------------------------------------------------------------

    fn say(&self, level: Level, msg: &str) {
        if self.log.enabled(level) {
            self.log.log(level, &format!("[{}] {msg}", self.name));
        }
    }
    fn info(&self, msg: &str) {
        self.say(Level::Info, msg);
    }
    fn warn(&self, msg: &str) {
        self.say(Level::Warn, msg);
    }

    /// Log an error, but the same kind (`key`) only once per
    /// [`ERROR_LOG_PERIOD`].
    fn error_once(&mut self, key: &str, msg: &str) {
        if let Some(held) = self.limiter.pass(key, self.clock.now()) {
            if held > 0 {
                self.say(
                    Level::Error,
                    &format!("{msg} (repeated {held} more time(s) since last logged)"),
                );
            } else {
                self.say(Level::Error, msg);
            }
        }
    }

    fn set_phase(&mut self, p: Phase) {
        if p != self.phase {
            self.info(&format!("state {} -> {}", self.phase.name(), p.name()));
            self.phase = p;
        }
    }

    /// Move the signaling trace to the debug log (it must not pile up).
    fn drain_trace(&self, sig: &mut Signaling<DriverChannel<D>>) {
        if self.log.enabled(Level::Debug) {
            for l in sig.trace.drain(..) {
                self.say(Level::Debug, &format!("avdtp {l}"));
            }
        } else {
            sig.trace.clear();
        }
    }

    // -- WaitDriver ---------------------------------------------------------

    fn step_wait_driver(&mut self) {
        let now = self.clock.now();
        if now < self.next_driver_try {
            // Only the stop request can end this wait early.
            let _ = self
                .drv
                .wait(&Interest::default(), self.next_driver_try - now);
            return;
        }
        match self.try_attach() {
            Ok(()) => {
                self.limiter.forget("driver");
                self.retry_at = now;
                self.next_probe = now;
                self.was_connected = None;
                self.backoff = 0;
                self.set_phase(Phase::WaitLink);
            }
            Err(msg) => {
                self.error_once(
                    "driver",
                    &format!(
                        "driver not ready: {msg}; retrying every {} s",
                        DRIVER_RETRY.as_secs()
                    ),
                );
                self.next_driver_try = now + DRIVER_RETRY;
            }
        }
    }

    fn try_attach(&mut self) -> Result<(), String> {
        let info = match self.drv.info() {
            Ok(i) => i,
            Err(_) => {
                // Not open yet, or the handle went stale: open it afresh.
                self.drv
                    .reopen()
                    .map_err(|e| format!("cannot open the device: {e}"))?;
                self.drv.info().map_err(|e| format!("GET_INFO: {e}"))?
            }
        };
        if info.protocol_version < 2 {
            return Err(format!(
                "the driver speaks protocol v{} and has no audio path (v2 needed)",
                info.protocol_version
            ));
        }
        if info.flags & ioc::INFO_FLAG_ADDR_VALID == 0 {
            return Err("the driver does not know the buds' Bluetooth address".into());
        }
        let st = self
            .drv
            .audio_state()
            .map_err(|e| format!("AUDIO_GET_STATE: {e}"))?;
        if !st.acx() {
            return Err("the driver's audio endpoint is off (ACX disabled)".into());
        }
        self.drv
            .attach()
            .map_err(|e| format!("AUDIO_ATTACH: {e}"))?;
        self.addr = info.remote_addr;
        self.note_state(st);
        self.info(&format!(
            "attached: driver {}.{}.{} (protocol v{}), buds {}",
            info.driver_version >> 24,
            (info.driver_version >> 16) & 0xFF,
            info.driver_version & 0xFFFF,
            info.protocol_version,
            fmt_addr(info.remote_addr)
        ));
        Ok(())
    }

    fn note_state(&mut self, s: AudioState) {
        // A run starts when rendering starts, and also when the format
        // changes under it (a new stream; the states in between may have
        // been missed).
        if s.stream == StreamState::Running
            && (self.audio.stream != StreamState::Running || self.audio.format != s.format)
        {
            self.run_start = s.frames_rendered;
        }
        self.audio = s;
        if s.format.is_valid() {
            self.last_format = Some(s.format);
        }
    }

    /// The driver ended the session (or its audio requests fail): drop
    /// everything and start over at WaitDriver.
    fn on_detached(&mut self, why: &str) {
        self.error_once(
            "detached",
            &format!(
                "the driver detached the session ({why}); retrying every {} s",
                DRIVER_RETRY.as_secs()
            ),
        );
        // The driver has already closed the channels; closing them again
        // just fails.
        self.link = None;
        self.drv.detach();
        self.audio = AudioState::default();
        self.next_driver_try = self.clock.now() + DRIVER_RETRY;
        self.set_phase(Phase::WaitDriver);
    }

    // -- WaitLink -----------------------------------------------------------

    fn step_wait_link(&mut self) {
        let now = self.clock.now();
        let due = self.retry_at.max(self.next_probe);
        if now < due {
            let interest = Interest {
                seq: Some(self.audio.seq),
                ..Interest::default()
            };
            match self.drv.wait(&interest, (due - now).min(MAX_WAIT)) {
                Wake::Detached(e) | Wake::StateError(e) => self.on_detached(&e.to_string()),
                Wake::State(s) => self.note_state(s),
                _ => {}
            }
            return;
        }
        self.next_probe = now + LINK_POLL;
        match self.probe.connected(self.addr) {
            Ok(true) => {
                self.limiter.forget("probe");
                if self.was_connected != Some(true) {
                    self.info("the buds are connected");
                }
                self.was_connected = Some(true);
            }
            Ok(false) => {
                if self.was_connected != Some(false) {
                    self.info("the buds are not connected; waiting for them");
                }
                self.was_connected = Some(false);
                // Whatever failed before, a new connection starts afresh.
                self.backoff = 0;
                self.retry_at = now;
                return;
            }
            Err(e) => {
                self.error_once(
                    "probe",
                    &format!("cannot tell whether the buds are connected: {e}"),
                );
                return;
            }
        }
        match self.configure() {
            Ok(link) => {
                self.limiter.forget("link");
                self.info(&format!("link up: {}", link.plan.describe()));
                self.link = Some(link);
                self.set_phase(Phase::Ready);
            }
            Err(f) => {
                let now = self.clock.now();
                let wait = if f.unsupported {
                    UNSUPPORTED_RETRY
                } else {
                    self.next_backoff()
                };
                self.retry_at = now + wait;
                self.next_probe = self.retry_at;
                self.error_once(
                    "link",
                    &format!(
                        "cannot set up the stream: {}; next attempt in {} s",
                        f.msg,
                        wait.as_secs()
                    ),
                );
            }
        }
    }

    fn next_backoff(&mut self) -> Duration {
        let d = BACKOFF[self.backoff.min(BACKOFF.len() - 1)];
        self.backoff = (self.backoff + 1).min(BACKOFF.len());
        d
    }

    fn open_channel(&mut self, what: &str, send_cap: usize) -> Result<DriverChannel<D>, Fail> {
        let req = ioc::OpenChannelRequest {
            psm: avdtp::PSM_AVDTP,
            in_mtu: REQUESTED_IN_MTU,
            out_mtu_max: 0,
            flags: 0,
        };
        DriverChannel::open(&self.drv, req, OPEN_TIMEOUT, send_cap)
            .map_err(|e| fail(format!("opening the {what} channel: {e}")))
    }

    /// Everything from the signaling channel to AUDIO_SET_LINK(true).
    fn configure(&mut self) -> Result<Link<D>, Fail> {
        let st = self
            .drv
            .audio_state()
            .map_err(|e| fail(format!("AUDIO_GET_STATE: {e}")))?;
        self.note_state(st);
        let fmt = if st.format.is_valid() {
            st.format
        } else {
            self.last_format.unwrap_or(DEFAULT_FORMAT)
        };
        // Signaling SDUs are small; never rely on the reported MTU for more
        // than the L2CAP default.
        let ch = self.open_channel("signaling", usize::from(ioc::L2CAP_DEFAULT_MTU))?;
        let mut sig = Signaling::new(ch);
        let reports = signaling::probe(&mut sig);
        self.drain_trace(&mut sig);
        let reports = reports.map_err(|e| fail(format!("capability probe: {e}")))?;
        let sep = choose_ssc(&reports).map_err(|msg| Fail {
            msg,
            unsupported: true,
        })?;
        self.info(&format!(
            "SSC SEP {:#04x}: caps {:#04x} [{}]{}",
            sep.seid,
            sep.flags.0,
            sep.flags.names().collect::<Vec<_>>().join(", "),
            if sep.delay_reporting {
                ", Delay Reporting"
            } else {
                ""
            }
        ));
        let plan = select_plan(fmt, sep.flags, &self.settings).map_err(|e| Fail {
            msg: e.to_string(),
            unsupported: true,
        })?;
        let (media, plan, mtu_limit) = self.open_stream(&mut sig, &sep, fmt, plan)?;
        let pipe = match Pipeline::new(fmt, &plan) {
            Ok(p) => p,
            Err(e) => {
                self.release(&mut sig, sep.seid);
                return Err(fail(e));
            }
        };
        if let Err(e) = self.drv.set_link(true) {
            self.release(&mut sig, sep.seid);
            return Err(fail(format!("AUDIO_SET_LINK: {e}")));
        }
        let now = self.clock.now();
        Ok(Link {
            sig,
            media,
            sep,
            plan,
            fmt,
            pipe,
            mtu_limit,
            started: false,
            skip_before: 0,
            skipped: 0,
            since: now,
            idle_since: None,
            next_start: now,
            start_failures: 0,
            sink_suspended: false,
            stats: Stats::default(),
            next_stats: now,
        })
    }

    /// Handle what the sink said meanwhile. Returns the event that ends the
    /// stream (CLOSE / ABORT), if any; a SUSPEND or START is reported
    /// through `moved` (the last one, if the sink sent several).
    fn take_events(
        &mut self,
        sig: &mut Signaling<DriverChannel<D>>,
        moved: &mut Option<SinkMove>,
    ) -> Option<PeerEvent> {
        self.drain_trace(sig);
        let mut ended = None;
        for e in sig.events.drain(..) {
            match e {
                PeerEvent::Delay { delay, .. } => {
                    self.last_delay = Some(delay);
                    self.info(&e.to_string());
                }
                PeerEvent::Suspended => *moved = Some(SinkMove::Suspended),
                PeerEvent::Started => *moved = Some(SinkMove::Started),
                PeerEvent::Closed | PeerEvent::Aborted => {
                    ended.get_or_insert(e);
                }
            }
        }
        ended
    }

    /// CLOSE (or ABORT) a stream that is still ours. Best effort.
    fn release(&mut self, sig: &mut Signaling<DriverChannel<D>>, seid: u8) {
        match media::release(sig, seid) {
            Ok(how) => self.info(&format!("stream released with {how}")),
            Err(e) => self.warn(&format!("releasing the stream failed: {e}")),
        }
        self.drain_trace(sig);
        sig.events.clear();
    }

    /// SET_CONFIGURATION, OPEN and the media channel for `plan`. On failure
    /// nothing is left configured on the sink (as far as it still listens).
    fn open_stream(
        &mut self,
        sig: &mut Signaling<DriverChannel<D>>,
        sep: &SscSep,
        fmt: PcmFormat,
        plan: Plan,
    ) -> Result<(DriverChannel<D>, Plan, usize), Fail> {
        let config = caps::ssc_configuration(plan.config, sep.delay_reporting);
        sig.stream = Some(LocalStream {
            int_seid: INT_SEID,
            config: config.clone(),
            caps: local_caps(),
        });
        self.info(&format!(
            "configuring SEP {:#04x} for endpoint format {}: {}{}",
            sep.seid,
            fmt_format(fmt),
            plan.describe(),
            if sep.delay_reporting {
                ", with Delay Reporting"
            } else {
                ""
            }
        ));
        let choice = Choice {
            acp_seid: sep.seid,
            config,
        };
        // A SUSPEND or START from the sink means nothing before the stream
        // is open (everything collected here arrived before OPEN's answer).
        let mut premature = None;
        let r = media::set_configuration(sig, &choice);
        let ended = self.take_events(sig, &mut premature);
        r.map_err(|e| fail(e.to_string()))?;
        if let Some(e) = ended {
            return Err(fail(format!("{e} right after SET_CONFIGURATION")));
        }
        let r = media::open(sig, sep.seid);
        let ended = self.take_events(sig, &mut premature);
        if let Err(e) = r {
            // CLOSE is not valid before OPEN: ABORT, unless the sink has
            // dropped the stream itself.
            if ended.is_none() && !matches!(e, StepError::Chan(_)) {
                if let Err(a) = media::abort(sig, sep.seid) {
                    self.warn(&format!("ABORT after a refused OPEN failed: {a}"));
                }
                self.drain_trace(sig);
                sig.events.clear();
            }
            return Err(fail(match ended {
                Some(ev) => format!("{e} ({ev})"),
                None => e.to_string(),
            }));
        }
        if let Some(e) = ended {
            return Err(fail(format!("{e} right after OPEN")));
        }
        let media = match self.open_channel("media", usize::MAX) {
            Ok(m) => m,
            Err(f) => {
                self.release(sig, sep.seid);
                return Err(f);
            }
        };
        let Some((limit, why)) = media_mtu_limit(media.out_mtu, media.in_mtu, REQUESTED_IN_MTU)
        else {
            self.release(sig, sep.seid);
            return Err(fail(format!(
                "the media channel reported no plausible MTU (out_mtu={} in_mtu={})",
                media.out_mtu, media.in_mtu
            )));
        };
        let mut plan = plan;
        match plan.fit_mtu(limit) {
            Ok(None) => {}
            Ok(Some(old)) => self.warn(&format!(
                "{} kbps does not fit the media MTU limit {limit}; using {} kbps",
                old.bps() / 1000,
                plan.bitrate.bps() / 1000
            )),
            Err(e) => {
                self.release(sig, sep.seid);
                return Err(fail(e));
            }
        }
        self.info(&format!(
            "media channel open: out_mtu={} in_mtu={}, packet {} bytes, limit {limit} ({why})",
            media.out_mtu,
            media.in_mtu,
            plan.packet_bytes()
        ));
        Ok((media, plan, limit))
    }

    // -- Ready / Streaming --------------------------------------------------

    fn step_linked(&mut self) {
        let Some(mut link) = self.link.take() else {
            // Cannot happen; recover instead of trusting that.
            self.set_phase(Phase::WaitLink);
            return;
        };
        match self.linked(&mut link) {
            Flow::Keep => self.link = Some(link),
            Flow::Down { reason, release } => self.teardown(link, &reason, release),
            Flow::Detached(why) => {
                self.link = Some(link);
                self.on_detached(&why);
            }
        }
    }

    fn linked(&mut self, link: &mut Link<D>) -> Flow {
        let now = self.clock.now();
        let running = self.audio.stream == StreamState::Running;
        // Things that are due now.
        if link.started {
            if running {
                link.idle_since = None;
            } else {
                let since = *link.idle_since.get_or_insert(now);
                if now >= since + IDLE_SUSPEND {
                    return self.suspend(link);
                }
            }
            if now >= link.next_stats {
                self.log_stats(link, now);
            }
        } else if running && now >= link.next_start {
            return self.start(link);
        }
        // Wait for the next thing.
        let mut timeout = MAX_WAIT;
        if link.started {
            timeout = timeout.min(link.next_stats.saturating_sub(now));
            if let Some(since) = link.idle_since {
                timeout = timeout.min((since + IDLE_SUSPEND).saturating_sub(now));
            }
        } else if running {
            timeout = timeout.min(link.next_start.saturating_sub(now));
        }
        let interest = Interest {
            signaling: Some(link.sig.ch.id),
            seq: Some(self.audio.seq),
            pcm: link.started,
        };
        match self
            .drv
            .wait(&interest, timeout.max(Duration::from_millis(1)))
        {
            Wake::Timeout | Wake::Stop => Flow::Keep,
            Wake::Detached(e) | Wake::StateError(e) => Flow::Detached(e.to_string()),
            Wake::State(s) => self.on_state(link, s),
            Wake::Signaling(Ok(sdu)) => match link.sig.handle_sdu(&sdu) {
                Ok(()) => self.on_peer(link),
                Err(e) => down(format!("signaling channel: {e}"), false),
            },
            Wake::Signaling(Err(e)) => down(format!("signaling channel lost: {e}"), false),
            Wake::Pcm(Ok(chunk)) => self.on_pcm(link, chunk),
            Wake::Pcm(Err(e)) => Flow::Detached(format!("AUDIO_READ_PCM: {e}")),
        }
    }

    /// React to what the sink sent (events collected by the signaling
    /// client, during a request or between requests).
    fn on_peer(&mut self, link: &mut Link<D>) -> Flow {
        let mut moved = None;
        if let Some(e) = self.take_events(&mut link.sig, &mut moved) {
            return down(e.to_string(), false);
        }
        let now = self.clock.now();
        match moved {
            Some(SinkMove::Suspended) => {
                link.sink_suspended = true;
                link.next_start = now + START_RETRY;
                link.start_failures = 0;
                if link.started {
                    self.stream_stopped(link);
                    self.info(&format!(
                        "the sink suspended the stream; START is retried every {} s while audio plays",
                        START_RETRY.as_secs()
                    ));
                    self.set_phase(Phase::Ready);
                }
            }
            // We accepted the sink's START (see `peer`), so the stream is
            // streaming: a START of ours would be refused from here on.
            Some(SinkMove::Started) if !link.started => {
                self.info("the sink started the stream");
                self.begin_streaming(link, now);
            }
            Some(SinkMove::Started) | None => {}
        }
        Flow::Keep
    }

    /// Bookkeeping when the stream leaves the started state.
    fn stream_stopped(&mut self, link: &mut Link<D>) {
        link.started = false;
        link.idle_since = None;
        link.pipe.flush();
        self.drv.flush_pcm();
    }

    /// The stream has entered the started state at `now` (our START was
    /// accepted, or we accepted the sink's).
    fn begin_streaming(&mut self, link: &mut Link<D>, now: Duration) {
        link.started = true;
        link.sink_suspended = false;
        link.start_failures = 0;
        link.idle_since = None;
        link.pipe.flush();
        // The driver's ring may hold audio from before this START.
        // If the endpoint only just started rendering, that is the
        // beginning of what the user plays: keep it. Otherwise (a
        // START retried while audio kept playing, a rebuilt link) it
        // is a backlog that would only add delay: skip up to now.
        let st = self.drv.audio_state().unwrap_or(self.audio);
        let fresh = u64::from(link.fmt.sample_rate / 2);
        link.skip_before = if st.frames_rendered.saturating_sub(self.run_start) <= fresh {
            self.run_start
        } else {
            st.frames_rendered
        };
        link.skipped = 0;
        link.stats = Stats {
            dropped_base: st.frames_dropped,
            ..Stats::default()
        };
        link.next_stats = now + self.stats_every;
        self.limiter.forget("start");
        self.set_phase(Phase::Streaming);
    }

    fn start(&mut self, link: &mut Link<D>) -> Flow {
        let now = self.clock.now();
        let r = media::start(&mut link.sig, link.sep.seid);
        let mut moved = None;
        if let Some(e) = self.take_events(&mut link.sig, &mut moved) {
            return down(e.to_string(), false);
        }
        match (r, moved) {
            (Err(StepError::Chan(e)), _) => down(format!("START: {e}"), false),
            // The sink's own START crossed ours and we accepted it: the
            // stream is started, whatever the sink then answered to ours
            // (a refusal is what AVDTP prescribes for a streaming stream).
            (_, Some(SinkMove::Started)) => {
                self.info("the sink started the stream");
                self.begin_streaming(link, now);
                Flow::Keep
            }
            (Ok(()), Some(SinkMove::Suspended)) => {
                // Accepted and suspended again in the same breath.
                link.sink_suspended = true;
                link.next_start = now + START_RETRY;
                Flow::Keep
            }
            (Ok(()), None) => {
                self.begin_streaming(link, now);
                Flow::Keep
            }
            (Err(StepError::Refused(msg)), _) => {
                link.start_failures += 1;
                link.next_start = now + START_RETRY;
                if !link.sink_suspended && link.start_failures >= MAX_START_FAILURES {
                    return down(
                        format!("{msg} ({} times in a row)", link.start_failures),
                        true,
                    );
                }
                self.error_once(
                    "start",
                    &format!("{msg}; retrying every {} s", START_RETRY.as_secs()),
                );
                Flow::Keep
            }
        }
    }

    fn suspend(&mut self, link: &mut Link<D>) -> Flow {
        let now = self.clock.now();
        let r = media::suspend(&mut link.sig, link.sep.seid);
        // A sink SUSPEND or START that crossed ours arrived before the
        // answer, and the answer settles the state: an accepted SUSPEND
        // leaves the stream open, a refused one rebuilds the link.
        let mut crossed = None;
        if let Some(e) = self.take_events(&mut link.sig, &mut crossed) {
            return down(e.to_string(), false);
        }
        match r {
            Ok(()) => {
                self.log_stats(link, now);
                self.stream_stopped(link);
                link.next_start = now;
                self.info(&format!(
                    "no audio for {} s: stream suspended",
                    IDLE_SUSPEND.as_secs()
                ));
                self.set_phase(Phase::Ready);
                Flow::Keep
            }
            Err(StepError::Chan(e)) => down(format!("SUSPEND: {e}"), false),
            // The sink and we disagree about the stream's state: rebuild it.
            Err(StepError::Refused(msg)) => down(msg, true),
        }
    }

    fn on_state(&mut self, link: &mut Link<D>, s: AudioState) -> Flow {
        self.note_state(s);
        if s.format.is_valid() && s.format != link.fmt {
            return self.on_format(link, s.format);
        }
        Flow::Keep
    }

    /// The endpoint's format is no longer the one the link was built for.
    fn on_format(&mut self, link: &mut Link<D>, fmt: PcmFormat) -> Flow {
        let mut plan = match select_plan(fmt, link.sep.flags, &self.settings) {
            Ok(p) => p,
            Err(e) => return down(format!("endpoint format changed: {e}"), true),
        };
        // Compare like with like: the MTU may have lowered the bitrate.
        if let Err(e) = plan.fit_mtu(link.mtu_limit) {
            return down(e, true);
        }
        if plan.same_stream(&link.plan) {
            self.info(&format!(
                "endpoint format is now {}: same stream, {}",
                fmt_format(fmt),
                plan.describe()
            ));
            link.pipe.retarget(fmt, &plan);
            link.plan = plan;
            link.fmt = fmt;
            return Flow::Keep;
        }
        self.info(&format!(
            "endpoint format is now {}: reconfiguring ({} -> {})",
            fmt_format(fmt),
            link.plan.mode.name(),
            plan.mode.name()
        ));
        if link.started {
            self.stream_stopped(link);
            self.set_phase(Phase::Ready);
        }
        // CLOSE, then the old media channel goes, then the new stream.
        let r = media::release(&mut link.sig, link.sep.seid);
        // The stream is being closed: what the sink did to it is moot.
        let mut moot = None;
        let ended = self.take_events(&mut link.sig, &mut moot);
        if let Err(e) = r {
            return down(format!("CLOSE before reconfiguring: {e}"), false);
        }
        if let Some(e) = ended {
            return down(e.to_string(), false);
        }
        if let Err(e) = link.media.close() {
            self.warn(&format!("closing the old media channel: {e}"));
        }
        let sep = link.sep.clone();
        match self.open_stream(&mut link.sig, &sep, fmt, plan) {
            Ok((media, plan, limit)) => {
                let pipe = match Pipeline::new(fmt, &plan) {
                    Ok(p) => p,
                    Err(e) => return down(e, true),
                };
                link.media = media;
                link.plan = plan;
                link.mtu_limit = limit;
                link.fmt = fmt;
                link.pipe = pipe;
                link.sink_suspended = false;
                link.start_failures = 0;
                link.next_start = self.clock.now();
                self.info(&format!("reconfigured: {}", plan.describe()));
                Flow::Keep
            }
            // open_stream has released what it had set up.
            Err(f) => down(f.msg, false),
        }
    }

    fn on_pcm(&mut self, link: &mut Link<D>, chunk: PcmChunk) -> Flow {
        if !link.started {
            return Flow::Keep; // read ahead before the stream stopped
        }
        let h = chunk.header;
        // Audio from before this START (see `start`): not sent, whatever
        // its format was.
        let end = h.first_frame.saturating_add(u64::from(h.frames));
        if end <= link.skip_before {
            link.skipped += u64::from(h.frames);
            return Flow::Keep;
        }
        let stale = link.skip_before.saturating_sub(h.first_frame);
        // The chunk's own format is what counts; the state event for a
        // format change may still be on its way.
        if h.format != link.fmt {
            match self.on_format(link, h.format) {
                Flow::Keep => {}
                other => return other,
            }
            if !link.started {
                return Flow::Keep; // reconfigured: this audio predates START
            }
        }
        if h.flags & PCM_FLAG_DISCONTINUITY != 0 {
            link.pipe.flush();
            link.stats.discontinuities += 1;
            self.say(
                Level::Debug,
                &format!("discontinuity before frame {}", h.first_frame),
            );
        }
        // A chunk that straddles the START position: its tail is fresh.
        let cut = (stale as usize).saturating_mul(h.format.frame_bytes());
        let data = chunk.data.get(cut..).unwrap_or(&[]);
        link.skipped += stale;
        if link.skipped > 0 {
            self.say(
                Level::Debug,
                &format!("skipped {} frame(s) rendered before START", link.skipped),
            );
            link.skipped = 0;
        }
        link.stats.frames_in += u64::from(h.frames).saturating_sub(stale);
        let clock = &self.clock;
        let media = &mut link.media;
        let stats = &mut link.stats;
        let r = link.pipe.push(data, &mut |pkt| {
            use crate::channel::SduChannel;
            let t0 = clock.now();
            let r = media.send(pkt);
            let dt = clock.now().saturating_sub(t0);
            if r.is_ok() {
                stats.packets += 1;
                stats.write_total += dt;
                stats.write_max = stats.write_max.max(dt);
            }
            r
        });
        match r {
            Ok(_) => Flow::Keep,
            // The signaling channel may well be alive: give the stream back.
            Err(PipeError::Send(e)) => down(format!("media write failed: {e}"), true),
            Err(e @ PipeError::Encoder(_)) => down(e.to_string(), true),
        }
    }

    fn log_stats(&mut self, link: &mut Link<D>, now: Duration) {
        link.next_stats = now + self.stats_every;
        let s = link.stats;
        if s.packets == 0 && s.frames_in == 0 {
            return;
        }
        // The counters do not bump the state's sequence number: ask.
        let dropped = match self.drv.audio_state() {
            Ok(st) => {
                let d = st.frames_dropped.saturating_sub(s.dropped_base);
                link.stats.dropped_base = st.frames_dropped;
                d.to_string()
            }
            Err(_) => "?".into(),
        };
        let avg = s.write_total.as_micros() / u128::from(s.packets.max(1));
        self.info(&format!(
            "stats: packets={} frames_in={} dropped={dropped} discontinuities={} write avg={avg}us max={}us{}",
            s.packets,
            s.frames_in,
            s.discontinuities,
            s.write_max.as_micros(),
            match self.last_delay {
                Some(d) => format!(" sink_delay={}.{}ms", d / 10, d % 10),
                None => String::new(),
            }
        ));
        link.stats = Stats {
            dropped_base: link.stats.dropped_base,
            ..Stats::default()
        };
    }

    /// The link is gone or unusable: unplug the endpoint, give the stream
    /// back if it is still ours, close the channels, reconnect later.
    fn teardown(&mut self, mut link: Link<D>, reason: &str, release: bool) {
        let now = self.clock.now();
        self.warn(&format!("link down: {reason}"));
        if let Err(e) = self.drv.set_link(false) {
            self.error_once("set-link", &format!("AUDIO_SET_LINK(false): {e}"));
        }
        if release {
            self.release(&mut link.sig, link.sep.seid);
        }
        self.drain_trace(&mut link.sig);
        self.drv.flush_pcm();
        if now.saturating_sub(link.since) >= STABLE_LINK {
            self.backoff = 0;
        }
        let Link { sig, media, .. } = link;
        drop(media);
        drop(sig);
        let wait = self.next_backoff();
        self.retry_at = now + wait;
        self.next_probe = now;
        self.info(&format!("reconnecting in {} s", wait.as_secs()));
        self.set_phase(Phase::WaitLink);
    }

    // -- stop ---------------------------------------------------------------

    /// Clean shutdown: CLOSE the stream, close the channels, link flag off,
    /// cancel ATTACH.
    fn shutdown(&mut self) {
        self.info("stop requested");
        if let Some(mut link) = self.link.take() {
            self.release(&mut link.sig, link.sep.seid);
            let Link { sig, media, .. } = link;
            drop(media);
            drop(sig);
        }
        if self.phase != Phase::WaitDriver
            && let Err(e) = self.drv.set_link(false)
        {
            self.warn(&format!("AUDIO_SET_LINK(false): {e}"));
        }
        self.drv.detach();
        self.set_phase(Phase::Stopped);
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
