//! Adaptive bitrate: which SSC rate the next frame is encoded at.
//!
//! The SSC frame header carries the rate code, so the rate may change on any
//! frame without touching the AVDTP configuration. [`Abr`] decides it from
//! what the sender can observe per media packet, without a driver change
//! (docs/DESIGN.md, "Adaptive bitrate", has the reasoning and what is only a
//! hypothesis until measured on real buds):
//!
//! | Signal | Use |
//! |---|---|
//! | Time the WRITE of a packet took | The congestion signal. A WRITE completes when bthport completes the outbound ACL transfer; on a healthy link that is 0.1 to 0.25 ms. |
//! | Time the sender waited for the packet's audio | Tells a paced packet from one of a burst (a backlog being sent back to back), whose WRITE time says how fast the link takes data, not how busy it is. |
//! | Lateness of the PCM against the endpoint clock ([`Lateness`]) | The sender is behind. Blocks stepping up; does not step down by itself (a starved sender is not cured by a lower rate). |
//! | PCM discontinuity (the driver dropped audio the reader did not fetch) | Steps down at once, but only next to WRITEs that blocked for [`DOWN_STRESS`]. |
//! | WRITE that timed out ([`write_error_is_congestion`]) | Lowest rate, and a longer wait before the next step up. |
//!
//! Rules, all in [`Abr::on_packet`]:
//!
//! * A WRITE is **busy** when it took at least 1/[`SLOW_WRITE_SHARE`] of the
//!   audio time the packet carries (and at least [`SLOW_WRITE_MIN`]).
//! * A busy WRITE is **slow**, evidence of a link that carries too little,
//!   unless a queue is being worked off. That is the case after
//!   [`BURST_PACKETS`] packets in a row the sender did not have to wait for
//!   (it is behind: after a stalled WRITE, at START with audio waiting) and
//!   after every step down (packets of the old rate fill the link's
//!   buffer). A sender that catches up offers the link more than the
//!   stream needs, so a WRITE is then slow only if it took as long as the
//!   packet plays: the link is slower than the stream. The usual measure
//!   is back after [`CALM_PACKETS`] packets that the sender waited for
//!   with no busy WRITE in between.
//! * **Down** (fast): the time slow WRITEs took is collected in a bucket
//!   that leaks at 1/[`STRESS_LEAK_SHARE`] of real time. [`DOWN_STRESS`] in
//!   the bucket from at least [`DOWN_MIN_SLOW`] slow packets: one rung down.
//!   One stalled WRITE, however long, is one slow packet and changes
//!   nothing, and neither does catching up after it. After a change the
//!   bucket starts empty and collects nothing for [`DOWN_SETTLE`].
//! * **Up** (slow): one rung up after the hold time ([`UP_HOLD_MIN`]) of
//!   clean streamed audio. A busy WRITE takes [`UP_PENALTY`] times its
//!   duration off the clean time (so a link that blocks the sender for
//!   1/[`UP_PENALTY`] of the time never steps up, and a stray slow WRITE
//!   costs a fraction of a second); a late packet or a step down restarts
//!   it. A step up is a probe: if the rate leaves that rung downwards
//!   within [`PROBE_WINDOW`] the hold time for the rung doubles (up to
//!   [`UP_HOLD_MAX`]). Every rung has its own hold time, and it halves
//!   again for every [`PROBE_WINDOW`] the stream stays at that rung or
//!   above it.
//!
//! Hold and probe times count streamed audio, not wall time, so a suspended
//! stream proves nothing about the link. Nothing here allocates, and the
//! per-packet work is a few integer operations.

use std::time::Duration;

use avdtp::rtp::RtpHeader;
use ssc_enc::{Bitrate, Mode, frame_size};

use crate::channel::ChanError;
use crate::rtp::packet_deadline;

/// Most rungs a ladder has (HiFi with a 229 kbps top: 229 / 192 / 128 / 88).
pub const MAX_RUNGS: usize = 4;

/// A WRITE that took this share (1/4) of the audio time its packet carries
/// is busy: the link then spends a quarter of real time taking this stream,
/// 10 to 100 times the healthy figure (0.1 to 0.25 ms per 9 or 18 ms).
pub const SLOW_WRITE_SHARE: u32 = 4;
/// ... but never less than this, so scheduler noise on a short packet
/// (UHQ: 9 ms of audio) does not count.
pub const SLOW_WRITE_MIN: Duration = Duration::from_millis(2);
/// The stress bucket leaks at this share (1/4) of real time: only a link
/// that blocks the sender for more than a quarter of the time fills it.
pub const STRESS_LEAK_SHARE: u32 = 4;
/// Blocked time in the bucket that steps the rate down. At a fully blocked
/// link (every WRITE takes as long as its packet plays) that is 130 to
/// 160 ms after the first slow packet: well inside the 250 ms of PCM the
/// driver keeps for a slow reader, so the step comes before audio is lost.
pub const DOWN_STRESS: Duration = Duration::from_millis(100);
/// The bucket never holds more, so one long stall is forgotten after at
/// most `STRESS_CAP * STRESS_LEAK_SHARE` (0.8 s).
pub const STRESS_CAP: Duration = Duration::from_millis(200);
/// Slow packets needed in the bucket: a single outlier is not congestion.
pub const DOWN_MIN_SLOW: u32 = 3;
/// A packet the sender waited less than this share (1/8) of its audio time
/// for was sent back to back: its audio was there when the WRITE before it
/// returned. (Reading and encoding a packet takes a fraction of a
/// millisecond; a paced packet waits most of its audio time.)
pub const BURST_WAIT_SHARE: u32 = 8;
/// This many back-to-back packets in a row are a burst: the sender is
/// working off a backlog. One alone is how the PCM chunks happen to fall
/// (a 10 ms chunk can complete two 9 ms packets).
pub const BURST_PACKETS: u8 = 2;
/// This many packets that the sender waited for, without a busy WRITE in
/// between: the queues are empty again. (One alone can slip through while
/// the link's buffer is still at its brim.)
pub const CALM_PACKETS: u8 = 4;
/// A busy WRITE takes this many times its duration off the clean time: a
/// link that blocks the sender for 2 % of the time does not step up, and
/// one stray WRITE of 5 ms costs a quarter of a second, not the whole hold.
pub const UP_PENALTY: u32 = 50;
/// After a rate change the bucket collects nothing for this long: packets
/// of the old rate are still queued below us, and the stall that caused a
/// step must not count for the next one.
pub const DOWN_SETTLE: Duration = Duration::from_millis(200);
/// PCM this far behind the endpoint clock makes a packet "late": four
/// endpoint periods (10 ms), beyond what wake-up jitter explains.
pub const LATE: Duration = Duration::from_millis(40);
/// Clean streamed audio before one rung up. Long against the bursts of
/// Wi-Fi and a passing body, short enough that the top rate is back soon
/// after the listener returns.
pub const UP_HOLD_MIN: Duration = Duration::from_secs(10);
/// The hold time doubles with every failed probe up to this (10 s * 2^3):
/// a link that cannot carry the next rung is probed once in 80 s.
pub const UP_HOLD_MAX: Duration = Duration::from_secs(80);
/// A step up that is taken back within this much streamed audio has failed.
/// Also: for every such stretch at a rung without a step down, the hold time
/// for that rung (and the ones below it) halves.
pub const PROBE_WINDOW: Duration = Duration::from_secs(15);
/// [`Lateness`] takes the smallest lateness of each such window as the new
/// zero, which follows the drift between the endpoint clock and ours.
pub const REANCHOR_EVERY: Duration = Duration::from_secs(10);
/// PCM later than this is not a backlog (the driver keeps 250 ms and flags
/// what it drops): the endpoint stood still, and [`Lateness`] starts anew.
pub const LATE_BOUND: Duration = Duration::from_secs(1);

/// The rates the buds are known to decode, highest first: up to 229 kbps
/// at 44.1 / 48 kHz, and 584 / 291 / 152 kbps in UHQ. (886 kbps UHQ needs
/// 1012-byte packets, above the 1005-byte media MTU, and is unproven.)
pub fn decodable(mode: Mode) -> &'static [Bitrate] {
    if mode.is_96k() {
        &[Bitrate::K584, Bitrate::K291, Bitrate::K152]
    } else {
        &[
            Bitrate::K229,
            Bitrate::K192,
            Bitrate::K128,
            Bitrate::K96,
            Bitrate::K88,
        ]
    }
}

