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
//!   RTP packet per frame as it completes. The bitrate of each packet is
//!   the adaptive controller's ([`crate::abr`]): it is told how long every
//!   WRITE took, how long the session waited for the packet's audio and
//!   how late the PCM was, and steps down a ladder under the configured
//!   rate while the link is congested (setting `AdaptiveBitrate`; a new
//!   stream configuration starts a new controller at the top). PCM the
//!   driver still holds from
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
//! With a [`FormatSource`] ([`Session::set_format_source`]) the session also
//! makes the driver's default format follow the format picked for the buds
//! in the Windows sound settings, in every state with the driver attached
//! ([`crate::winformat`]): SET_DEFAULT_FORMAT once per change, one log line
//! per change, and a warning when the engine is still rendering the old
//! format [`crate::winformat::APPLY_GRACE`] after the default was set (it
//! takes the new one with its next stream).
//!
//! Commands the sink sends on the signaling channel are answered in every
//! state that has the channel open ([`crate::peer`]). Nothing here panics:
//! every failure is logged and leads to one of the states above.

use std::sync::Arc;
use std::time::Duration;

use avdtp::caps;
use crossc_ioctl as ioc;
use crossc_ioctl::audio::{AudioState, PCM_FLAG_DISCONTINUITY, PcmFormat, StreamState};

use crate::abr::{Abr, Change, Ladder, Lateness, MAX_RUNGS, Obs, Rate, write_error_is_congestion};
use crate::driver::{Driver, DriverChannel, Interest, LinkProbe, PcmChunk, Wake};
use crate::log::{Level, Log, RateLimit};
use crate::media::{self, Choice, INT_SEID, StepError};
use crate::pace::Clock;
use crate::peer::{LocalStream, PeerEvent};
use crate::plan::{Plan, SscMode, select_plan};
use crate::rtp::media_mtu_limit;
use crate::settings::Settings;
use crate::signaling::{self, Signaling};
use crate::ssc::{SscSep, choose_ssc, local_caps};
use crate::stop::StopSignal;
use crate::stream::Pipeline;
use crate::volume::{self, Decision, VolumeMode};
use crate::winformat::{FormatSource, FormatSync, Step, short};

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
/// At most this many bitrate changes are logged per [`ERROR_LOG_PERIOD`]
/// (the controller itself makes no more than that on any link; the limit
/// is for the log's sake should it ever). Changes held back are counted in
/// the next line, and the statistics line always has the current rate.
pub const RATE_LOG_BURST: u32 = 8;
/// A link that is rebuilt within this time after a media WRITE timed out
/// starts at the lowest bitrate of the same mode.
pub const WRITE_ERROR_MEMORY: Duration = Duration::from_secs(60);
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
    /// Worst lateness of the PCM against the endpoint clock.
    late_max: Duration,
    /// The controller's counters at the start of the interval.
    times_base: [Duration; MAX_RUNGS],
    changes_base: u32,
    slow_base: u64,
}

impl Stats {
    /// A new interval: the driver's and the controller's counters as they
    /// are now.
    fn begin(dropped: u64, abr: &Abr) -> Self {
        Stats {
            dropped_base: dropped,
            times_base: abr.time_at(),
            changes_base: abr.changes(),
            slow_base: abr.slow_packets(),
            ..Stats::default()
        }
    }

    /// The interval goes on with another controller.
    fn rebase(&mut self, abr: &Abr) {
        self.times_base = abr.time_at();
        self.changes_base = abr.changes();
        self.slow_base = abr.slow_packets();
    }
}

/// Counters since the session started (the statistics interval's are in
/// `Stats`). The three WRITE fields describe the current stream: they
/// start anew with every START.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// Media packets sent.
    pub packets: u64,
    /// PCM frames taken from the endpoint (and not skipped).
    pub frames_in: u64,
    /// Gaps in the PCM the driver flagged.
    pub discontinuities: u64,
    /// Media WRITEs that failed.
    pub write_errors: u64,
    /// Packets whose PCM was [`crate::abr::LATE`] or more behind the
    /// endpoint clock.
    pub late_packets: u64,
    /// Links that went down and are set up again.
    pub reconnects: u32,
    /// Frames the driver dropped while a stream was started: audio this
    /// session did not read in time. The driver's own counter also counts
    /// what it renders while nothing reads it (no session, a stream that
    /// is not started) and is never reset; this one takes only what that
    /// counter rose by between a START and the end of that stream.
    pub dropped_frames: u64,
    /// Time spent in the media WRITEs of the current stream.
    pub write_total: Duration,
    /// The longest of them.
    pub write_max: Duration,
    /// How many there were.
    pub write_count: u64,
}