/// Whether a failed WRITE says something about what the link carries: only
/// one that did not complete in time (the caller's timeout, or the driver's:
/// Win32 error 121). Every other failure (not connected, aborted, a stale
/// channel) is a link or a channel going away, as when the buds go into
/// their case, and no reason to come back at a low rate.
pub fn write_error_is_congestion(e: &ChanError) -> bool {
    matches!(e, ChanError::Timeout | ChanError::Os { code: 121, .. })
}

/// The rungs below the top rate, as Samsung phones step: HiFi 192 / 128 /
/// 88, plain SSC 128 / 88, UHQ 291 / 152.
fn steps(mode: Mode) -> &'static [Bitrate] {
    match mode {
        Mode::Uhq96k | Mode::LegacyUhq96k => &[Bitrate::K291, Bitrate::K152],
        Mode::Hifi48k => &[Bitrate::K192, Bitrate::K128, Bitrate::K88],
        Mode::Ssc48k | Mode::Ssc44k1 => &[Bitrate::K128, Bitrate::K88],
    }
}

/// What a packet is built with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate {
    pub bitrate: Bitrate,
    /// SSC frames per media packet.
    pub frames_per_packet: usize,
}

impl Rate {
    pub fn kbps(self) -> u32 {
        self.bitrate.bps() / 1000
    }
    pub fn packet_bytes(self, mode: Mode) -> usize {
        RtpHeader::LEN + self.frames_per_packet * frame_size(mode, self.bitrate)
    }
}

/// The rates a stream may use, highest (the configured rate) first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ladder {
    mode: Mode,
    rungs: [Rate; MAX_RUNGS],
    len: usize,
}

impl Ladder {
    /// The ladder under `top` for `mode`. Not `adaptive`: `top` alone.
    ///
    /// A lower rung takes the frames per packet of `top`, except that the
    /// UHQ rungs carry two frames per packet when `two_frames` (as the
    /// phones do at 291 and 152 kbps; a packet then spans as much audio as
    /// a HiFi one). A rung whose packet does not fit `mtu_limit` falls back
    /// to `top`'s frames per packet, or is left out.
    pub fn new(mode: Mode, top: Rate, mtu_limit: usize, adaptive: bool, two_frames: bool) -> Self {
        let mut l = Ladder {
            mode,
            rungs: [top; MAX_RUNGS],
            len: 1,
        };
        if !adaptive {
            return l;
        }
        for &b in steps(mode) {
            if b.bps() >= top.bitrate.bps() || l.len == MAX_RUNGS {
                continue;
            }
            let mut r = Rate {
                bitrate: b,
                frames_per_packet: top.frames_per_packet,
            };
            if two_frames && mode.is_96k() {
                let two = Rate {
                    frames_per_packet: top.frames_per_packet.max(2),
                    ..r
                };
                if two.packet_bytes(mode) <= mtu_limit {
                    r = two;
                }
            }
            if r.packet_bytes(mode) <= mtu_limit {
                l.rungs[l.len] = r;
                l.len += 1;
            }
        }
        l
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn rungs(&self) -> &[Rate] {
        &self.rungs[..self.len]
    }
    pub fn top(&self) -> Rate {
        self.rungs[0]
    }
    /// More than one rate to choose from.
    pub fn is_adaptive(&self) -> bool {
        self.len > 1
    }
    /// Audio time one packet at `rate` carries.
    pub fn packet_time(&self, rate: Rate) -> Duration {
        packet_deadline(1, rate.frames_per_packet, self.mode.sample_rate())
    }
    /// The largest payload any rung puts into a packet.
    pub fn max_payload_bytes(&self) -> usize {
        self.rungs()
            .iter()
            .map(|r| r.packet_bytes(self.mode) - RtpHeader::LEN)
            .max()
            .unwrap_or(0)
    }
    /// One line for the log: "192 / 128 / 88 kbps".
    pub fn describe(&self) -> String {
        let mut s = String::new();
        for (i, r) in self.rungs().iter().enumerate() {
            if i > 0 {
                s.push_str(" / ");
            }
            s.push_str(&r.kbps().to_string());
            if r.frames_per_packet != self.top().frames_per_packet {
                s.push_str(&format!(" ({} frames/packet)", r.frames_per_packet));
            }
        }
        s.push_str(" kbps");
        s
    }
}

/// What the sender saw of one packet.
#[derive(Clone, Copy, Debug)]
pub struct Obs {
    /// When the WRITE returned.
    pub now: Duration,
    /// How long the WRITE took.
    pub write: Duration,
    /// How far behind the endpoint clock the packet's audio was when it was
    /// taken up ([`Lateness`]), or behind its send time for a paced sender.
    pub late: Duration,
    /// How long the sender waited for this packet's audio (or its send
    /// time) since the WRITE before it returned. Next to nothing: the
    /// packet went out back to back.
    pub waited: Duration,
}

/// Why the rate changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// WRITEs blocked for `blocked` in `slow` slow packets.
    Congested { blocked: Duration, slow: u32 },
    /// The driver dropped PCM while WRITEs blocked.
    Dropout,
    /// A WRITE timed out.
    WriteError,
    /// This much clean streamed audio (see [`UP_PENALTY`]).
    Clean(Duration),
}

/// One rate change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub from: Rate,
    pub to: Rate,
    pub why: Why,
}

impl Change {
    pub fn is_down(&self) -> bool {
        self.to.bitrate.bps() < self.from.bitrate.bps()
    }
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bitrate {} -> {} kbps", self.from.kbps(), self.to.kbps())?;
        if self.to.frames_per_packet != self.from.frames_per_packet {
            write!(f, ", {} frame(s)/packet", self.to.frames_per_packet)?;
        }
        match self.why {
            Why::Congested { blocked, slow } => write!(
                f,
                ": link congested (WRITE blocked {} ms in {slow} slow packets)",
                blocked.as_millis()
            ),
            Why::Dropout => f.write_str(": audio was dropped while WRITEs blocked"),
            Why::WriteError => f.write_str(": a WRITE timed out"),
            Why::Clean(d) => write!(f, ": link clean for {} s", d.as_secs()),
        }
    }
}

/// The controller. One per stream configuration; a mode switch starts a new
/// one.
#[derive(Clone, Debug)]
pub struct Abr {
    ladder: Ladder,
    /// Index into the ladder (0 = top).
    rung: usize,
    /// Per rung: audio time of a packet, the WRITE time that is busy, and
    /// the wait below which a packet was sent back to back.
    packet_time: [Duration; MAX_RUNGS],
    slow_write: [Duration; MAX_RUNGS],
    burst_wait: [Duration; MAX_RUNGS],
    /// When the last packet was observed.
    last: Option<Duration>,
    /// Packets in a row that were sent back to back.
    back_to_back: u8,
    /// Packets that were waited for since the last busy WRITE.
    calm: u8,
    /// A queue is being worked off (the sender's backlog, or packets of a
    /// higher rate in the link's buffer): a WRITE is slow only if it took
    /// as long as its packet plays.
    draining: bool,
    /// The bucket: time slow WRITEs took, minus the leak.
    stress: Duration,
    /// Slow packets in the bucket.
    slow: u32,
    /// The same bucket for every busy WRITE, slow or not: how long the link
    /// held the sender lately, whatever that says about its capacity.
    blocked: Duration,
    /// The buckets collect nothing before this time.
    settle_until: Duration,
    /// Clean streamed audio: what packets with a quick WRITE carried, less
    /// [`UP_PENALTY`] times what busy WRITEs took.
    clean: Duration,
    /// Per rung: `clean` needed to step up to it.
    hold: [Duration; MAX_RUNGS],
    /// Per rung: streamed audio since the step up to it, while that is a
    /// probe.
    probe: [Option<Duration>; MAX_RUNGS],
    /// Streamed audio at this rung since the holds were last halved.
    stay: Duration,
    // Counters for the statistics.
    time_at: [Duration; MAX_RUNGS],
    changes: u32,
    slow_total: u64,
}

impl Abr {
    /// Starts at the top of `ladder`.
    pub fn new(ladder: Ladder) -> Self {
        let mut packet_time = [Duration::ZERO; MAX_RUNGS];
        let mut slow_write = [Duration::MAX; MAX_RUNGS];
        let mut burst_wait = [Duration::ZERO; MAX_RUNGS];
        for (i, r) in ladder.rungs().iter().enumerate() {
            packet_time[i] = ladder.packet_time(*r);
            slow_write[i] = (packet_time[i] / SLOW_WRITE_SHARE).max(SLOW_WRITE_MIN);
            burst_wait[i] = packet_time[i] / BURST_WAIT_SHARE;
        }
        Abr {
            ladder,
            rung: 0,
            packet_time,
            slow_write,
            burst_wait,
            last: None,
            back_to_back: 0,
            calm: 0,
            draining: false,
            stress: Duration::ZERO,
            slow: 0,
            blocked: Duration::ZERO,
            settle_until: Duration::ZERO,
            clean: Duration::ZERO,
            hold: [UP_HOLD_MIN; MAX_RUNGS],
            probe: [None; MAX_RUNGS],
            stay: Duration::ZERO,
            time_at: [Duration::ZERO; MAX_RUNGS],
            changes: 0,
            slow_total: 0,
        }
    }

    pub fn ladder(&self) -> &Ladder {
        &self.ladder
    }
    /// The rate for the next packet.
    pub fn rate(&self) -> Rate {
        self.ladder.rungs[self.rung]
    }
    /// Index of the current rate in the ladder (0 = top).
    pub fn rung(&self) -> usize {
        self.rung
    }
    /// Rate changes so far.
    pub fn changes(&self) -> u32 {
        self.changes
    }
    /// Busy WRITEs so far (slow or not).
    pub fn slow_packets(&self) -> u64 {
        self.slow_total
    }
    /// Streamed audio at each rung so far.
    pub fn time_at(&self) -> [Duration; MAX_RUNGS] {
        self.time_at
    }
    /// Clean audio needed for the next step up (at the top: for the step
    /// back to it).
    pub fn hold(&self) -> Duration {
        self.hold[self.rung.saturating_sub(1)]
    }
    /// Whether a queue is being worked off (see the module text).
    pub fn is_draining(&self) -> bool {
        self.draining
    }

    /// A packet at the current rate has been written. `Some` if the rate
    /// for the next packet is another one.
    pub fn on_packet(&mut self, o: Obs) -> Option<Change> {
        let audio = self.packet_time[self.rung];
        self.time_at[self.rung] += audio;
        // The leak, over the real time since the previous packet.
        let elapsed = o.now.saturating_sub(self.last.unwrap_or(o.now));
        self.last = Some(o.now);
        let leak = elapsed / STRESS_LEAK_SHARE;
        self.stress = self.stress.saturating_sub(leak);
        self.blocked = self.blocked.saturating_sub(leak);
        if self.stress.is_zero() {
            self.slow = 0;
        }
        // Is the sender working off a backlog?
        let waited = o.waited >= self.burst_wait[self.rung];
        if waited {
            self.back_to_back = 0;
        } else {
            self.back_to_back = self.back_to_back.saturating_add(1);
            if self.back_to_back >= BURST_PACKETS {
                self.draining = true;
            }
        }
        let busy = o.write >= self.slow_write[self.rung];
        if busy {
            self.calm = 0;
        } else if waited {
            // (A quick packet that was not waited for counts neither way:
            // two packets can become ready at once on a paced sender too.)
            self.calm = self.calm.saturating_add(1);
            if self.calm >= CALM_PACKETS {
                // Paced again, and the link takes the packets at once: no
                // queue is left, ours or the link's.
                self.draining = false;
            }
        }
        // While a queue is worked off, the link is offered more than the
        // stream needs: only a WRITE as long as the packet plays says that
        // it carries less than that.
        let slow = busy && (!self.draining || o.write >= audio);
        if busy {
            self.slow_total += 1;
            if o.now >= self.settle_until {
                self.blocked = (self.blocked + o.write).min(STRESS_CAP);
                if slow {
                    self.stress = (self.stress + o.write).min(STRESS_CAP);
                    self.slow += 1;
                }
            }
        }
        if o.late >= LATE {
            self.clean = Duration::ZERO;
        } else if busy {
            self.clean = self
                .clean
                .saturating_sub(o.write.saturating_mul(UP_PENALTY));
        } else {
            self.clean += audio;
        }
        for p in &mut self.probe {
            // A step up that has held for the window is no probe any more.
            *p = p.map(|t| t + audio).filter(|t| *t < PROBE_WINDOW);
        }
        // A rung that holds is probed sooner the next time (and so are the
        // rungs below it); the rungs above keep what their probes cost.
        self.stay += audio;
        if self.stay >= PROBE_WINDOW {
            self.stay = Duration::ZERO;
            for h in &mut self.hold[self.rung..] {
                *h = (*h / 2).max(UP_HOLD_MIN);
            }
        }
        if self.stress >= DOWN_STRESS && self.slow >= DOWN_MIN_SLOW {
            let why = Why::Congested {
                blocked: self.stress,
                slow: self.slow,
            };
            return self.step_down(o.now, why);
        }
        if self.rung > 0 && self.clean >= self.hold[self.rung - 1] {
            let from = self.rate();
            let why = Why::Clean(self.clean);
            self.rung -= 1;
            self.clean = Duration::ZERO;
            self.probe[self.rung] = Some(Duration::ZERO);
            return Some(self.changed(o.now, from, why));
        }
        None
    }

    /// The driver reported that it dropped PCM the reader did not fetch in
    /// time. If WRITEs held the sender for [`DOWN_STRESS`] lately, that is
    /// the link: step down at once. (One short WRITE can cost audio too,
    /// when the driver's ring was full already, as it may be at START.)
    pub fn on_discontinuity(&mut self, now: Duration) -> Option<Change> {
        self.clean = Duration::ZERO;
        if self.blocked < DOWN_STRESS || now < self.settle_until {
            return None;
        }
        self.step_down(now, Why::Dropout)
    }

    /// A WRITE failed for want of capacity ([`write_error_is_congestion`]):
    /// the lowest rate, and every step up waits twice as long.
    pub fn on_write_error(&mut self, now: Duration) -> Option<Change> {
        self.clean = Duration::ZERO;
        self.probe = [None; MAX_RUNGS];
        for h in &mut self.hold {
            *h = (*h * 2).min(UP_HOLD_MAX);
        }
        let bottom = self.ladder.len - 1;
        if self.rung == bottom {
            return None;
        }
        let from = self.rate();
        self.rung = bottom;
        Some(self.changed(now, from, Why::WriteError))
    }

    fn step_down(&mut self, now: Duration, why: Why) -> Option<Change> {
        self.stress = Duration::ZERO;
        self.blocked = Duration::ZERO;
        self.slow = 0;
        self.clean = Duration::ZERO;
        if self.probe[self.rung].take().is_some() {
            // The step up to this rung did not hold: wait longer before
            // the next one.
            let h = &mut self.hold[self.rung];
            *h = (*h * 2).min(UP_HOLD_MAX);
        }
        if self.rung + 1 >= self.ladder.len {
            return None;
        }
        let from = self.rate();
        self.rung += 1;
        // Packets of the old rate are still in the link's buffer.
        self.draining = true;
        Some(self.changed(now, from, why))
    }

    fn changed(&mut self, now: Duration, from: Rate, why: Why) -> Change {
        self.changes += 1;
        self.stress = Duration::ZERO;
        self.blocked = Duration::ZERO;
        self.slow = 0;
        self.stay = Duration::ZERO;
        self.settle_until = now + DOWN_SETTLE;
        Change {
            from,
            to: self.rate(),
            why,
        }
    }

    /// Streamed audio per rate since `base` (an earlier [`Self::time_at`]),
    /// for the statistics: "192k=41.2s 128k=18.8s".
    pub fn describe_times(&self, base: &[Duration; MAX_RUNGS]) -> String {
        let mut s = String::new();
        for (i, r) in self.ladder.rungs().iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            let t = self.time_at[i].saturating_sub(base[i]);
            s.push_str(&format!("{}k={:.1}s", r.kbps(), t.as_secs_f64()));
        }
        s
    }
}

/// How far the sender is behind the endpoint: the PCM's position against
/// the time it is taken up. The earliest arrival is the reference, and
/// every [`REANCHOR_EVERY`] the smallest lateness since becomes the new
/// zero, so clock drift between the endpoint and this clock does not add up
/// (a backlog that stays for longer than that is taken as the new normal:
/// what counts is growth). The caller forgets the schedule ([`Self::reset`])
/// whenever the audio is not contiguous or the endpoint does not render: a
/// pause is the endpoint standing still, not the sender being behind.
#[derive(Clone, Copy, Debug, Default)]
pub struct Lateness {
    /// (time, frame position, sample rate) the schedule is counted from.
    anchor: Option<(Duration, u64, u32)>,
    window_min: Duration,
    window_end: Duration,
}