/// The current link, as [`Session::status`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkView {
    pub plan: Plan,
    /// The buds' SSC capability octet.
    pub caps: u8,
    /// The rates the stream may use, and which one it uses now.
    pub ladder: Ladder,
    pub rate: Rate,
    /// Index of `rate` in the ladder (0: the top).
    pub rung: usize,
    /// Largest media packet allowed, bytes, and how that was derived.
    pub mtu_limit: usize,
    pub mtu_why: String,
    /// For how long the stream has been started. `None`: it is not.
    pub streaming_for: Option<Duration>,
}

/// Where the session is and what it has counted: plain data for whoever
/// shows it (the service's status feed). See [`Session::status`].
#[derive(Clone, Debug, PartialEq)]
pub struct SessionStatus {
    pub phase: Phase,
    /// The link probe's last answer. `None`: not asked since the driver
    /// was attached.
    pub connected: Option<bool>,
    /// Waiting out the backoff of a failed link before the next attempt.
    pub retrying: bool,
    /// Why the session is where it is: what the driver lacks, why the
    /// stream could not be set up, why the link went down. Empty while
    /// there is a link.
    pub reason: String,
    /// The endpoint's format: the current one, else the last valid one.
    pub endpoint: Option<PcmFormat>,
    pub link: Option<LinkView>,
    /// Last delay the sink reported, 1/10 ms.
    pub sink_delay: Option<u16>,
    /// Who applies the volume.
    pub volume: Decision,
    /// The last audio state seen (flags, volume, the driver's counters).
    pub audio: AudioState,
    /// (driver version, protocol version) of GET_INFO while attached.
    pub driver: Option<(u32, u32)>,
    pub totals: Totals,
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
    /// Which rate of the ladder under `plan.bitrate` packets are built at.
    abr: Abr,
    /// How far the PCM is behind the endpoint clock.
    late: Lateness,
    /// How long the session has waited (with nothing to send) since the
    /// last packet: next to nothing while it works off a backlog.
    waited: Duration,
    mtu_limit: usize,
    /// How `mtu_limit` was derived.
    mtu_why: String,
    started: bool,
    /// Since when the stream is started.
    streaming_since: Option<Duration>,
    /// PCM rendered before this position predates the current START and is
    /// not sent (see [`Session::start`]).
    skip_before: u64,
    /// Frames skipped for that reason since START (for the log).
    skipped: u64,
    /// The driver's dropped-frames counter as last accounted for in
    /// [`Totals::dropped_frames`] (see [`Session::count_dropped`]).
    dropped_seen: u64,
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
    /// Who applies the volume and the software gain, from the last state.
    volume: Decision,
    /// Mode and mute last logged.
    volume_logged: Option<(VolumeMode, bool)>,
    /// A media WRITE timed out in this mode at this time.
    write_error: Option<(SscMode, Duration)>,
    /// Bitrate changes logged in the period that began at `.0`, and held
    /// back since the last one logged.
    rate_log: (Duration, u32, u32),
    /// Counters since the session started.
    totals: Totals,
    /// (driver version, protocol version) of GET_INFO while attached.
    driver: Option<(u32, u32)>,
    /// Why the session is where it is (see [`SessionStatus::reason`]).
    reason: String,
    /// Where the Windows device format comes from, if anywhere.
    formats: Option<Box<dyn FormatSource>>,
    /// What to do about it.
    fsync: FormatSync,
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
            volume: volume::decide(&AudioState::default()),
            volume_logged: None,
            write_error: None,
            rate_log: (Duration::ZERO, 0, 0),
            totals: Totals::default(),
            driver: None,
            reason: String::new(),
            formats: None,
            fsync: FormatSync::new(),
        }
    }

    /// Make the driver's default format follow what `source` reports (the
    /// format picked for the buds in the Windows sound settings).
    pub fn set_format_source(&mut self, source: Box<dyn FormatSource>) {
        self.formats = Some(source);
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

    /// The rate the current link's packets are built at.
    pub fn rate(&self) -> Option<Rate> {
        self.link.as_ref().map(|l| l.abr.rate())
    }

    /// Last delay the sink reported, in 1/10 ms.
    pub fn sink_delay(&self) -> Option<u16> {
        self.last_delay
    }

    /// Who applies the volume, and the software gain, as of the last state.
    pub fn volume(&self) -> Decision {
        self.volume
    }

    /// The gain the current link's pipeline is ramping to (or holds).
    pub fn pipeline_gain(&self) -> Option<[f64; 2]> {
        self.link.as_ref().map(|l| l.pipe.gain().target())
    }

    /// The session's clock.
    pub fn now(&self) -> Duration {
        self.clock.now()
    }

    /// Where the session is and what it has counted, as of the last step.
    /// Plain data from memory: no driver call, nothing logged.
    pub fn status(&self) -> SessionStatus {
        let now = self.clock.now();
        SessionStatus {
            phase: self.phase,
            connected: self.was_connected,
            retrying: self.phase == Phase::WaitLink && self.retry_at > now,
            reason: self.reason.clone(),
            endpoint: if self.audio.format.is_valid() {
                Some(self.audio.format)
            } else {
                self.last_format
            },
            link: self.link.as_ref().map(|l| LinkView {
                plan: l.plan,
                caps: l.sep.flags.0,
                ladder: *l.abr.ladder(),
                rate: l.abr.rate(),
                rung: l.abr.rung(),
                mtu_limit: l.mtu_limit,
                mtu_why: l.mtu_why.clone(),
                streaming_for: l.streaming_since.map(|t| now.saturating_sub(t)),
            }),
            sink_delay: self.last_delay,
            volume: self.volume,
            audio: self.audio,
            driver: self.driver,
            totals: self.totals,
        }
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
        self.sync_format();
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

    // -- the Windows device format ------------------------------------------

    /// Take a new Windows format from the source, and act on it while the
    /// driver is attached (see [`FormatSync`]). Cheap when nothing changed.
    fn sync_format(&mut self) {
        let Some(src) = self.formats.as_mut() else {
            return;
        };
        if src.changed() {
            let f = src.current();
            self.fsync.report(f);
        }
        let Some((_, protocol)) = self.driver else {
            return;
        };
        let id = self.name.clone();
        match self.fsync.decide(protocol, self.audio.default_format) {
            Step::Nothing => {}
            Step::OldDriver(w) => self.warn(&format!(
                "Windows device format for {id} is {}: this driver (protocol v{protocol}) cannot take it as its default, so Windows keeps playing at 48000 Hz; update the driver",
                short(w)
            )),
            Step::NotOffered(w) => self.info(&format!(
                "Windows device format for {id} is {}: the endpoint does not offer it; driver default left as it is",
                short(w)
            )),
            Step::AlreadySet(t) => self.info(&format!(
                "Windows device format for {id} is {}: driver default already set",
                short(t)
            )),
            Step::Send(t) => match self.drv.set_default_format(t) {
                Ok(()) => {
                    // A later failure is news again, not a repeat.
                    self.limiter.forget("default-format");
                    self.fsync.applied(t);
                    self.info(&format!(
                        "Windows device format for {id} is {}: driver default set",
                        short(t)
                    ));
                }
                Err(e) => self.error_once(
                    "default-format",
                    &format!(
                        "Windows device format for {id} is {}: setting the driver default failed: {e}",
                        short(t)
                    ),
                ),
            },
        }
        let now = self.clock.now();
        if self.fsync.check(now, self.audio.stream, self.audio.format)
            && let Some(t) = self.fsync.applying()
        {
            self.warn(&format!(
                "Windows still renders at {} although the driver default is now {}: UHQ through the Windows device is a known issue in this release (see the README)",
                short(self.audio.format),
                short(t)
            ));
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
                self.reason = format!("driver not ready: {msg}");
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
        self.driver = Some((info.driver_version, info.protocol_version));
        self.reason.clear();
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
        self.note_volume(s);
    }

    /// Who applies the volume now; log transitions (info) and level
    /// changes (debug). The pipeline follows in [`Self::on_state`].
    fn note_volume(&mut self, s: AudioState) {
        let d = volume::decide(&s);
        let v = s.volume;
        let dbs = |l: i32| f64::from(l) / 65536.0;
        let now = (d.mode, s.muted());
        if self.volume_logged != Some(now) {
            let detail = match d.mode {
                VolumeMode::Windows => String::new(),
                _ => format!(
                    "; level {:.1} dB (AVRCP {}){}",
                    dbs(v.level),
                    v.avrcp,
                    if s.muted() { ", muted" } else { "" }
                ),
            };
            self.info(&format!("volume: {}{detail}", d.mode.describe()));
            self.volume_logged = Some(now);
        } else if d != self.volume {
            self.say(
                Level::Debug,
                &format!(
                    "volume: level {:.1} dB (AVRCP {}), left {:.1} / right {:.1} dB, gain {:.4} / {:.4}",
                    dbs(v.level),
                    v.avrcp,
                    dbs(v.left),
                    dbs(v.right),
                    d.gain[0],
                    d.gain[1]
                ),
            );
        }
        self.volume = d;
    }

    /// A new pipeline starts at the current gain (no ramp from unity).
    fn new_pipeline(&self, fmt: PcmFormat, plan: &Plan) -> Result<Pipeline, String> {
        let mut p = Pipeline::new(fmt, plan)?;
        p.set_gain(self.volume.gain, true);
        Ok(p)
    }

    /// The bitrate controller for a stream configured as `plan`: at the top
    /// of the ladder, or at the bottom if a WRITE of the same mode timed
    /// out a moment ago.
    fn new_abr(&mut self, plan: &Plan, mtu_limit: usize) -> Abr {
        let top = Rate {
            bitrate: plan.bitrate,
            frames_per_packet: plan.frames_per_packet,
        };
        let ladder = Ladder::new(plan.encoder, top, mtu_limit, self.settings.adaptive, true);
        let mut abr = Abr::new(ladder);
        let now = self.clock.now();
        if !self.settings.adaptive {
            self.info(&format!(
                "adaptive bitrate is off (AdaptiveBitrate=0): always {} kbps",
                top.kbps()
            ));
        } else if !ladder.is_adaptive() {
            self.info(&format!(
                "adaptive bitrate: {} kbps is the lowest rate of {}, nothing to adapt",
                top.kbps(),
                plan.mode.name()
            ));
        } else {
            self.info(&format!("adaptive bitrate: {}", ladder.describe()));
            // Once: the link after the failure, not every one for a minute.
            let recent = self.write_error.take().is_some_and(|(mode, at)| {
                mode == plan.mode && now.saturating_sub(at) < WRITE_ERROR_MEMORY
            });
            if recent && let Some(c) = abr.on_write_error(now) {
                self.info(&format!(
                    "a media write timed out on the last link: starting at {} kbps",
                    c.to.kbps()
                ));
            }
        }
        abr
    }

    /// Log a bitrate change (see [`RATE_LOG_BURST`]).
    fn log_rate_change(&mut self, c: &Change, bytes: usize, now: Duration) {
        let (since, logged, held) = &mut self.rate_log;
        if now.saturating_sub(*since) >= ERROR_LOG_PERIOD {
            *since = now;
            *logged = 0;
        }
        if *logged >= RATE_LOG_BURST {
            *held += 1;
            self.say(Level::Debug, &format!("{c} (not in the info log)"));
            return;
        }
        *logged += 1;
        let held = std::mem::take(held);
        let more = if held > 0 {
            format!(" ({held} change(s) before this one were not logged)")
        } else {
            String::new()
        };
        self.info(&format!("{c}; {bytes}-byte packets{more}"));
    }

    /// The driver ended the session (or its audio requests fail): drop
    /// everything and start over at WaitDriver.
    fn on_detached(&mut self, why: &str) {
        self.reason = format!("the driver detached the session ({why})");
        self.driver = None;
        self.fsync.driver_lost();
        // The stream it was reported for is gone.
        self.last_delay = None;
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
                // Whatever failed before, a new connection starts afresh:
                // the buds being away is all there is to say now.
                self.reason.clear();
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
                self.reason.clear();
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
                // A delay the sink reported for the stream that did not
                // come about is nobody's.
                self.last_delay = None;
                self.reason = format!("cannot set up the stream: {}", f.msg);
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
        let (media, plan, mtu_limit, mtu_why) = self.open_stream(&mut sig, &sep, fmt, plan)?;
        let mut pipe = match self.new_pipeline(fmt, &plan) {
            Ok(p) => p,
            Err(e) => {
                self.release(&mut sig, sep.seid);
                return Err(fail(e));
            }
        };
        let abr = self.new_abr(&plan, mtu_limit);
        pipe.set_rate(abr.rate());
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
            stats: Stats::begin(0, &abr),
            abr,
            late: Lateness::new(),
            waited: Duration::ZERO,
            mtu_limit,
            mtu_why,
            started: false,
            streaming_since: None,
            skip_before: 0,
            skipped: 0,
            dropped_seen: 0,
            since: now,
            idle_since: None,
            next_start: now,
            start_failures: 0,
            sink_suspended: false,
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
    ) -> Result<(DriverChannel<D>, Plan, usize, String), Fail> {
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
        Ok((media, plan, limit, why))
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
                // The endpoint stands still: whatever comes next is not
                // late for that (a pause too short for a SUSPEND).
                link.late.reset();
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
        if let Some(t) = self.fsync.deadline() {
            timeout = timeout.min(t.saturating_sub(now));
        }
        let interest = Interest {
            signaling: Some(link.sig.ch.id),
            seq: Some(self.audio.seq),
            pcm: link.started,
        };
        let wake = self
            .drv
            .wait(&interest, timeout.max(Duration::from_millis(1)));
        link.waited += self.clock.now().saturating_sub(now);
        match wake {
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

    /// The driver's dropped-frames counter stands at `counter`: what it
    /// rose by since it was last looked at was lost from the started
    /// stream. (From START on only: what the driver drops while nothing
    /// reads it is no loss.)
    ///
    /// The counter only rises, but what is looked at may be older than the
    /// last look (a state that was announced before the counter was last
    /// asked for): the base never goes back, or the same frames would be
    /// counted again.
    fn count_dropped(&mut self, link: &mut Link<D>, counter: u64) {
        if counter <= link.dropped_seen {
            return;
        }
        if link.started {
            self.totals.dropped_frames += counter - link.dropped_seen;
        }
        link.dropped_seen = counter;
    }

    /// [`Self::count_dropped`] with the counter as the driver has it now:
    /// before the stream leaves the started state, so that what was lost
    /// since the last look is not forgotten.
    fn count_dropped_now(&mut self, link: &mut Link<D>) {
        if link.started
            && let Ok(st) = self.drv.audio_state()
        {
            self.count_dropped(link, st.frames_dropped);
            // Only the counter: the rest of a state goes through
            // `note_state`, with the event that announces it.
            self.audio.frames_dropped = st.frames_dropped;
        }
    }

    /// Bookkeeping when the stream leaves the started state.
    fn stream_stopped(&mut self, link: &mut Link<D>) {
        self.count_dropped_now(link);
        link.started = false;
        link.streaming_since = None;
        link.idle_since = None;
        link.pipe.flush();
        self.drv.flush_pcm();
    }

    /// The stream has entered the started state at `now` (our START was
    /// accepted, or we accepted the sink's).
    fn begin_streaming(&mut self, link: &mut Link<D>, now: Duration) {
        link.started = true;
        link.streaming_since = Some(now);
        self.totals.write_total = Duration::ZERO;
        self.totals.write_max = Duration::ZERO;
        self.totals.write_count = 0;
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
        // Only the counter: the rest of a state goes through `note_state`.
        self.audio.frames_dropped = st.frames_dropped;
        // What the driver dropped before this START was not this stream's.
        link.dropped_seen = st.frames_dropped;
        let fresh = u64::from(link.fmt.sample_rate / 2);
        link.skip_before = if st.frames_rendered.saturating_sub(self.run_start) <= fresh {
            self.run_start
        } else {
            st.frames_rendered
        };
        link.skipped = 0;
        // The rate stays where the controller left it: a stream that was
        // suspended has learnt nothing new about the link.
        link.late.reset();
        link.waited = Duration::ZERO;
        link.stats = Stats::begin(st.frames_dropped, &link.abr);
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
        self.count_dropped(link, s.frames_dropped);
        self.note_state(s);
        link.pipe.set_gain(self.volume.gain, false);
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
        // The stream is being closed: what the sink did to it is moot, and
        // the delay it reported was that stream's (the new one reports its
        // own while it is set up).
        let mut moot = None;
        let ended = self.take_events(&mut link.sig, &mut moot);
        self.last_delay = None;
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
            Ok((media, plan, limit, why)) => {
                let mut pipe = match self.new_pipeline(fmt, &plan) {
                    Ok(p) => p,
                    Err(e) => return down(e, true),
                };
                // Another mode, another ladder: the controller starts anew.
                let abr = self.new_abr(&plan, limit);
                pipe.set_rate(abr.rate());
                link.stats.rebase(&abr);
                link.abr = abr;
                link.late.reset();
                link.media = media;
                link.plan = plan;
                link.mtu_limit = limit;
                link.mtu_why = why;
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
            link.late.reset();
            link.stats.discontinuities += 1;
            self.totals.discontinuities += 1;
            self.say(
                Level::Debug,
                &format!("discontinuity before frame {}", h.first_frame),
            );
            // The driver dropped audio we did not fetch in time. Next to
            // WRITEs that blocked that is the link's doing.
            let now = self.clock.now();
            if let Some(c) = link.abr.on_discontinuity(now) {
                link.pipe.set_rate(c.to);
                self.log_rate_change(&c, c.to.packet_bytes(link.plan.encoder), now);
            }
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
        let taken = u64::from(h.frames).saturating_sub(stale);
        link.stats.frames_in += taken;
        self.totals.frames_in += taken;
        // How far behind the endpoint this chunk is taken up.
        let late = link
            .late
            .observe(self.clock.now(), end, h.format.sample_rate);
        link.stats.late_max = link.stats.late_max.max(late);
        link.pipe.feed(data);
        loop {
            use crate::channel::SduChannel;
            let pkt = match link.pipe.next_packet() {
                Ok(Some(pkt)) => pkt,
                Ok(None) => return Flow::Keep,
                Err(e) => return down(e.to_string(), true),
            };
            let t0 = self.clock.now();
            let r = link.media.send(pkt);
            let now = self.clock.now();
            if let Err(e) = r {
                self.totals.write_errors += 1;
                // A WRITE that did not complete in time is a link that
                // carries too little; a channel or a link that went away
                // says nothing of the kind.
                if write_error_is_congestion(&e) {
                    self.write_error = Some((link.plan.mode, now));
                }
                // The signaling channel may well be alive: give the stream
                // back.
                return down(format!("media write failed: {e}"), true);
            }
            let write = now.saturating_sub(t0);
            link.stats.packets += 1;
            link.stats.write_total += write;
            link.stats.write_max = link.stats.write_max.max(write);
            self.totals.packets += 1;
            self.totals.write_count += 1;
            self.totals.write_total += write;
            self.totals.write_max = self.totals.write_max.max(write);
            if late >= crate::abr::LATE {
                self.totals.late_packets += 1;
            }
            // The rate of the next packet follows this one's fate.
            let waited = std::mem::take(&mut link.waited);
            let obs = Obs {
                now,
                write,
                late,
                waited,
            };
            if let Some(c) = link.abr.on_packet(obs) {
                link.pipe.set_rate(c.to);
                self.log_rate_change(&c, c.to.packet_bytes(link.plan.encoder), now);
            }
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
                self.count_dropped(link, st.frames_dropped);
                // Only the counter: the rest of a state goes through
                // `note_state`, with the event that announces it.
                self.audio.frames_dropped = st.frames_dropped;
                d.to_string()
            }
            Err(_) => "?".into(),
        };
        let avg = s.write_total.as_micros() / u128::from(s.packets.max(1));
        self.info(&format!(
            "stats: packets={} frames_in={} dropped={dropped} discontinuities={} write avg={avg}us max={}us{} rate={}kbps at[{}] rate_changes={} slow_writes={} late_max={}ms",
            s.packets,
            s.frames_in,
            s.discontinuities,
            s.write_max.as_micros(),
            match self.last_delay {
                Some(d) => format!(" sink_delay={}.{}ms", d / 10, d % 10),
                None => String::new(),
            },
            link.abr.rate().kbps(),
            link.abr.describe_times(&s.times_base),
            link.abr.changes() - s.changes_base,
            link.abr.slow_packets() - s.slow_base,
            s.late_max.as_millis()
        ));
        link.stats = Stats::begin(link.stats.dropped_base, &link.abr);
    }

    /// The link is gone or unusable: unplug the endpoint, give the stream
    /// back if it is still ours, close the channels, reconnect later.
    fn teardown(&mut self, mut link: Link<D>, reason: &str, release: bool) {
        let now = self.clock.now();
        self.count_dropped_now(&mut link);
        self.reason = format!("link down: {reason}");
        self.totals.reconnects += 1;
        self.warn(&format!("link down: {reason}"));
        if let Err(e) = self.drv.set_link(false) {
            self.error_once("set-link", &format!("AUDIO_SET_LINK(false): {e}"));
        }
        if release {
            self.release(&mut link.sig, link.sep.seid);
        }
        self.drain_trace(&mut link.sig);
        // The delay the sink reported was this stream's.
        self.last_delay = None;
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