impl Lateness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the schedule (the audio is not contiguous with what was).
    pub fn reset(&mut self) {
        self.anchor = None;
    }

    /// PCM up to frame `end` (at `rate` frames per second) is taken up at
    /// `now`: how late that is.
    pub fn observe(&mut self, now: Duration, end: u64, rate: u32) -> Duration {
        let late = match self.anchor {
            Some((t0, f0, r0)) if r0 == rate && end >= f0 && rate > 0 => {
                let ns = u128::from(end - f0) * 1_000_000_000 / u128::from(rate);
                let due = t0 + Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX));
                now.checked_sub(due)
            }
            _ => None,
        };
        let Some(late) = late.filter(|l| *l <= LATE_BOUND) else {
            // The first chunk, one earlier than every one before, or one
            // after the endpoint stood still.
            self.anchor = Some((now, end, rate));
            self.window_min = Duration::MAX;
            self.window_end = now + REANCHOR_EVERY;
            return Duration::ZERO;
        };
        self.window_min = self.window_min.min(late);
        if now >= self.window_end {
            if let Some((t0, _, _)) = &mut self.anchor {
                *t0 += self.window_min;
            }
            let late = late - self.window_min;
            self.window_min = Duration::MAX;
            self.window_end = now + REANCHOR_EVERY;
            return late;
        }
        late
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);
    /// A healthy WRITE.
    const FAST: Duration = Duration::from_micros(200);

    fn rate(b: Bitrate, fpp: usize) -> Rate {
        Rate {
            bitrate: b,
            frames_per_packet: fpp,
        }
    }

    fn hifi() -> Ladder {
        Ladder::new(Mode::Hifi48k, rate(Bitrate::K192, 1), 1005, true, true)
    }
    fn uhq() -> Ladder {
        Ladder::new(Mode::Uhq96k, rate(Bitrate::K584, 1), 1005, true, true)
    }

    fn kbps(l: &Ladder) -> Vec<(u32, usize)> {
        l.rungs()
            .iter()
            .map(|r| (r.kbps(), r.frames_per_packet))
            .collect()
    }

    /// A sender like the session: one packet per packet time, each WRITE
    /// taking what `write(t)` says; a WRITE longer than the packet time
    /// delays the packets after it. Collects (time, change).
    struct Sim {
        abr: Abr,
        now: Duration,
        changes: Vec<(Duration, Change)>,
    }

    impl Sim {
        fn new(l: Ladder) -> Self {
            Sim {
                abr: Abr::new(l),
                now: Duration::ZERO,
                changes: Vec::new(),
            }
        }
        fn run(&mut self, until: Duration, write: impl Fn(Duration) -> Duration) {
            while self.now < until {
                let period = self.abr.ladder().packet_time(self.abr.rate());
                let w = write(self.now);
                self.now += w.max(period);
                let c = self.abr.on_packet(Obs {
                    now: self.now,
                    write: w,
                    late: Duration::ZERO,
                    waited: period.saturating_sub(w),
                });
                if let Some(c) = c {
                    self.changes.push((self.now, c));
                }
            }
        }
        fn rates(&self) -> Vec<u32> {
            self.changes.iter().map(|(_, c)| c.to.kbps()).collect()
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn ladders() {
        assert_eq!(kbps(&hifi()), [(192, 1), (128, 1), (88, 1)]);
        assert_eq!(kbps(&uhq()), [(584, 1), (291, 2), (152, 2)]);
        for m in [Mode::Ssc48k, Mode::Ssc44k1] {
            let l = Ladder::new(m, rate(Bitrate::K229, 1), 1005, true, true);
            assert_eq!(kbps(&l), [(229, 1), (128, 1), (88, 1)]);
        }
        // The top is what was configured; only lower steps follow.
        let l = Ladder::new(Mode::Hifi48k, rate(Bitrate::K229, 1), 1005, true, true);
        assert_eq!(kbps(&l), [(229, 1), (192, 1), (128, 1), (88, 1)]);
        let l = Ladder::new(Mode::Hifi48k, rate(Bitrate::K128, 1), 1005, true, true);
        assert_eq!(kbps(&l), [(128, 1), (88, 1)]);
        let l = Ladder::new(Mode::Hifi48k, rate(Bitrate::K88, 1), 1005, true, true);
        assert_eq!(kbps(&l), [(88, 1)]);
        assert!(!l.is_adaptive());
        let l = Ladder::new(Mode::Uhq96k, rate(Bitrate::K291, 1), 1005, true, true);
        assert_eq!(kbps(&l), [(291, 1), (152, 2)]);
        // Not adaptive: the top alone.
        let l = Ladder::new(Mode::Hifi48k, rate(Bitrate::K192, 1), 1005, false, true);
        assert_eq!(kbps(&l), [(192, 1)]);
        // Two frames only where the caller wants them and they fit.
        let l = Ladder::new(Mode::Uhq96k, rate(Bitrate::K584, 1), 1005, true, false);
        assert_eq!(kbps(&l), [(584, 1), (291, 1), (152, 1)]);
        let two_291 = rate(Bitrate::K291, 2).packet_bytes(Mode::Uhq96k);
        let l = Ladder::new(
            Mode::Uhq96k,
            rate(Bitrate::K584, 1),
            two_291 - 1,
            true,
            true,
        );
        assert_eq!(kbps(&l), [(584, 1), (291, 1), (152, 2)]);
        assert_eq!(hifi().describe(), "192 / 128 / 88 kbps");
        assert_eq!(
            uhq().describe(),
            "584 / 291 (2 frames/packet) / 152 (2 frames/packet) kbps"
        );
    }

    #[test]
    fn every_rung_is_decodable_and_fits() {
        for (mode, tops) in [
            (Mode::Hifi48k, decodable(Mode::Hifi48k)),
            (Mode::Ssc48k, decodable(Mode::Ssc48k)),
            (Mode::Ssc44k1, decodable(Mode::Ssc44k1)),
            (Mode::Uhq96k, decodable(Mode::Uhq96k)),
        ] {
            for &top in tops {
                for limit in [1005usize, 700, 500, 300] {
                    let top = rate(top, 1);
                    let l = Ladder::new(mode, top, limit, true, true);
                    assert_eq!(l.top(), top);
                    let mut last = u32::MAX;
                    for (i, r) in l.rungs().iter().enumerate() {
                        assert!(decodable(mode).contains(&r.bitrate), "{mode:?} {r:?}");
                        assert!(r.bitrate.bps() < last, "strictly falling");
                        last = r.bitrate.bps();
                        assert!(r.bitrate.is_e_family() == mode.is_96k());
                        if i > 0 {
                            assert!(r.packet_bytes(mode) <= limit, "{mode:?} {r:?} {limit}");
                        }
                        assert!(l.max_payload_bytes() >= r.packet_bytes(mode) - 12);
                    }
                }
            }
        }
        // The packet sizes of the standard ladders.
        let sizes = |l: &Ladder| -> Vec<usize> {
            l.rungs().iter().map(|r| r.packet_bytes(l.mode())).collect()
        };
        assert_eq!(sizes(&hifi()), [496, 336, 236]);
        assert_eq!(sizes(&uhq()), [672, 672, 360]);
        assert_eq!(hifi().packet_time(hifi().top()), 18 * MS);
        assert_eq!(uhq().packet_time(uhq().top()), 9 * MS);
        assert_eq!(uhq().packet_time(uhq().rungs()[1]), 18 * MS);
    }

    #[test]
    fn a_good_link_stays_at_the_top() {
        for l in [hifi(), uhq()] {
            let mut s = Sim::new(l);
            // An hour of healthy WRITEs, with the jitter a busy PC adds.
            s.run(secs(3600), |t| {
                FAST + Duration::from_micros(t.as_millis() as u64 % 7 * 150)
            });
            assert!(s.changes.is_empty());
            assert_eq!(s.abr.rate(), l.top());
            assert_eq!(s.abr.slow_packets(), 0);
            let t = s.abr.time_at();
            assert!(t[0] >= secs(3599) && t[1].is_zero());
        }
    }

    #[test]
    fn sustained_congestion_steps_down_fast() {
        // HiFi: every WRITE takes 20 ms from t = 5 s on (the link carries
        // less than the stream needs).
        let mut s = Sim::new(hifi());
        s.run(secs(5), |_| FAST);
        let t0 = s.now;
        s.run(t0 + secs(2), |_| 20 * MS);
        assert_eq!(s.rates(), [128, 88], "{:?}", s.changes);
        let first = s.changes[0].0 - t0;
        assert!(first <= 200 * MS, "first step after {first:?}");
        assert!(first >= 60 * MS, "not on the first slow packets: {first:?}");
        let second = s.changes[1].0 - s.changes[0].0;
        assert!(
            second >= DOWN_SETTLE && second <= 500 * MS,
            "second step {second:?} after the first"
        );
        assert!(matches!(
            s.changes[0].1.why,
            Why::Congested { slow, blocked } if slow >= DOWN_MIN_SLOW && blocked >= DOWN_STRESS
        ));
        assert!(s.changes.iter().all(|(_, c)| c.is_down()));
        // The bottom is the bottom.
        assert_eq!(s.abr.rung(), 2);
        // UHQ: WRITEs of 9 ms, as long as a packet plays.
        let mut s = Sim::new(uhq());
        s.run(secs(5), |_| FAST);
        let t0 = s.now;
        s.run(t0 + secs(1), |_| 9 * MS);
        assert_eq!(s.rates()[0], 291);
        assert!(s.changes[0].0 - t0 <= 250 * MS, "{:?}", s.changes);
        assert_eq!(s.changes[0].1.to.frames_per_packet, 2);
    }

    #[test]
    fn moderate_load_steps_down_later_and_light_load_never() {
        // 9 ms per 18 ms packet: the link is busy half of the time.
        let mut s = Sim::new(hifi());
        s.run(secs(3), |_| 9 * MS);
        assert_eq!(s.rates()[0], 128);
        assert!(s.changes[0].0 <= 600 * MS, "{:?}", s.changes[0]);
        // 4 ms per 18 ms (22 % of the time): not slow, for ever.
        let mut s = Sim::new(hifi());
        s.run(secs(3600), |_| 4 * MS);
        assert!(s.changes.is_empty());
        assert_eq!(s.abr.slow_packets(), 0);
        // 5 ms per 18 ms (28 %): just over the leak, so it takes seconds.
        let mut s = Sim::new(hifi());
        s.run(secs(60), |_| 5 * MS);
        assert_eq!(s.rates()[0], 128);
        assert!(
            s.changes[0].0 >= secs(2) && s.changes[0].0 <= secs(5),
            "{:?}",
            s.changes[0]
        );
        // One rung, not the whole ladder: at 128 kbps the WRITEs are as
        // long as before but shorter than a packet plays, so nothing says
        // that the link is too small for that rate. (Latency that does not
        // depend on the rate looks like this; it costs one rung.)
        s.run(secs(3600), |_| 5 * MS);
        assert_eq!(s.rates(), [128]);
    }

    #[test]
    fn single_outliers_are_ignored() {
        for stall in [30 * MS, 150 * MS, 2000 * MS] {
            let mut s = Sim::new(hifi());
            // One stalled WRITE every 5 s for 10 minutes.
            s.run(secs(600), |t| {
                if t.as_millis() % 5000 < 18 {
                    stall
                } else {
                    FAST
                }
            });
            assert!(s.changes.is_empty(), "{stall:?}: {:?}", s.changes);
            assert!(s.abr.slow_packets() >= 100);
        }
        // Two slow packets in a row are still not three.
        let mut s = Sim::new(hifi());
        s.run(secs(60), |t| {
            if t.as_millis() % 5000 < 110 {
                60 * MS
            } else {
                FAST
            }
        });
        assert!(s.changes.is_empty(), "{:?}", s.changes);
    }

    #[test]
    fn recovery_steps_up_one_rung_per_hold() {
        let mut s = Sim::new(hifi());
        s.run(secs(2), |_| 20 * MS);
        assert_eq!(s.rates(), [128, 88]);
        let t0 = s.now;
        s.run(t0 + secs(40), |_| FAST);
        assert_eq!(s.rates(), [128, 88, 128, 192]);
        let up1 = s.changes[2].0 - t0;
        let up2 = s.changes[3].0 - s.changes[2].0;
        for up in [up1, up2] {
            assert!(
                up >= UP_HOLD_MIN && up <= UP_HOLD_MIN + 100 * MS,
                "{up:?} {:?}",
                s.changes
            );
        }
        assert!(matches!(
            s.changes[2].1.why,
            Why::Clean(d) if d >= UP_HOLD_MIN && d < UP_HOLD_MIN + 18 * MS
        ));
        assert_eq!(s.abr.rate(), hifi().top());
        // Never above the top, however long the link stays clean.
        s.run(s.now + secs(600), |_| FAST);
        assert_eq!(s.changes.len(), 4);
        assert_eq!(s.abr.hold(), UP_HOLD_MIN);
    }

    #[test]
    fn stray_slow_writes_do_not_keep_the_rate_down() {
        // One WRITE of 5 ms every 9 s never moves a stream at the top ...
        let stray = |t: Duration| {
            if t.as_millis() % 9000 < 18 {
                5 * MS
            } else {
                FAST
            }
        };
        let mut s = Sim::new(hifi());
        s.run(secs(600), stray);
        assert!(s.changes.is_empty());
        // ... and must not hold one down that congestion took to the
        // bottom: each costs a quarter of a second of clean time.
        for l in [hifi(), uhq()] {
            let mut s = Sim::new(l);
            s.run(secs(1), |_| 20 * MS);
            assert_eq!(s.abr.rung(), 2);
            s.run(secs(600), stray);
            assert_eq!(s.abr.rung(), 0, "{:?}", s.changes);
            let ups: Vec<Duration> = s.changes[2..].iter().map(|(t, _)| *t).collect();
            assert_eq!(ups.len(), 2, "{:?}", s.changes);
            assert!(ups[0] <= secs(12) && ups[1] <= secs(23), "{ups:?}");
            assert!(matches!(
                s.changes[2].1.why,
                Why::Clean(d) if d >= UP_HOLD_MIN && d < UP_HOLD_MIN + 18 * MS
            ));
        }
        // The same with a long hold: the link carries 128 kbps only for
        // 400 s (the hold reaches 80 s), then everything, with a stray
        // 5 ms WRITE every 60 s.
        let mut s = Sim::new(hifi());
        while s.now < secs(400) {
            let top = s.abr.rung() == 0;
            s.run(s.now + 18 * MS, |_| if top { 20 * MS } else { FAST });
        }
        assert_eq!(s.abr.hold(), UP_HOLD_MAX);
        let n = s.changes.len();
        s.run(secs(700), |t| {
            if t.as_millis() % 60_000 < 18 {
                5 * MS
            } else {
                FAST
            }
        });
        assert_eq!(s.abr.rung(), 0, "{:?}", &s.changes[n..]);
        assert_eq!(s.changes.len(), n + 1, "{:?}", &s.changes[n..]);
        assert!(s.changes[n].0 <= secs(482), "{:?}", s.changes[n]);
    }

    #[test]
    fn a_busy_or_late_link_does_not_step_up() {
        let mut s = Sim::new(hifi());
        s.run(secs(1), |_| 20 * MS);
        assert_eq!(s.rates(), [128, 88]);
        // 5 ms of every 18 ms (the link is busy 28 % of the time): the
        // rate stays where it is, for ever.
        s.run(secs(600), |_| 5 * MS);
        assert_eq!(s.rates(), [128, 88]);
        // A stall of 250 ms every 8 s costs more than the 8 s bring.
        let mut s = Sim::new(hifi());
        s.run(secs(1), |_| 20 * MS);
        s.run(secs(300), |t| {
            if t.as_millis() % 8000 < 18 {
                250 * MS
            } else {
                FAST
            }
        });
        assert_eq!(s.rates(), [128, 88]);
        // Late packets (the sender is behind) restart the hold, and do not
        // step down either.
        let mut abr = Abr::new(hifi());
        abr.on_write_error(Duration::ZERO);
        assert_eq!(abr.rung(), 2);
        let mut now = Duration::ZERO;
        for k in 0..20_000u32 {
            now += 18 * MS;
            let late = if k % 400 == 0 { LATE } else { LATE - MS };
            let c = abr.on_packet(Obs {
                now,
                write: FAST,
                late,
                waited: 17 * MS,
            });
            assert_eq!(c, None, "packet {k}");
        }
        let mut abr = Abr::new(hifi());
        for k in 0..20_000u32 {
            let c = abr.on_packet(Obs {
                now: 18 * MS * k,
                write: FAST,
                late: 200 * MS,
                waited: 17 * MS,
            });
            assert_eq!(c, None, "lateness alone never steps down");
        }
    }

    #[test]
    fn a_failed_probe_doubles_the_hold() {
        // The link carries 128 kbps but not 192: every WRITE at the top
        // rate blocks.
        let mut s = Sim::new(hifi());
        let mut ups = Vec::new();
        let mut downs = Vec::new();
        let end = secs(1200);
        while s.now < end {
            let top = s.abr.rung() == 0;
            let before = s.changes.len();
            let until = s.now + 18 * MS;
            s.run(until, |_| if top { 20 * MS } else { FAST });
            for (t, c) in &s.changes[before..] {
                if c.is_down() {
                    downs.push(*t);
                } else {
                    ups.push(*t);
                }
            }
        }
        // Down within 400 ms of every probe, and the probes thin out:
        // 10, 20, 40, 80, 80, ... seconds apart.
        assert_eq!(downs.len(), ups.len() + 1);
        let gaps: Vec<u64> = ups
            .iter()
            .zip(&downs)
            .map(|(u, d)| (*u - *d).as_secs_f64().round() as u64)
            .collect();
        assert_eq!(&gaps[..6], [10, 20, 40, 80, 80, 80], "{gaps:?}");
        for (u, d) in ups.iter().zip(&downs[1..]) {
            assert!(*d - *u <= 400 * MS, "{:?}", *d - *u);
        }
        assert_eq!(s.abr.hold(), UP_HOLD_MAX);
        // Never below 128 here: the lower rate is clean.
        assert!(s.changes.iter().all(|(_, c)| c.to.kbps() >= 128));
        // A probe that holds halves the wait again, and so does every
        // further 15 s at the top: a link that was marginal once is not
        // held against it for long.
        let t0 = s.now;
        s.run(t0 + secs(81) + PROBE_WINDOW, |_| FAST);
        assert_eq!(s.abr.rung(), 0);
        assert_eq!(s.abr.hold(), UP_HOLD_MAX / 2);
        s.run(s.now + 2 * PROBE_WINDOW, |_| FAST);
        assert_eq!(s.abr.hold(), UP_HOLD_MAX / 8);
        // After a short congestion the top is back in 10 s + 10 s.
        let t0 = s.now;
        let n = s.changes.len();
        s.run(t0 + secs(1), |_| 20 * MS);
        s.run(t0 + secs(30), |_| FAST);
        assert_eq!(s.rates()[n..], [128, 88, 128, 192]);
        assert!(s.changes[n + 3].0 - t0 <= secs(22), "{:?}", &s.changes[n..]);
    }

    #[test]
    fn every_rung_has_its_own_hold() {
        // The link carries 128 kbps but not 192, and now and then it is
        // bad enough for 88 only. Coming back to 128 (a probe that holds)
        // must not shorten the wait for 192.
        let mut s = Sim::new(hifi());
        let mut tops = Vec::new();
        let end = secs(1800);
        while s.now < end {
            let rung = s.abr.rung();
            let bad = s.now.as_secs() % 300 < 2;
            let before = s.changes.len();
            s.run(s.now + 18 * MS, |_| {
                if rung == 0 || (bad && rung == 1) {
                    20 * MS
                } else {
                    FAST
                }
            });
            for (t, c) in &s.changes[before..] {
                if c.to.kbps() == 192 {
                    tops.push(*t);
                }
            }
        }
        // 192 kbps is tried after 10, 20, 40 s and then every 80 s (plus
        // what the bad spells cost): never more often.
        assert!(tops.len() >= 10, "{tops:?}");
        for w in tops[3..].windows(2) {
            assert!(w[1] - w[0] >= secs(80), "{tops:?}");
        }
        assert!(tops.len() <= 3 + 1800 / 80, "{tops:?}");
        assert_eq!(s.abr.hold(), UP_HOLD_MAX);
        let t = s.abr.time_at();
        assert!(t[1] >= secs(1500), "mostly at 128 kbps: {t:?}");
    }

    /// Most rate changes in any 60 s of `changes`.
    fn worst_minute(changes: &[(Duration, Change)]) -> usize {
        (0..changes.len())
            .map(|i| {
                changes[i..]
                    .iter()
                    .take_while(|(t, _)| *t < changes[i].0 + secs(60))
                    .count()
            })
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn a_flapping_link_does_not_oscillate() {
        // Congested for `bad` ms out of every `period` ms, for an hour.
        for (period, bad) in [
            (1_000u128, 500u128),
            (3_000, 1_000),
            (6_000, 3_000),
            (12_000, 1_500),
            (20_000, 5_000),
            (30_000, 10_000),
            (45_000, 15_000),
            (90_000, 30_000),
        ] {
            for l in [hifi(), uhq()] {
                let mut s = Sim::new(l);
                let period_of = |r: Rate| l.packet_time(r);
                let top = period_of(l.top());
                s.run(secs(3600), |t| {
                    if t.as_millis() % period < bad {
                        top + 2 * MS
                    } else {
                        FAST
                    }
                });
                let worst = worst_minute(&s.changes);
                assert!(
                    worst <= 6,
                    "{period}/{bad}: {worst} changes in a minute: {:?}",
                    &s.changes[..s.changes.len().min(12)]
                );
                // In the long run a change is rarer still.
                let late = s.changes.iter().filter(|(t, _)| *t >= secs(600)).count();
                assert!(
                    late <= 50 * 4,
                    "{period}/{bad}: {late} changes in the last 50 minutes"
                );
                // A link that is bad more often than every 10 s stays down.
                if period - bad < 10_000 {
                    assert!(
                        s.changes.iter().all(|(_, c)| c.is_down()),
                        "{period}/{bad}: {:?}",
                        s.changes
                    );
                    assert!(s.changes.len() <= 2);
                }
            }
        }
    }

    #[test]
    fn a_dropout_next_to_slow_writes_steps_down_at_once() {
        let mut abr = Abr::new(hifi());
        let mut now = secs(1);
        // A discontinuity on a healthy link (a new stream, a seek) is not
        // the link's doing.
        abr.on_packet(Obs {
            now,
            write: FAST,
            late: Duration::ZERO,
            waited: 18 * MS,
        });
        assert_eq!(abr.on_discontinuity(now), None);
        // One WRITE of 30 ms is not what cost the audio: the driver's
        // ring was full before (as it may be at START).
        now += 30 * MS;
        let c = abr.on_packet(Obs {
            now,
            write: 30 * MS,
            late: Duration::ZERO,
            waited: 18 * MS,
        });
        assert_eq!(c, None);
        assert_eq!(abr.on_discontinuity(now), None);
        // One stalled WRITE of 300 ms: the driver's ring (250 ms) overflowed.
        now += secs(5);
        let c = abr.on_packet(Obs {
            now,
            write: 300 * MS,
            late: Duration::ZERO,
            waited: 18 * MS,
        });
        assert_eq!(c, None, "one slow packet alone");
        let c = abr.on_discontinuity(now).unwrap();
        assert_eq!((c.to.kbps(), c.why), (128, Why::Dropout));
        // Not twice for the same stall.
        assert_eq!(abr.on_discontinuity(now + MS), None);
        assert_eq!(abr.rung(), 1);
    }

    #[test]
    fn a_write_error_goes_to_the_bottom_and_waits_longer() {
        let mut abr = Abr::new(hifi());
        let c = abr.on_write_error(secs(1)).unwrap();
        assert_eq!((c.from.kbps(), c.to.kbps()), (192, 88));
        assert_eq!(c.why, Why::WriteError);
        assert_eq!(abr.hold(), UP_HOLD_MIN * 2);
        assert_eq!(abr.on_write_error(secs(2)), None, "already there");
        assert_eq!(abr.changes(), 1);
        // Up again after the longer hold, one rung at a time.
        let mut s = Sim {
            abr,
            now: secs(2),
            changes: Vec::new(),
        };
        s.run(secs(2) + secs(100), |_| FAST);
        assert_eq!(s.rates(), [128, 192]);
        // The second error doubled the hold again: 40 s.
        let up = s.changes[0].0 - secs(2);
        assert!(up >= secs(40) && up <= secs(41), "{up:?}");
    }

    #[test]
    fn only_a_timed_out_write_speaks_of_congestion() {
        let os = |code| ChanError::Os {
            code,
            what: "IOCTL",
        };
        assert!(write_error_is_congestion(&ChanError::Timeout));
        assert!(write_error_is_congestion(&os(121)));
        for e in [
            ChanError::Disconnected,
            os(6),
            os(64),
            os(995),
            os(1167),
            os(1236),
            os(1450),
            ChanError::Protocol("x".into()),
        ] {
            assert!(!write_error_is_congestion(&e), "{e}");
        }
    }

    #[test]
    fn a_fixed_rate_never_changes() {
        let l = Ladder::new(Mode::Uhq96k, rate(Bitrate::K584, 1), 1005, false, true);
        let mut s = Sim::new(l);
        s.run(secs(30), |_| 50 * MS);
        assert!(s.changes.is_empty());
        assert_eq!(s.abr.on_write_error(s.now), None);
        assert_eq!(s.abr.on_discontinuity(s.now), None);
        assert_eq!(s.abr.rate(), rate(Bitrate::K584, 1));
        assert!(s.abr.slow_packets() > 0);
    }

    #[test]
    fn idle_time_is_not_clean_time() {
        let mut s = Sim::new(hifi());
        s.run(secs(1), |_| 20 * MS);
        assert_eq!(s.abr.rung(), 2);
        // The stream is suspended for an hour; then 5 s of audio.
        s.now += secs(3600);
        s.run(s.now + secs(5), |_| FAST);
        assert_eq!(s.abr.rung(), 2, "5 s of clean audio are not 10");
        s.run(s.now + secs(6), |_| FAST);
        assert_eq!(s.abr.rung(), 1);
    }

    #[test]
    fn times_and_text() {
        let mut s = Sim::new(hifi());
        s.run(secs(10), |_| FAST);
        let base = s.abr.time_at();
        s.run(secs(11), |_| 20 * MS);
        s.run(secs(13), |_| FAST);
        let text = s.abr.describe_times(&base);
        assert!(text.starts_with("192k=0."), "{text}");
        assert!(
            text.contains(" 128k=0.") && text.contains(" 88k=2."),
            "{text}"
        );
        assert_eq!(s.abr.changes(), 2);
        assert_eq!(
            s.changes[0].1.to_string(),
            "bitrate 192 -> 128 kbps: link congested (WRITE blocked 110 ms in 7 slow packets)"
        );
        let c = Change {
            from: rate(Bitrate::K584, 1),
            to: rate(Bitrate::K291, 2),
            why: Why::Dropout,
        };
        assert_eq!(
            c.to_string(),
            "bitrate 584 -> 291 kbps, 2 frame(s)/packet: audio was dropped while WRITEs blocked"
        );
        let c = Change {
            from: rate(Bitrate::K128, 1),
            to: rate(Bitrate::K192, 1),
            why: Why::Clean(secs(10)),
        };
        assert_eq!(
            c.to_string(),
            "bitrate 128 -> 192 kbps: link clean for 10 s"
        );
    }

    #[test]
    fn lateness_follows_the_endpoint_clock() {
        let mut l = Lateness::new();
        // 10 ms chunks at 48 kHz, on time.
        let mut f = 0u64;
        for k in 0..100u32 {
            f += 480;
            assert_eq!(l.observe(10 * MS * k, f, 48000), Duration::ZERO);
        }
        // The reader falls 70 ms behind and catches up.
        let t = |k: u32| 10 * MS * k;
        f += 480;
        assert_eq!(l.observe(t(100) + 70 * MS, f, 48000), 70 * MS);
        f += 480;
        assert_eq!(l.observe(t(101) + 30 * MS, f, 48000), 30 * MS);
        f += 480;
        assert_eq!(l.observe(t(102), f, 48000), Duration::ZERO);
        // A gap in the audio (frames dropped) or another rate: a new start.
        f += 48_000;
        assert_eq!(l.observe(t(103), f, 48000), Duration::ZERO);
        f += 480;
        assert_eq!(l.observe(t(104) + 5 * MS, f, 48000), 5 * MS);
        assert_eq!(l.observe(t(105) + 500 * MS, f + 960, 96000), Duration::ZERO);
        l.reset();
        assert_eq!(l.observe(t(900), 0, 96000), Duration::ZERO);
        // More than the driver can hold back is no backlog: the endpoint
        // stood still (a pause nobody told us about). A new start, and the
        // pause is not held against the chunks after it.
        let mut l = Lateness::new();
        assert_eq!(l.observe(t(0), 480, 48000), Duration::ZERO);
        assert_eq!(l.observe(t(1) + 900 * MS, 960, 48000), 900 * MS);
        assert_eq!(l.observe(t(2) + 2500 * MS, 1440, 48000), Duration::ZERO);
        assert_eq!(l.observe(t(3) + 2500 * MS, 1920, 48000), Duration::ZERO);
        assert_eq!(l.observe(t(4) + 2507 * MS, 2400, 48000), 7 * MS);
    }

    #[test]
    fn lateness_absorbs_clock_drift() {
        // The endpoint clock runs 200 ppm slow against ours: every chunk
        // arrives 2 us later than the one before. Uncorrected that is 40 ms
        // after 200 s; corrected it never exceeds what two windows collect.
        let mut l = Lateness::new();
        let mut worst = Duration::ZERO;
        for k in 1..=360_000u64 {
            let now = Duration::from_micros(k * 10_002);
            worst = worst.max(l.observe(now, k * 480, 48000));
        }
        assert!(worst <= 5 * MS, "{worst:?}");
        // The other direction re-anchors on every chunk.
        let mut l = Lateness::new();
        for k in 1..=10_000u64 {
            let now = Duration::from_micros(k * 9_998);
            assert_eq!(l.observe(now, k * 480, 48000), Duration::ZERO);
        }
    }

    /// The session on a link of limited capacity, as the fake driver
    /// models it: the endpoint renders in real time in 10 ms chunks into a
    /// ring of 250 ms; the sender sends a packet as soon as its audio is
    /// there and the WRITE before it has returned; a WRITE blocks while
    /// the link's buffer is full. So a sender that fell behind sends back
    /// to back, each WRITE taking as long as the link needs for the bytes.
    struct LinkSim {
        abr: Abr,
        now: Duration,
        /// Audio the endpoint had rendered at time zero (waiting at START).
        backlog: Duration,
        /// Audio sent or dropped so far.
        sent: Duration,
        /// Capacity in bits per second at a time, and the buffer in bytes.
        capacity: Box<dyn Fn(Duration) -> Option<u32>>,
        buffer: usize,
        queued: f64,
        queued_at: Duration,
        /// (packet number, extra WRITE time).
        stalls: Vec<(u64, Duration)>,
        packets: u64,
        dropped: Duration,
        changes: Vec<(Duration, Change)>,
    }

    const CHUNK: Duration = Duration::from_millis(10);
    const RING: Duration = Duration::from_millis(250);

    impl LinkSim {
        fn new(l: Ladder, capacity: impl Fn(Duration) -> Option<u32> + 'static) -> Self {
            LinkSim {
                abr: Abr::new(l),
                now: Duration::ZERO,
                backlog: Duration::ZERO,
                sent: Duration::ZERO,
                capacity: Box::new(capacity),
                buffer: 2000,
                queued: 0.0,
                queued_at: Duration::ZERO,
                stalls: Vec::new(),
                packets: 0,
                dropped: Duration::ZERO,
                changes: Vec::new(),
            }
        }
        /// Audio rendered by `t`, in whole chunks.
        fn rendered(&self, t: Duration) -> Duration {
            let chunks = (t.as_micros() / CHUNK.as_micros()) as u32;
            self.backlog + CHUNK * chunks
        }
        /// When `audio` has been rendered.
        fn rendered_at(&self, audio: Duration) -> Duration {
            let need = audio.saturating_sub(self.backlog);
            CHUNK * need.as_micros().div_ceil(CHUNK.as_micros()) as u32
        }
        fn link_take(&mut self, len: usize) -> Duration {
            let Some(bps) = (self.capacity)(self.now) else {
                self.queued = 0.0;
                return Duration::ZERO;
            };
            let rate = f64::from(bps) / 8.0;
            let since = (self.now - self.queued_at).as_secs_f64();
            self.queued = (self.queued - since * rate).max(0.0);
            self.queued_at = self.now;
            self.queued += len as f64;
            let over = self.queued - self.buffer as f64;
            if over > 0.0 {
                Duration::from_secs_f64(over / rate)
            } else {
                Duration::ZERO
            }
        }
        fn run(&mut self, until: Duration) {
            while self.now < until {
                let rate = self.abr.rate();
                let audio = self.abr.ladder().packet_time(rate);
                let ready = self.rendered_at(self.sent + audio);
                let waited = ready.saturating_sub(self.now);
                let late = self.now.saturating_sub(ready);
                self.now = self.now.max(ready);
                let stall: Duration = self
                    .stalls
                    .iter()
                    .filter(|(k, _)| *k == self.packets)
                    .map(|(_, d)| *d)
                    .sum();
                let bytes = rate.packet_bytes(self.abr.ladder().mode());
                let write = FAST + stall + self.link_take(bytes);
                self.now += write;
                self.sent += audio;
                self.packets += 1;
                let mut c = self.abr.on_packet(Obs {
                    now: self.now,
                    write,
                    late,
                    waited,
                });
                // The ring overflowed while the WRITE blocked.
                let behind = self.rendered(self.now).saturating_sub(self.sent);
                if behind > RING {
                    self.dropped += behind - RING;
                    self.sent += behind - RING;
                    c = c.or(self.abr.on_discontinuity(self.now));
                }
                if let Some(c) = c {
                    self.changes.push((self.now, c));
                }
            }
        }
        fn rates(&self) -> Vec<u32> {
            self.changes.iter().map(|(_, c)| c.to.kbps()).collect()
        }
    }

    #[test]
    fn a_limited_link_is_still_found_out_fast() {
        // (ladder, link bps from 5 s on, the rates it must end up at).
        for (l, bps, want) in [
            (hifi(), 170_000, vec![128]),
            (hifi(), 120_000, vec![128, 88]),
            (hifi(), 60_000, vec![128, 88]),
            (uhq(), 400_000, vec![291]),
            (uhq(), 200_000, vec![291, 152]),
        ] {
            let mut s = LinkSim::new(l, move |t| (t >= secs(5)).then_some(bps));
            s.run(secs(9));
            assert_eq!(s.rates(), want, "{bps}: {:?}", s.changes);
            // The link's buffer fills first; then it is a matter of 0.3 s.
            let first = s.changes[0].0 - secs(5);
            assert!(first <= 1500 * MS, "{bps}: {first:?}");
            // No audio is lost on the way, if the link carries any rate.
            if bps >= 110_000 {
                assert_eq!(s.dropped, Duration::ZERO, "{bps}");
            }
        }
    }

    #[test]
    fn a_link_that_carries_the_middle_rate_settles_there() {
        // Between what the middle rate needs on the wire (HiFi 128 kbps:
        // 149.3; UHQ 291 kbps: 298.7) and what the top needs. The backlog
        // of a failed probe goes out back to back at the middle rate into
        // a full buffer: that is the link taking what it can, not a link
        // too small for the middle rate.
        for (l, band) in [
            (
                hifi(),
                [151_000, 155_000, 160_000, 165_000, 175_000, 200_000],
            ),
            (
                uhq(),
                [302_000, 310_000, 330_000, 360_000, 450_000, 550_000],
            ),
        ] {
            for bps in band {
                for buffer in [2000, 8000, 16_000] {
                    let mut s = LinkSim::new(l, move |t| (t >= secs(5)).then_some(bps));
                    s.buffer = buffer;
                    s.run(secs(600));
                    let ctx = format!("{bps} bps, buffer {buffer}: {:?}", s.rates());
                    let mid = l.rungs()[1].kbps();
                    let top = l.top().kbps();
                    for (k, r) in s.rates().iter().enumerate() {
                        assert_eq!(*r, if k % 2 == 0 { mid } else { top }, "{ctx}");
                    }
                    // Probes after 10, 20, 40, 80, 80, ... s: 9 in 10 min.
                    assert!((15..=19).contains(&s.changes.len()), "{ctx}");
                    assert!(worst_minute(&s.changes) <= 6, "{ctx}");
                    assert_eq!(s.abr.hold(), UP_HOLD_MAX, "{ctx}");
                    let t = s.abr.time_at();
                    // (A big buffer takes a probe a few seconds to fill.)
                    assert!(t[2].is_zero() && t[1] >= secs(520), "{ctx}: {t:?}");
                    assert_eq!(s.dropped, Duration::ZERO, "{ctx}");
                }
            }
        }
    }

    #[test]
    fn a_link_that_drops_to_the_middle_rate_now_and_then_does_not_flap() {
        // For 1 s of every 3 s the link carries the middle rate only, for
        // an hour. The top rate fails each time; the middle one never
        // does, and its packets going out back to back after a failed
        // probe must not send the stream on to the bottom.
        for (l, bps) in [(hifi(), 154_000), (uhq(), 310_000)] {
            for buffer in [2000, 8000] {
                let mut s = LinkSim::new(l, move |t| (t.as_secs() % 3 == 0).then_some(bps));
                s.buffer = buffer;
                s.run(secs(3600));
                let ctx = format!("{bps} bps, buffer {buffer}");
                let head = &s.changes[..s.changes.len().min(12)];
                assert!(worst_minute(&s.changes) <= 6, "{ctx}: {head:?}");
                assert!(s.changes.len() <= 100, "{ctx}: {}", s.changes.len());
                let t = s.abr.time_at();
                assert!(t[2] <= secs(36), "{ctx}: {t:?}");
                // (The bigger buffer takes the bad second without a change.)
                assert!(t[0] + t[1] >= secs(3560), "{ctx}: {t:?}");
                assert!(buffer > 2000 || t[1] >= secs(3400), "{ctx}: {t:?}");
                assert_eq!(s.dropped, Duration::ZERO, "{ctx}");
            }
        }
    }

    #[test]
    fn two_packets_per_wake_do_not_keep_the_burst_measure() {
        // A paced sender whose audio arrives two packets at a time: every
        // other packet is not waited for. After a stall the usual measure
        // must still come back, and a busy link be seen as before.
        let mut abr = Abr::new(hifi());
        let mut now = secs(1);
        let mut send = |abr: &mut Abr, k: u32, write: Duration| {
            let waited = if k & 1 == 0 {
                (36 * MS).saturating_sub(2 * write)
            } else {
                Duration::ZERO
            };
            now += waited + write;
            abr.on_packet(Obs {
                now,
                write,
                late: Duration::ZERO,
                waited,
            })
        };
        for k in 0..100 {
            assert_eq!(send(&mut abr, k, FAST), None);
        }
        assert!(!abr.is_draining());
        // A stall, and the backlog after it.
        assert_eq!(send(&mut abr, 0, 150 * MS), None);
        for _ in 0..8 {
            assert_eq!(send(&mut abr, 1, FAST), None);
        }
        assert!(abr.is_draining());
        for k in 0..8 {
            assert_eq!(send(&mut abr, k, FAST), None);
        }
        assert!(!abr.is_draining());
        // WRITEs of 9 ms each (half of the time): down within a second.
        let mut down = None;
        for k in 0..60 {
            if let Some(c) = send(&mut abr, k, 9 * MS) {
                down = Some((k, c.to.kbps()));
                break;
            }
        }
        assert!(matches!(down, Some((k, 128)) if k < 50), "{down:?}");
    }

    #[test]
    fn catching_up_through_a_good_link_is_not_congestion() {
        // A link with room to spare, but not without limit: after one
        // stalled WRITE the backlog goes out back to back, every WRITE
        // waiting for room in the link's buffer. Up to a link four times
        // the stream that is "a quarter of the packet time" per WRITE.
        for (l, wire) in [(hifi(), 220_000u32), (uhq(), 597_000)] {
            for bps in [wire * 23 / 20, wire * 3 / 2, wire * 2, wire * 3, wire * 4] {
                for buffer in [2000, 8000] {
                    for stall in [30, 80, 150, 230] {
                        let mut s = LinkSim::new(l, move |_| Some(bps));
                        s.buffer = buffer;
                        s.stalls = vec![(300, stall * MS), (1500, stall * MS)];
                        s.run(secs(60));
                        let ctx = format!("{bps} bps, buffer {buffer}, stall {stall} ms");
                        assert!(s.changes.is_empty(), "{ctx}: {:?}", s.changes);
                        assert_eq!(s.dropped, Duration::ZERO, "{ctx}");
                        assert!(!s.abr.is_draining(), "{ctx}");
                    }
                }
            }
        }
    }

    #[test]
    fn audio_waiting_at_start_is_not_congestion() {
        // The driver's ring holds up to 250 ms when the stream starts.
        for (l, wire) in [(hifi(), 220_000u32), (uhq(), 597_000)] {
            for bps in [wire * 23 / 20, wire * 3 / 2, wire * 2, wire * 3, wire * 4] {
                for buffer in [2000, 8000] {
                    for backlog in [50, 100, 150, 250] {
                        let mut s = LinkSim::new(l, move |_| Some(bps));
                        s.buffer = buffer;
                        s.backlog = backlog * MS;
                        s.run(secs(30));
                        let ctx = format!("{bps} bps, buffer {buffer}, {backlog} ms waiting");
                        assert!(s.changes.is_empty(), "{ctx}: {:?}", s.changes);
                        assert!(!s.abr.is_draining(), "{ctx}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_stall_that_loses_audio_on_a_limited_link_steps_down_once() {
        for l in [hifi(), uhq()] {
            let mut s = LinkSim::new(l, |_| Some(1_500_000));
            s.stalls = vec![(300, 400 * MS)];
            s.run(secs(9));
            assert_eq!(s.changes.len(), 1, "{:?}", s.changes);
            assert_eq!(s.changes[0].1.why, Why::Dropout);
            assert!(s.dropped > Duration::ZERO);
            // And back after 10 s.
            s.run(secs(30));
            assert_eq!(s.changes.len(), 2, "{:?}", s.changes);
            assert_eq!(s.abr.rung(), 0);
        }
    }
}
