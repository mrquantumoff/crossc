//! `play`: stream live-encoded SSC to the sink.
//!
//! Flow (all over the existing signaling channel, kept open throughout):
//! DISCOVER, GET_ALL_CAPABILITIES, pick the SSC SEP and check its capability
//! octet allows the chosen configuration, SET_CONFIGURATION (Media Transport,
//! SSC with the one-octet configuration, Delay Reporting if offered), OPEN,
//! open the media channel, START, stream, then CLOSE (ABORT if refused).
//!
//! While streaming, the signaling channel belongs to a reader thread that
//! answers sink-initiated commands ([`crate::peer`]); PCM is generated and
//! encoded by a producer thread into a bounded queue; the calling thread
//! paces RTP packets against a [`Clock`] and writes them to the media channel.
//!
//! Media packet = 12-byte RTP header (V=2, PT=96, fixed SSRC) + `fpp` SSC
//! frames back to back. No payload header octet, no fragmentation. The RTP
//! timestamp advances by 1 per packet (what Samsung's stack is reported to do)
//! or by the samples per packet (`--ts samples`).
//!
//! `--adaptive`: the bitrate follows the link with the service's controller
//! ([`crossc_core::abr`]), from the mode's top rate down its ladder. The
//! pacing thread tells the controller how long each WRITE took, how long it
//! waited for the packet's send time and how late the packet went out; the
//! producer encodes each packet at the rate the controller is at. Packets
//! already encoded (at most [`ADAPTIVE_QUEUE`], plus the one in hand) still
//! go out at the old rate, and the frames per packet stay as given, so the
//! pacing schedule does not change with the rate. `--bitrate` is the
//! opposite: exactly that rate, whatever happens.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use avdtp::caps;
use avdtp::codec::SscFlags;
use avdtp::msg;
use avdtp::rtp::RtpHeader;
use avdtp::signal::{MessageType, SignalId};
use crossc_core::abr::{Abr, Ladder, Obs, Rate, write_error_is_congestion};
use crossc_core::peer::{LocalStream, PeerEvent};
pub use crossc_core::rtp::{KNOWN_BUDS_MEDIA_MTU, Packetizer, TsMode, packet_deadline};
use crossc_core::ssc::{choose_ssc, local_caps};
use ssc_enc::{Bitrate, Encoder, FRAME_SAMPLES, MAX_FRAME_BYTES, Mode};

use crate::channel::{ChanError, SduChannel};
use crate::hex;
use crate::media::{self, Choice, INT_SEID};
use crate::pace::Clock;
use crate::pcm::Source;
use crate::probe::{self, Signaling};

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayMode {
    Hifi,
    Uhq,
    Ssc48,
    Ssc44,
}

impl PlayMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "hifi" => PlayMode::Hifi,
            "uhq" => PlayMode::Uhq,
            "ssc48" => PlayMode::Ssc48,
            "ssc44" => PlayMode::Ssc44,
            o => return Err(format!("unknown --mode {o:?} (hifi|uhq|ssc48|ssc44)")),
        })
    }
    pub fn encoder_mode(self) -> Mode {
        match self {
            PlayMode::Hifi => Mode::Hifi48k,
            PlayMode::Uhq => Mode::Uhq96k,
            PlayMode::Ssc48 => Mode::Ssc48k,
            PlayMode::Ssc44 => Mode::Ssc44k1,
        }
    }
    pub fn sample_rate(self) -> u32 {
        self.encoder_mode().sample_rate()
    }
    pub fn default_config(self) -> u8 {
        match self {
            PlayMode::Hifi => SscFlags::CONFIG_HIFI,
            PlayMode::Uhq => SscFlags::CONFIG_UHQ_V2,
            PlayMode::Ssc48 => SscFlags::CONFIG_SSC_48K,
            PlayMode::Ssc44 => SscFlags::CONFIG_SSC_44K1,
        }
    }
    /// Default bitrate. HiFi 192 (the phone's max) and UHQ 584 are from the
    /// community notes; 328 for plain SSC is a guess (its top "hq" F-rate).
    pub fn default_bitrate(self) -> Bitrate {
        match self {
            PlayMode::Hifi => Bitrate::K192,
            PlayMode::Uhq => Bitrate::K584,
            PlayMode::Ssc48 | PlayMode::Ssc44 => Bitrate::K328,
        }
    }
    /// The capability bit that names this mode.
    pub fn mode_bit(self) -> u8 {
        match self {
            PlayMode::Hifi => SscFlags::HIFI_48K_24,
            PlayMode::Uhq => SscFlags::UHQ_V2_96K_24,
            PlayMode::Ssc48 => SscFlags::SR_48K,
            PlayMode::Ssc44 => SscFlags::SR_44K1,
        }
    }
    /// 24-bit modes: synthetic sources are 24-bit and 16-bit WAVs go in as
    /// I24 (`<< 8`), as the encoder docs do for HiFi/UHQ.
    pub fn hires(self) -> bool {
        matches!(self, PlayMode::Hifi | PlayMode::Uhq)
    }
    pub fn name(self) -> &'static str {
        match self {
            PlayMode::Hifi => "HiFi 48 kHz/24",
            PlayMode::Uhq => "UHQ v2 96 kHz/24",
            PlayMode::Ssc48 => "SSC 48 kHz",
            PlayMode::Ssc44 => "SSC 44.1 kHz",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceSpec {
    Tone { hz: f64, dbfs: f64 },
    Sweep,
    Wav(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlayArgs {
    pub mode: PlayMode,
    pub config: u8,
    pub bitrate: Bitrate,
    pub fpp: usize,
    pub ts: TsMode,
    /// None = 20 s for tone/sweep, whole file for WAV.
    pub seconds: Option<f64>,
    pub source: SourceSpec,
    pub delay_report: bool,
    pub lead: u32,
    pub force_mtu: Option<usize>,
    pub force_config: bool,
    pub dry_run: bool,
    /// The bitrate follows the link, from `bitrate` down the mode's ladder.
    pub adaptive: bool,
}

/// Packets the producer may have encoded ahead in adaptive mode (16
/// otherwise): this many more go out at the old rate after a change.
pub const ADAPTIVE_QUEUE: usize = 2;

pub const DEFAULT_SECONDS: f64 = 20.0;
pub const DEFAULT_LEAD: u32 = 3;

pub const PLAY_USAGE: &str =
    "play [--mode hifi|uhq|ssc48|ssc44] [--config 0xNN] [--bitrate KBPS | --adaptive] [--frames-per-packet N]
     [--ts packets|samples] [--seconds S] [--tone HZ [--level DBFS] | --sweep | --wav PATH]
     [--no-delay-report] [--lead N] [--force-mtu N] [--force-config] [--dry-run]";

fn num_f64(s: Option<&String>, what: &str) -> Result<f64, String> {
    let s = s.ok_or(format!("{what} needs a value"))?;
    let v: f64 = s.parse().map_err(|_| format!("{what}: bad number {s:?}"))?;
    if v.is_finite() {
        Ok(v)
    } else {
        Err(format!("{what}: bad number {s:?}"))
    }
}

pub fn parse_play_args(args: &[String]) -> Result<PlayArgs, String> {
    let mut mode = PlayMode::Hifi;
    let mut config = None;
    let mut kbps = None;
    let mut fpp = 1usize;
    let mut ts = TsMode::Packets;
    let mut seconds = None;
    let mut tone = None;
    let mut level = -20.0;
    let mut level_given = false;
    let mut sweep = false;
    let mut wav = None;
    let mut delay_report = true;
    let mut lead = DEFAULT_LEAD;
    let mut force_mtu = None;
    let mut force_config = false;
    let mut dry_run = false;
    let mut adaptive = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |what: &str| -> Result<u32, String> {
            hex::parse_num(it.next().ok_or(format!("{what} needs a value"))?)
        };
        match a.as_str() {
            "--mode" => {
                mode = PlayMode::parse(args_next(&mut it, "--mode")?)?;
            }
            "--config" => {
                config = Some(u8::try_from(val("--config")?).map_err(|_| "--config is one octet")?)
            }
            "--bitrate" => kbps = Some(val("--bitrate")?),
            "--frames-per-packet" => {
                fpp = val("--frames-per-packet")? as usize;
                if !(1..=8).contains(&fpp) {
                    return Err("--frames-per-packet must be 1..8".into());
                }
            }
            "--ts" => {
                ts = match args_next(&mut it, "--ts")? {
                    "packets" => TsMode::Packets,
                    "samples" => TsMode::Samples,
                    o => return Err(format!("--ts {o:?}: packets|samples")),
                }
            }
            "--seconds" => {
                let s = num_f64(it.next(), "--seconds")?;
                if s <= 0.0 || s > 24.0 * 3600.0 {
                    return Err("--seconds must be > 0".into());
                }
                seconds = Some(s);
            }
            "--tone" => {
                let hz = num_f64(it.next(), "--tone")?;
                if hz <= 0.0 {
                    return Err("--tone must be > 0 Hz".into());
                }
                tone = Some(hz);
            }
            "--level" => {
                level = num_f64(it.next(), "--level")?;
                level_given = true;
                if level > 0.0 {
                    return Err("--level is dBFS, <= 0".into());
                }
            }
            "--sweep" => sweep = true,
            "--wav" => wav = Some(args_next(&mut it, "--wav")?.to_string()),
            "--no-delay-report" => delay_report = false,
            "--lead" => {
                lead = val("--lead")?;
                if lead > 100 {
                    return Err("--lead must be 0..100 packets".into());
                }
            }
            "--force-mtu" => force_mtu = Some(val("--force-mtu")? as usize),
            "--force-config" => force_config = true,
            "--dry-run" => dry_run = true,
            "--adaptive" => adaptive = true,
            o => {
                return Err(format!(
                    "play: unknown argument {o:?}\nusage: crossc-cli {PLAY_USAGE}"
                ));
            }
        }
    }
    let n_src = usize::from(tone.is_some()) + usize::from(sweep) + usize::from(wav.is_some());
    if n_src > 1 {
        return Err("pick one source: --tone, --sweep or --wav".into());
    }
    if level_given && tone.is_none() && n_src == 1 {
        return Err("--level only applies to --tone".into());
    }
    let source = match (tone, sweep, wav) {
        (_, true, _) => SourceSpec::Sweep,
        (_, _, Some(p)) => SourceSpec::Wav(p),
        (hz, _, _) => SourceSpec::Tone {
            hz: hz.unwrap_or(1000.0),
            dbfs: level,
        },
    };
    if adaptive && kbps.is_some() {
        return Err(
            "--bitrate forces a fixed rate and --adaptive lets it follow the link: pick one".into(),
        );
    }
    let bitrate = match kbps {
        // The ladder starts at the highest rate the buds are known to
        // decode in the mode (plain SSC: 229, not the fixed default 328).
        None if adaptive => {
            let top = crossc_core::abr::decodable(mode.encoder_mode())[0];
            if top.bps() < mode.default_bitrate().bps() {
                top
            } else {
                mode.default_bitrate()
            }
        }
        None => mode.default_bitrate(),
        Some(k) => Bitrate::from_bps(k.saturating_mul(1000)).ok_or_else(|| {
            let all: Vec<String> = Bitrate::ALL
                .iter()
                .map(|b| (b.bps() / 1000).to_string())
                .collect();
            format!("--bitrate {k}: SSC rates are {} kbps", all.join(", "))
        })?,
    };
    Ok(PlayArgs {
        mode,
        config: config.unwrap_or(mode.default_config()),
        bitrate,
        fpp,
        ts,
        seconds,
        source,
        delay_report,
        lead,
        force_mtu,
        force_config,
        dry_run,
        adaptive,
    })
}

fn args_next<'a>(it: &mut std::slice::Iter<'a, String>, what: &str) -> Result<&'a str, String> {
    it.next()
        .map(String::as_str)
        .ok_or(format!("{what} needs a value"))
}

impl PlayArgs {
    pub fn frame_bytes(&self) -> usize {
        ssc_enc::frame_size(self.mode.encoder_mode(), self.bitrate)
    }
    pub fn packet_bytes(&self) -> usize {
        RtpHeader::LEN + self.fpp * self.frame_bytes()
    }
    pub fn packet_duration(&self) -> Duration {
        packet_deadline(1, self.fpp, self.mode.sample_rate())
    }
    /// The rates the stream may use: `bitrate` alone, or with `--adaptive`
    /// the mode's ladder under it (every rung at `fpp` frames per packet,
    /// so each fits wherever the top rate fits).
    pub fn ladder(&self) -> Ladder {
        let top = Rate {
            bitrate: self.bitrate,
            frames_per_packet: self.fpp,
        };
        Ladder::new(
            self.mode.encoder_mode(),
            top,
            usize::MAX,
            self.adaptive,
            false,
        )
    }
    /// Bitrate/mode combinations outside what is known to work.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        let e = self.bitrate.is_e_family();
        if self.mode == PlayMode::Uhq && !e {
            w.push(format!(
                "UHQ normally uses the E-family rates (584/291/152); {} kbps is untested",
                self.bitrate.bps() / 1000
            ));
        }
        if self.mode != PlayMode::Uhq && e {
            w.push(format!(
                "{} kbps is an E-family (UHQ) rate; untested in {}",
                self.bitrate.bps() / 1000,
                self.mode.name()
            ));
        }
        if self.mode == PlayMode::Hifi && self.bitrate.bps() > Bitrate::K229.bps() {
            w.push(format!(
                "HiFi above 229 kbps is outside the known range (phone max 192, Buds3 Pro firmware up to 229); {} kbps is untested",
                self.bitrate.bps() / 1000
            ));
        }
        if self.packet_bytes() > KNOWN_BUDS_MEDIA_MTU {
            w.push(format!(
                "{}-byte packets exceed {KNOWN_BUDS_MEDIA_MTU} (the phone's Buds4 Pro media MTU); play will refuse unless the reported MTU allows it or --force-mtu",
                self.packet_bytes()
            ));
        }
        if self.config & self.mode.mode_bit() == 0 {
            w.push(format!(
                "--config {:#04x} lacks the {} bit {:#04x}; the sink may decode another mode",
                self.config,
                self.mode.name(),
                self.mode.mode_bit()
            ));
        }
        w
    }

    /// Build the PCM source (reads the WAV file).
    pub fn build_source(&self) -> Result<Source, String> {
        let rate = self.mode.sample_rate();
        Ok(match &self.source {
            SourceSpec::Tone { hz, dbfs } => {
                if *hz >= f64::from(rate) / 2.0 {
                    return Err(format!("--tone {hz} Hz is above Nyquist for {rate} Hz"));
                }
                Source::tone(*hz, *dbfs, rate, self.mode.hires())
            }
            SourceSpec::Sweep => Source::sweep(
                rate,
                self.seconds.unwrap_or(DEFAULT_SECONDS),
                self.mode.hires(),
            ),
            SourceSpec::Wav(p) => {
                let bytes = std::fs::read(p).map_err(|e| format!("{p}: {e}"))?;
                let wav = crate::wav::Wav::parse(&bytes).map_err(|e| format!("{p}: {e}"))?;
                if wav.frames() == 0 {
                    return Err(format!("{p}: the data chunk has no audio"));
                }
                let wav = if wav.sample_rate != rate {
                    eprintln!(
                        "{p}: resampling {} Hz -> {rate} Hz for {} (windowed sinc, {} lobes each side)...",
                        wav.sample_rate,
                        self.mode.name(),
                        crate::resample::ZERO_CROSSINGS
                    );
                    crate::resample::resample_wav(wav, rate)
                } else {
                    wav
                };
                if wav.frames() == 0 {
                    return Err(format!("{p}: the data chunk has no audio"));
                }
                Source::Wav {
                    wav,
                    pos: 0,
                    hires: self.mode.hires(),
                }
            }
        })
    }

    /// Frames to send for `source`.
    pub fn total_frames(&self, source: &Source) -> u64 {
        let rate = f64::from(self.mode.sample_rate());
        let by_time = |s: f64| (s * rate / FRAME_SAMPLES as f64).ceil() as u64;
        let n = match (self.seconds, source.total_frames()) {
            (Some(s), Some(f)) => by_time(s).min(f),
            (Some(s), None) => by_time(s),
            (None, Some(f)) => f,
            (None, None) => by_time(DEFAULT_SECONDS),
        };
        // Whole packets only (the last one is padded with silence).
        n.div_ceil(self.fpp as u64) * self.fpp as u64
    }
}

// ---------------------------------------------------------------------------
// Configuration checks
// ---------------------------------------------------------------------------

/// `config` must only use bits the sink's capability octet offers, and UHQ
/// must not be disabled by the sink, unless `force`.
pub fn check_config(
    caps: SscFlags,
    config: u8,
    mode: PlayMode,
    force: bool,
) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    let mut fail = |s: String| -> Result<(), String> {
        if force {
            notes.push(format!("{s} (continuing: --force-config)"));
            Ok(())
        } else {
            Err(format!("{s}; pass --force-config to send it anyway"))
        }
    };
    let missing = caps.missing_for(SscFlags(config));
    if missing != 0 {
        let names: Vec<&str> = SscFlags(missing).names().collect();
        fail(format!(
            "configuration {config:#04x} uses bit(s) {missing:#04x} ({}) the sink does not offer (caps {:#04x})",
            names.join(", "),
            caps.0
        ))?;
    }
    if mode == PlayMode::Uhq && caps.has(SscFlags::SINK_DISABLES_UHQ_V2) {
        fail(format!(
            "the sink sets 0x01 (UHQ v2 disabled) in caps {:#04x}",
            caps.0
        ))?;
    }
    Ok(notes)
}

// ---------------------------------------------------------------------------
// MTU
// ---------------------------------------------------------------------------

/// Largest media packet we allow on a channel the driver reported with
/// (`out_mtu`, `in_mtu`) after we asked for `requested_in_mtu`: the rule of
/// [`crossc_core::rtp::media_mtu_limit`], unless `force` (`--force-mtu`)
/// overrides everything.
pub fn media_mtu_limit(
    out_mtu: u16,
    in_mtu: u16,
    requested_in_mtu: u16,
    force: Option<usize>,
) -> Result<(usize, String), String> {
    if let Some(f) = force {
        return Ok((f, format!("--force-mtu {f}")));
    }
    crossc_core::rtp::media_mtu_limit(out_mtu, in_mtu, requested_in_mtu).ok_or_else(|| {
        format!(
            "media channel reported no plausible MTU (out_mtu={out_mtu} in_mtu={in_mtu}); pass --force-mtu N"
        )
    })
}

// ---------------------------------------------------------------------------
// Producer (PCM + encoder)
// ---------------------------------------------------------------------------

/// One media packet's payload (frames back to back).
pub struct Payload {
    pub bytes: Vec<u8>,
    pub frames: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EncodeStats {
    pub frames: u64,
    pub encode_time: Duration,
}

impl EncodeStats {
    /// Audio seconds encoded per wall second.
    pub fn realtime_factor(&self, rate: u32) -> f64 {
        let audio = self.frames as f64 * FRAME_SAMPLES as f64 / f64::from(rate);
        audio / self.encode_time.as_secs_f64().max(1e-9)
    }
}

/// Generate and encode `total_frames` into `tx`, `fpp` frames per payload,
/// each payload at the rate of `ladder` that `rung` names when it is begun.
/// Stops early when the receiver hangs up.
fn produce(
    args: &PlayArgs,
    mut source: Source,
    total_frames: u64,
    tx: mpsc::SyncSender<Payload>,
    ladder: &Ladder,
    rung: &AtomicUsize,
) -> Result<EncodeStats, String> {
    let mut enc = Encoder::new_boxed(args.mode.encoder_mode(), source.channels())
        .map_err(|e| format!("encoder: {e}"))?;
    let mut buf = source.new_buf();
    let mut frame = [0u8; MAX_FRAME_BYTES];
    let mut st = EncodeStats::default();
    let mut sent = 0u64;
    while sent < total_frames {
        let mut p = Payload {
            bytes: Vec::with_capacity(args.fpp * args.frame_bytes()),
            frames: 0,
        };
        let rungs = ladder.rungs();
        let bitrate = rungs[rung.load(Ordering::Relaxed).min(rungs.len() - 1)].bitrate;
        for _ in 0..args.fpp {
            source.fill(&mut buf); // past the end: silence (buffer zeroed)
            let t0 = Instant::now();
            let n = enc
                .encode_frame(buf.pcm(), bitrate, &mut frame)
                .map_err(|e| format!("encoder: {e}"))?;
            st.encode_time += t0.elapsed();
            st.frames += 1;
            p.bytes.extend_from_slice(&frame[..n]);
            p.frames += 1;
        }
        sent += p.frames as u64;
        if tx.send(p).is_err() {
            break;
        }
    }
    Ok(st)
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// What the signaling reader thread reports.
#[derive(Debug)]
pub enum SigEvent {
    Peer(PeerEvent),
    Trace(String),
    /// The signaling channel failed (disconnect or I/O error); the reader stopped.
    Dead(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    Completed,
    Interrupted,
    SinkEnded(PeerEvent),
    SignalingLost(String),
    MediaLost(String),
    MediaErrors(String),
    Encoder(String),
}

impl EndReason {
    pub fn is_error(&self) -> bool {
        !matches!(self, EndReason::Completed | EndReason::Interrupted)
    }
    /// Whether the stream is still ours to CLOSE.
    fn may_release(&self) -> bool {
        !matches!(
            self,
            EndReason::SignalingLost(_)
                | EndReason::SinkEnded(PeerEvent::Closed | PeerEvent::Aborted)
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub packets: u64,
    pub frames: u64,
    pub bytes: u64,
    pub write_total: Duration,
    pub write_max: Duration,
    /// Sent more than [`LATE_TOLERANCE`] after the intended send time
    /// (deadline minus lead): any send-side hiccup shows here.
    pub late: u64,
    /// Worst lateness against the intended send time.
    pub max_late: Duration,
    /// Sent after the packet's playout deadline (the sink has, by our
    /// schedule, run dry). Each one re-anchors the schedule, see
    /// [`stream_packets`].
    pub missed: u64,
    pub write_errors: u64,
}

impl Stats {
    pub fn render(&self, elapsed: Duration) -> String {
        let avg = if self.packets > 0 {
            self.write_total / u32::try_from(self.packets).unwrap_or(u32::MAX)
        } else {
            Duration::ZERO
        };
        format!(
            "t={:.1}s packets={} frames={} bytes={} WRITE avg={}us max={}us late={} (worst {:.1} ms) missed={} write_errors={}",
            elapsed.as_secs_f64(),
            self.packets,
            self.frames,
            self.bytes,
            avg.as_micros(),
            self.write_max.as_micros(),
            self.late,
            self.max_late.as_secs_f64() * 1e3,
            self.missed,
            self.write_errors
        )
    }
}

#[derive(Debug)]
pub struct Outcome {
    pub reason: EndReason,
    pub elapsed: Duration,
    pub stats: Stats,
    pub encode: EncodeStats,
    pub delays: Vec<u16>,
    /// Bitrate changes the controller made (`--adaptive`).
    pub rate_changes: u32,
    /// Audio sent at each rate of the ladder: "192k=12.3s 128k=7.7s".
    pub rates: String,
}

impl Outcome {
    /// Ended before any packet was sent.
    fn early(reason: EndReason, delays: Vec<u16>) -> Self {
        Outcome {
            reason,
            elapsed: Duration::ZERO,
            stats: Stats::default(),
            encode: EncodeStats::default(),
            delays,
            rate_changes: 0,
            rates: String::new(),
        }
    }
}

/// A packet counts as late when sent more than this after its intended send
/// time (timer and scheduling jitter below this is not interesting).
pub const LATE_TOLERANCE: Duration = Duration::from_micros(500);
/// Consecutive non-disconnect WRITE failures before giving up.
const MAX_CONSECUTIVE_WRITE_ERRORS: u32 = 5;
const STATS_EVERY: Duration = Duration::from_secs(2);

/// Pace and send packets until the source ends, `stop` is set, or a channel
/// fails. `events` (from the signaling reader) is polled between packets.
///
/// Packet k is due (playout deadline) at `anchor + (k - anchor_k) * period`
/// and is sent `lead` periods earlier, but never before `anchor`, so the
/// first `lead + 1` packets go out back to back. If a packet still goes out
/// after its playout deadline (a stalled WRITE, a descheduled thread), the
/// schedule is re-anchored at that packet instead of bursting the whole
/// backlog: it is due now and the following `lead` packets go out back to
/// back, as at stream start. No audio is dropped. After the last packet
/// the function waits (stop-aware) until that packet's deadline, so CLOSE
/// does not cut off the audio still buffered in the sink.
#[allow(clippy::too_many_arguments)]
pub fn stream_packets<M: SduChannel, K: Clock>(
    args: &PlayArgs,
    source: Source,
    media: &mut M,
    clock: &mut K,
    stop: &AtomicBool,
    events: Option<&mpsc::Receiver<SigEvent>>,
    verbose: bool,
    log: &mut dyn FnMut(String),
) -> Outcome {
    let total = args.total_frames(&source);
    let rate = args.mode.sample_rate();
    let lead = packet_deadline(u64::from(args.lead), args.fpp, rate);
    let ladder = args.ladder();
    // What the producer has encoded ahead goes out at the rate it was
    // encoded at: keep that short when the rate is to follow the link.
    let depth = if ladder.is_adaptive() {
        ADAPTIVE_QUEUE
    } else {
        16
    };
    let (tx, rx) = mpsc::sync_channel::<Payload>(depth);
    let mut abr = Abr::new(ladder);
    let rung = AtomicUsize::new(0);
    let mut stats = Stats::default();
    let mut delays = Vec::new();
    let mut reason = EndReason::Completed;
    let mut pk = Packetizer::new(args.ts, args.fpp);
    let mut pkt = Vec::with_capacity(args.packet_bytes());
    // Schedule origin; reset once the first payload is in hand so encoder
    // start-up (allocation, first frame) does not count as lateness.
    let mut start = clock.now();
    let (encode, elapsed) = std::thread::scope(|s| {
        // Owned by this closure so a panic below drops it (unblocking the
        // producer) before the scope joins.
        let rx = rx;
        let (ladder, rung) = (&ladder, &rung);
        let producer = s.spawn(move || produce(args, source, total, tx, ladder, rung));
        let mut next_stats = STATS_EVERY;
        let mut consecutive_errors = 0u32;
        let mut k = 0u64;
        let mut anchor = start;
        let mut anchor_k = 0u64;
        // When the WRITE before returned.
        let mut last_write: Option<Duration> = None;
        'outer: loop {
            if stop.load(Ordering::Relaxed) {
                reason = EndReason::Interrupted;
                break;
            }
            if let Some(ev) = events {
                while let Ok(e) = ev.try_recv() {
                    let t = clock.now() - start;
                    match e {
                        SigEvent::Trace(l) if verbose => {
                            log(format!("[{:8.3}] {l}", t.as_secs_f64()))
                        }
                        SigEvent::Trace(_) => {}
                        SigEvent::Peer(d @ PeerEvent::Delay { delay, .. }) => {
                            delays.push(delay);
                            log(format!("[{:8.3}] {d}", t.as_secs_f64()));
                        }
                        // We are streaming already: a START from the sink
                        // (accepted by the reader) changes nothing.
                        SigEvent::Peer(p @ PeerEvent::Started) => {
                            log(format!("[{:8.3}] {p}: already streaming", t.as_secs_f64()));
                        }
                        SigEvent::Peer(p) => {
                            log(format!("[{:8.3}] {p}: stopping", t.as_secs_f64()));
                            reason = EndReason::SinkEnded(p);
                            break 'outer;
                        }
                        SigEvent::Dead(e) => {
                            reason = EndReason::SignalingLost(e);
                            break 'outer;
                        }
                    }
                }
            }
            let Ok(p) = rx.recv() else {
                break; // source done (or encoder failed: see join below)
            };
            if k == 0 {
                start = clock.now();
                anchor = start;
            }
            let deadline = anchor + packet_deadline(k - anchor_k, args.fpp, rate);
            let target = deadline.saturating_sub(lead).max(anchor);
            // Burst packets (target clamped to the anchor) go out as soon as
            // the previous WRITE returns; they have no send time to be late for.
            let burst = deadline.saturating_sub(lead) <= anchor;
            clock.sleep_until(target);
            let now = clock.now();
            // How late the packet goes out, for the bitrate controller
            // (burst packets have no send time to be late for).
            let late = if burst {
                Duration::ZERO
            } else {
                now.saturating_sub(target)
            };
            if !burst && now > target + LATE_TOLERANCE {
                stats.late += 1;
                stats.max_late = stats.max_late.max(now - target);
            }
            if now > deadline + LATE_TOLERANCE {
                stats.missed += 1;
                log(format!(
                    "[{:8.3}] packet {k} is {:.1} ms past its playout deadline: re-anchoring the schedule (the next {} packet(s) go out back to back)",
                    (now - start).as_secs_f64(),
                    (now - deadline).as_secs_f64() * 1e3,
                    args.lead
                ));
                anchor = now;
                anchor_k = k;
            }
            pk.build(&p.bytes, &mut pkt);
            // Timed with the pacing clock (the wall clock on a device), so
            // that a test's virtual WRITE time is seen as well.
            let w0 = clock.now();
            let r = media.send(&pkt);
            let after = clock.now();
            let w = after.saturating_sub(w0);
            // Waiting for its send time (or for the producer) since the
            // WRITE before: next to nothing for a packet of a burst.
            let waited = w0.saturating_sub(last_write.unwrap_or(Duration::ZERO));
            last_write = Some(after);
            let change = match &r {
                Ok(()) => abr.on_packet(Obs {
                    now: after,
                    write: w,
                    late,
                    waited,
                }),
                // Only a WRITE that timed out speaks of the link's capacity.
                Err(e) if write_error_is_congestion(e) => abr.on_write_error(after),
                Err(_) => None,
            };
            if let Some(c) = change {
                rung.store(abr.rung(), Ordering::Relaxed);
                log(format!(
                    "[{:8.3}] {c}; {}-byte packets",
                    (after - start).as_secs_f64(),
                    c.to.packet_bytes(args.mode.encoder_mode())
                ));
            }
            match r {
                Ok(()) => {
                    if verbose && stats.packets == 0 {
                        log(format!(
                            "first packet ({} bytes): {} ...",
                            pkt.len(),
                            hex::format(&pkt[..pkt.len().min(20)])
                        ));
                    }
                    consecutive_errors = 0;
                    stats.packets += 1;
                    stats.frames += p.frames as u64;
                    stats.bytes += pkt.len() as u64;
                    stats.write_total += w;
                    stats.write_max = stats.write_max.max(w);
                }
                Err(ChanError::Disconnected) => {
                    reason = EndReason::MediaLost(ChanError::Disconnected.to_string());
                    break;
                }
                Err(e) => {
                    stats.write_errors += 1;
                    consecutive_errors += 1;
                    log(format!(
                        "[{:8.3}] media WRITE failed: {e}",
                        (now - start).as_secs_f64()
                    ));
                    if consecutive_errors >= MAX_CONSECUTIVE_WRITE_ERRORS {
                        reason = EndReason::MediaErrors(e.to_string());
                        break;
                    }
                }
            }
            k += 1;
            let t = clock.now() - start;
            if verbose && t >= next_stats {
                next_stats = t + STATS_EVERY;
                log(format!(
                    "stats: {} rate={}kbps",
                    stats.render(t),
                    abr.rate().kbps()
                ));
            }
        }
        if reason == EndReason::Completed && k > 0 {
            // Drain: let the sink play what it has buffered before CLOSE.
            let end = anchor + packet_deadline(k - anchor_k, args.fpp, rate);
            while clock.now() < end && !stop.load(Ordering::Relaxed) {
                let step = clock.now() + Duration::from_millis(50);
                clock.sleep_until(end.min(step));
            }
        }
        let elapsed = clock.now() - start;
        drop(rx); // unblocks the producer
        let enc = match producer.join() {
            Ok(Ok(st)) => st,
            Ok(Err(e)) => {
                reason = EndReason::Encoder(e);
                EncodeStats::default()
            }
            Err(_) => {
                reason = EndReason::Encoder("encoder thread panicked".into());
                EncodeStats::default()
            }
        };
        (enc, elapsed)
    });
    Outcome {
        reason,
        elapsed,
        stats,
        encode,
        delays,
        rate_changes: abr.changes(),
        rates: abr.describe_times(&Default::default()),
    }
}

/// The signaling reader: answer peer commands until `stop`, then drain what
/// is already queued. Returns when stopped or when the channel fails.
fn signaling_reader<C: SduChannel>(
    sig: &mut Signaling<C>,
    stop: &AtomicBool,
    tx: &mpsc::Sender<SigEvent>,
) {
    let poll = Duration::from_millis(200);
    let mut traced = sig.trace.len();
    loop {
        let stopping = stop.load(Ordering::Relaxed);
        let t0 = Instant::now();
        let r = sig.recv_msg(if stopping {
            Duration::from_millis(20)
        } else {
            poll
        });
        let res = match r {
            Ok(m) if m.message_type == MessageType::Command => sig.answer_peer(&m).map(|ev| {
                // Forwarded below instead; nothing reads `events` while streaming.
                sig.events.clear();
                if let Some(e) = ev {
                    let _ = tx.send(SigEvent::Peer(e));
                }
            }),
            Ok(m) => {
                sig.trace.push(format!(
                    "  ignoring unsolicited response {} label {}",
                    m.signal.name(),
                    m.label
                ));
                Ok(())
            }
            Err(e) => Err(e),
        };
        for l in &sig.trace[traced..] {
            let _ = tx.send(SigEvent::Trace(l.clone()));
        }
        traced = sig.trace.len();
        match res {
            Ok(()) => {}
            Err(ChanError::Timeout) if stopping => return,
            Err(ChanError::Timeout) => {
                // A fake channel returns Timeout at once; don't spin.
                if t0.elapsed() < poll / 4 {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            Err(e) => {
                let _ = tx.send(SigEvent::Dead(e.to_string()));
                return;
            }
        }
    }
}

/// Stream with a signaling reader thread servicing `sig` meanwhile.
#[allow(clippy::too_many_arguments)]
pub fn stream_with_signaling<S: SduChannel + Send, M: SduChannel, K: Clock>(
    sig: &mut Signaling<S>,
    args: &PlayArgs,
    source: Source,
    media: &mut M,
    clock: &mut K,
    stop: &AtomicBool,
    verbose: bool,
    log: &mut dyn FnMut(String),
) -> Outcome {
    /// Stops the reader even if streaming panics (else the scope would wait
    /// for it forever).
    struct StopOnDrop<'a>(&'a AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let (tx, rx) = mpsc::channel();
    let reader_stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let reader = s.spawn(|| signaling_reader(sig, &reader_stop, &tx));
        let guard = StopOnDrop(&reader_stop);
        let mut out = stream_packets(args, source, media, clock, stop, Some(&rx), verbose, log);
        drop(guard);
        let _ = reader.join();
        // Events that arrived after the last packet.
        while let Ok(e) = rx.try_recv() {
            match e {
                SigEvent::Trace(l) if verbose => log(l),
                SigEvent::Trace(_) => {}
                SigEvent::Peer(d @ PeerEvent::Delay { delay, .. }) => {
                    out.delays.push(delay);
                    log(format!("{d}"));
                }
                SigEvent::Peer(p @ PeerEvent::Started) => log(format!("{p}")),
                SigEvent::Peer(p) => {
                    log(format!("{p}"));
                    if !out.reason.is_error() {
                        out.reason = EndReason::SinkEnded(p);
                    }
                }
                SigEvent::Dead(e) => {
                    if !out.reason.is_error() {
                        out.reason = EndReason::SignalingLost(e);
                    }
                }
            }
        }
        out
    })
}

// ---------------------------------------------------------------------------
// Whole flow
// ---------------------------------------------------------------------------

/// A media channel as opened by the caller, with the MTUs the driver reported.
pub struct OpenedMedia<M> {
    pub ch: M,
    pub out_mtu: u16,
    pub in_mtu: u16,
}

pub fn describe_end(reason: &EndReason, elapsed: Duration) -> String {
    let t = elapsed.as_secs_f64();
    match reason {
        EndReason::Completed => format!("finished after {t:.1} s"),
        EndReason::Interrupted => format!("interrupted (Ctrl+C) after {t:.1} s"),
        EndReason::SinkEnded(p) => format!("{p} after {t:.1} s"),
        EndReason::SignalingLost(e) => format!(
            "SIGNALING CHANNEL LOST after {t:.1} s: {e} (the ACL link probably dropped; check the driver trace / Event Viewer for the reason)"
        ),
        EndReason::MediaLost(e) => format!(
            "MEDIA CHANNEL LOST after {t:.1} s: {e} (the sink closed the transport or the ACL link dropped)"
        ),
        EndReason::MediaErrors(e) => {
            format!(
                "stopping after {t:.1} s: {MAX_CONSECUTIVE_WRITE_ERRORS} consecutive media WRITE errors, last: {e}"
            )
        }
        EndReason::Encoder(e) => format!("encoder failed after {t:.1} s: {e}"),
    }
}

/// How far setup got, which decides how an abandoned stream is released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    Nothing,
    /// SET_CONFIGURATION accepted: only ABORT is valid (CLOSE needs OPEN).
    Configured,
    /// OPEN (or START) accepted: CLOSE, falling back to ABORT.
    Open,
}

fn release_held<S: SduChannel>(
    sig: &mut Signaling<S>,
    acp_seid: u8,
    held: Held,
    log: &mut dyn FnMut(String),
) {
    let r = match held {
        Held::Nothing => return,
        Held::Configured => media::abort(sig, acp_seid).map(|()| "ABORT"),
        Held::Open => media::release(sig, acp_seid),
    };
    match r {
        Ok(how) => log(format!("stream released with {how}")),
        Err(e) => log(format!("releasing the stream failed: {e}")),
    }
}

/// Between setup steps: act on the peer commands answered so far (delay
/// reports are printed and kept in `delays`; a CLOSE / ABORT / SUSPEND means
/// the sink does not want this stream) and on Ctrl+C. When stopping, releases
/// what we hold, unless the sink already closed or aborted it.
fn setup_check<S: SduChannel>(
    sig: &mut Signaling<S>,
    stop: &AtomicBool,
    acp_seid: u8,
    held: Held,
    delays: &mut Vec<u16>,
    log: &mut dyn FnMut(String),
) -> Option<Outcome> {
    let mut reason = None;
    for e in std::mem::take(&mut sig.events) {
        log(format!("{e}"));
        match e {
            PeerEvent::Delay { delay, .. } => delays.push(delay),
            // Not a reason to give up: the sink wants the stream to run.
            PeerEvent::Started => {}
            other @ (PeerEvent::Closed | PeerEvent::Aborted | PeerEvent::Suspended) => {
                reason.get_or_insert(EndReason::SinkEnded(other));
            }
        }
    }
    if reason.is_none() && stop.load(Ordering::Relaxed) {
        reason = Some(EndReason::Interrupted);
    }
    let reason = reason?;
    log(describe_end(&reason, Duration::ZERO));
    if reason.may_release() {
        release_held(sig, acp_seid, held, log);
    }
    Some(Outcome::early(reason, std::mem::take(delays)))
}

/// DISCOVER .. START, stream, CLOSE. `open_media` opens the second L2CAP
/// channel (called after OPEN was accepted). `stop` (Ctrl+C) is checked
/// between every setup step; START is never sent once it is set.
#[allow(clippy::too_many_arguments)]
pub fn play<S, M, K>(
    sig: &mut Signaling<S>,
    args: &PlayArgs,
    source: Source,
    requested_in_mtu: u16,
    open_media: impl FnOnce() -> Result<OpenedMedia<M>, String>,
    clock: &mut K,
    stop: &AtomicBool,
    verbose: bool,
    log: &mut dyn FnMut(String),
) -> Result<Outcome, String>
where
    S: SduChannel + Send,
    M: SduChannel,
    K: Clock,
{
    let mut delays = Vec::new();
    // Delay reports answered during setup are kept; see setup_check.
    macro_rules! check {
        ($seid:expr, $held:expr) => {
            if let Some(o) = setup_check(sig, stop, $seid, $held, &mut delays, log) {
                return Ok(o);
            }
        };
    }
    if stop.load(Ordering::Relaxed) {
        return Ok(Outcome::early(EndReason::Interrupted, delays));
    }
    let reports = probe::run(sig).map_err(|e| format!("probe: {e}"))?;
    if verbose {
        log(probe::render(&reports));
    }
    let sep = choose_ssc(&reports)?;
    log(format!(
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
    if sep.in_use {
        log(format!(
            "NOTE: DISCOVER marks SEP {:#04x} in use; trying anyway (expect SEP_IN_USE if another source holds it)",
            sep.seid
        ));
    }
    for n in check_config(sep.flags, args.config, args.mode, args.force_config)? {
        log(format!("WARNING: {n}"));
    }
    let dr = sep.delay_reporting && args.delay_report;
    let config = caps::ssc_configuration(args.config, dr);
    sig.stream = Some(LocalStream {
        int_seid: INT_SEID,
        config: config.clone(),
        caps: local_caps(),
    });
    let choice = Choice {
        acp_seid: sep.seid,
        config,
    };
    check!(sep.seid, Held::Nothing);
    log(format!(
        "SET_CONFIGURATION SEP {:#04x}: SSC config {:#04x}{} ({}, {} kbps, {} frame(s)/packet of {} bytes -> {}-byte packets)",
        sep.seid,
        args.config,
        if dr { " + Delay Reporting" } else { "" },
        args.mode.name(),
        args.bitrate.bps() / 1000,
        args.fpp,
        args.frame_bytes(),
        args.packet_bytes()
    ));
    media::set_configuration(sig, &choice).map_err(|e| e.to_string())?;
    check!(sep.seid, Held::Configured);
    if let Err(e) = media::open(sig, sep.seid) {
        log(format!("{e}"));
        // A sink that aborted or closed meanwhile rejects OPEN (BAD_STATE):
        // report that instead, and do not release what it already dropped.
        check!(sep.seid, Held::Configured);
        release_held(sig, sep.seid, Held::Configured, log);
        return Err(e.to_string());
    }
    log("SET_CONFIGURATION + OPEN accepted; opening the media channel".into());
    check!(sep.seid, Held::Open);
    let mut m = match open_media() {
        Ok(m) => m,
        Err(e) => {
            release_held(sig, sep.seid, Held::Open, log);
            return Err(format!("media channel: {e}"));
        }
    };
    let (limit, why) = match media_mtu_limit(m.out_mtu, m.in_mtu, requested_in_mtu, args.force_mtu)
    {
        Ok(v) => v,
        Err(e) => {
            release_held(sig, sep.seid, Held::Open, log);
            return Err(e);
        }
    };
    log(format!(
        "media channel: out_mtu={} in_mtu={}; packet {} bytes, limit {limit} ({why})",
        m.out_mtu,
        m.in_mtu,
        args.packet_bytes()
    ));
    if args.packet_bytes() > limit {
        release_held(sig, sep.seid, Held::Open, log);
        return Err(format!(
            "{}-byte packets exceed the media MTU limit {limit}; use fewer --frames-per-packet, a lower --bitrate, or --force-mtu N",
            args.packet_bytes()
        ));
    }
    check!(sep.seid, Held::Open);
    let r = sig
        .request(|l| msg::start(l, &[sep.seid]))
        .map_err(|e| format!("START: {e}"));
    if let Err(e) = r
        .and_then(|r| media::expect_accept("START", SignalId::Start, &r).map_err(|e| e.to_string()))
    {
        release_held(sig, sep.seid, Held::Open, log);
        return Err(e);
    }
    check!(sep.seid, Held::Open);
    log(format!(
        "START accepted; streaming {} frames ({:.1} s), packet every {:.3} ms, lead {} packet(s), pacing: {}",
        args.total_frames(&source),
        args.total_frames(&source) as f64 * FRAME_SAMPLES as f64
            / f64::from(args.mode.sample_rate()),
        args.packet_duration().as_secs_f64() * 1e3,
        args.lead,
        clock.describe()
    ));
    let mut out = stream_with_signaling(sig, args, source, &mut m.ch, clock, stop, verbose, log);
    delays.append(&mut out.delays);
    out.delays = delays;
    log(describe_end(&out.reason, out.elapsed));
    log(format!("final: {}", out.stats.render(out.elapsed)));
    if args.adaptive {
        log(format!(
            "bitrate: {} change(s); audio sent at {}",
            out.rate_changes, out.rates
        ));
    }
    log(format!(
        "encoder: {} frames in {:.3} s = {:.1}x realtime",
        out.encode.frames,
        out.encode.encode_time.as_secs_f64(),
        out.encode.realtime_factor(args.mode.sample_rate())
    ));
    if out.reason.may_release() {
        release_held(sig, sep.seid, Held::Open, log);
    }
    drop(m); // closes the media channel (device::Channel closes on drop)
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pace::VirtualClock;
    use crossc_core::fake::{FakeSink, Inbox};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn args(v: &[&str]) -> PlayArgs {
        parse_play_args(&s(v)).unwrap()
    }

    #[test]
    fn defaults_per_mode() {
        let a = args(&[]);
        assert_eq!(
            (
                a.mode,
                a.config,
                a.bitrate,
                a.fpp,
                a.ts,
                a.lead,
                a.delay_report
            ),
            (
                PlayMode::Hifi,
                0x0C,
                Bitrate::K192,
                1,
                TsMode::Packets,
                3,
                true
            )
        );
        assert_eq!(
            a.source,
            SourceSpec::Tone {
                hz: 1000.0,
                dbfs: -20.0
            }
        );
        let u = args(&["--mode", "uhq"]);
        assert_eq!((u.config, u.bitrate, u.fpp), (0x0A, Bitrate::K584, 1));
        assert_eq!(u.frame_bytes(), 660);
        assert_eq!(u.packet_bytes(), 672);
        assert_eq!(args(&["--mode", "ssc48"]).config, 0x18);
        assert_eq!(args(&["--mode", "ssc44"]).config, 0x28);
        let x = args(&[
            "--mode",
            "uhq",
            "--config",
            "0x0E",
            "--bitrate",
            "291",
            "--frames-per-packet",
            "2",
            "--ts",
            "samples",
            "--seconds",
            "1.5",
            "--sweep",
            "--no-delay-report",
            "--lead",
            "5",
            "--force-mtu",
            "700",
        ]);
        assert_eq!(
            (
                x.config,
                x.bitrate,
                x.fpp,
                x.ts,
                x.seconds,
                x.source.clone(),
                x.delay_report,
                x.lead,
                x.force_mtu
            ),
            (
                0x0E,
                Bitrate::K291,
                2,
                TsMode::Samples,
                Some(1.5),
                SourceSpec::Sweep,
                false,
                5,
                Some(700)
            )
        );
        let t = args(&["--tone", "440", "--level", "-6"]);
        assert_eq!(
            t.source,
            SourceSpec::Tone {
                hz: 440.0,
                dbfs: -6.0
            }
        );
        assert_eq!(
            args(&["--wav", "a.wav"]).source,
            SourceSpec::Wav("a.wav".into())
        );
    }

    #[test]
    fn adaptive_args_and_ladders() {
        let kbps = |a: &PlayArgs| -> Vec<(u32, usize)> {
            a.ladder()
                .rungs()
                .iter()
                .map(|r| (r.kbps(), r.frames_per_packet))
                .collect()
        };
        // Not asked for: the one rate, as ever.
        let a = args(&[]);
        assert!(!a.adaptive);
        assert_eq!(kbps(&a), [(192, 1)]);
        assert_eq!(kbps(&args(&["--bitrate", "128"])), [(128, 1)]);
        assert_eq!(args(&["--mode", "ssc48"]).bitrate, Bitrate::K328);
        // Asked for: the mode's ladder from its top rate.
        let a = args(&["--adaptive"]);
        assert!(a.adaptive);
        assert_eq!(kbps(&a), [(192, 1), (128, 1), (88, 1)]);
        let a = args(&["--mode", "uhq", "--adaptive"]);
        assert_eq!(kbps(&a), [(584, 1), (291, 1), (152, 1)]);
        // Plain SSC starts at what the buds decode, not at the fixed 328.
        for m in ["ssc48", "ssc44"] {
            let a = args(&["--mode", m, "--adaptive"]);
            assert_eq!(kbps(&a), [(229, 1), (128, 1), (88, 1)]);
            assert!(a.warnings().is_empty(), "{:?}", a.warnings());
        }
        // The frames per packet are the user's at every rate.
        let a = args(&["--mode", "uhq", "--adaptive", "--frames-per-packet", "2"]);
        assert_eq!(kbps(&a), [(584, 2), (291, 2), (152, 2)]);
        // --bitrate is the fixed rate: not both.
        let e = parse_play_args(&s(&["--adaptive", "--bitrate", "192"])).unwrap_err();
        assert!(e.contains("pick one"), "{e}");
        assert!(PLAY_USAGE.contains("--bitrate KBPS | --adaptive"));
    }

    #[test]
    fn bad_args() {
        let e = |v: &[&str]| parse_play_args(&s(v)).unwrap_err();
        assert!(e(&["--bitrate", "200"]).contains("SSC rates are"));
        assert!(e(&["--mode", "aptx"]).contains("unknown --mode"));
        assert!(e(&["--tone", "1000", "--sweep"]).contains("one source"));
        assert!(e(&["--sweep", "--level", "-3"]).contains("--level"));
        assert!(e(&["--frames-per-packet", "0"]).contains("1..8"));
        assert!(e(&["--config", "0x100"]).contains("one octet"));
        assert!(e(&["--seconds", "-1"]).contains("> 0"));
        assert!(e(&["--ts", "wall"]).contains("packets|samples"));
        assert!(e(&["--bogus"]).contains("unknown argument"));
        assert!(e(&["--level", "3"]).contains("<= 0"));
    }

    #[test]
    fn packet_sizes_per_mode_and_bitrate() {
        // (mode, kbps, fpp) -> frame bytes from ssc_enc::frame_size, packet = 12 + fpp * frame.
        let cases = [
            ("hifi", "192", "1"),
            ("hifi", "192", "2"),
            ("hifi", "229", "1"),
            ("hifi", "328", "1"),
            ("uhq", "584", "1"),
            ("uhq", "291", "2"),
            ("uhq", "152", "2"),
            ("ssc48", "328", "1"),
            ("ssc44", "328", "1"),
        ];
        for (m, k, f) in cases {
            let a = args(&["--mode", m, "--bitrate", k, "--frames-per-packet", f]);
            let fb = ssc_enc::frame_size(a.mode.encoder_mode(), a.bitrate);
            assert_eq!(a.frame_bytes(), fb);
            assert_eq!(a.packet_bytes(), 12 + a.fpp * fb, "{m} {k} {f}");
            assert!(
                a.packet_bytes() <= KNOWN_BUDS_MEDIA_MTU,
                "{m} {k} x{f}: {}",
                a.packet_bytes()
            );
        }
        // The one anchor from the phone capture.
        assert_eq!(args(&["--mode", "uhq"]).packet_bytes(), 672);
        // 584 kbps does not fit twice.
        assert!(
            args(&["--mode", "uhq", "--frames-per-packet", "2"]).packet_bytes()
                > KNOWN_BUDS_MEDIA_MTU
        );
    }

    #[test]
    fn config_vs_caps() {
        let buds = SscFlags(0x3E);
        for (m, c) in [
            (PlayMode::Hifi, 0x0C),
            (PlayMode::Uhq, 0x0A),
            (PlayMode::Uhq, 0x0E),
            (PlayMode::Ssc48, 0x18),
            (PlayMode::Ssc44, 0x28),
        ] {
            assert!(
                check_config(buds, c, m, false).unwrap().is_empty(),
                "{c:#x}"
            );
        }
        let old = SscFlags(0x16);
        let e = check_config(old, 0x0C, PlayMode::Hifi, false).unwrap_err();
        assert!(e.contains("0x08") && e.contains("--force-config"), "{e}");
        assert_eq!(
            check_config(old, 0x0C, PlayMode::Hifi, true).unwrap().len(),
            1
        );
        let e = check_config(SscFlags(0x0B), 0x0A, PlayMode::Uhq, false).unwrap_err();
        assert!(e.contains("UHQ v2 disabled"), "{e}");
        assert!(
            check_config(SscFlags(0x0B), 0x0B, PlayMode::Uhq, false)
                .unwrap_err()
                .contains("0x01")
        );
        // Mode bit missing from a forced config: a warning, not an error.
        let a = args(&["--mode", "uhq", "--config", "0x0C"]);
        assert!(a.warnings().iter().any(|w| w.contains("lacks")));
        assert!(
            args(&["--mode", "uhq", "--bitrate", "192"])
                .warnings()
                .iter()
                .any(|w| w.contains("E-family"))
        );
        assert!(args(&[]).warnings().is_empty());
        assert!(args(&["--mode", "uhq"]).warnings().is_empty());
    }

    #[test]
    fn mtu_limits() {
        // Live values: media out=1021 in=65535 after requesting 1021.
        let (l, why) = media_mtu_limit(1021, 65535, 1021, None).unwrap();
        assert_eq!(l, 1005, "{why}");
        // Follow-up with --in-mtu 900 if out_mtu is really the peer's MTU.
        assert_eq!(media_mtu_limit(1005, 65535, 900, None).unwrap().0, 1005);
        assert_eq!(media_mtu_limit(1021, 900, 900, None).unwrap().0, 900);
        assert_eq!(media_mtu_limit(679, 65535, 1021, None).unwrap().0, 679);
        assert!(media_mtu_limit(0, 65535, 1021, None).is_err());
        assert_eq!(media_mtu_limit(0, 65535, 1021, Some(1100)).unwrap().0, 1100);
    }

    #[test]
    fn wav_at_another_rate_is_resampled() {
        let dir = std::env::temp_dir().join(format!("crossc-play-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t44.wav");
        std::fs::write(
            &p,
            crate::wav::tests::build(1, 2, 44100, 16, None, &[0; 400]),
        )
        .unwrap();
        let ps = p.to_str().unwrap();
        // 100 frames at 44.1 kHz -> ceil(100 * 48000 / 44100) = 109 frames at 48 kHz, as float.
        let src = args(&["--wav", ps]).build_source().unwrap();
        assert_eq!(src.new_buf().name(), "F32");
        match &src {
            Source::Wav { wav, .. } => assert_eq!((wav.sample_rate, wav.frames()), (48000, 109)),
            _ => panic!("not a WAV source"),
        }
        let src = args(&["--mode", "ssc44", "--wav", ps])
            .build_source()
            .unwrap();
        assert_eq!(src.new_buf().name(), "I16");
        let e = args(&["--wav", dir.join("missing.wav").to_str().unwrap()])
            .build_source()
            .err()
            .unwrap();
        assert!(e.contains("missing.wav"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn total_frames_rounding() {
        let a = args(&["--seconds", "1"]);
        let src = a.build_source().unwrap();
        assert_eq!(a.total_frames(&src), 56); // ceil(48000/864) = 56
        let b = args(&["--seconds", "1", "--frames-per-packet", "3"]);
        assert_eq!(b.total_frames(&src), 57);
        assert_eq!(args(&[]).total_frames(&src), 1112); // 20 s
    }

    type Shared<T> = std::sync::Arc<std::sync::Mutex<T>>;

    /// Fake media channel: records packets; after packet n (1-based) pushes
    /// the given signaling packet into the fake sink's inbox (and waits until
    /// the reader thread has taken it); optionally disconnects.
    struct FakeMedia {
        packets: Shared<Vec<Vec<u8>>>,
        inbox: Inbox,
        inject: Vec<(usize, Vec<u8>)>,
        disconnect_after: Option<usize>,
        mtu: usize,
    }

    impl SduChannel for FakeMedia {
        fn send(&mut self, sdu: &[u8]) -> Result<(), ChanError> {
            let mut p = self.packets.lock().unwrap();
            if self.disconnect_after == Some(p.len()) {
                return Err(ChanError::Disconnected);
            }
            assert!(sdu.len() <= self.mtu);
            p.push(sdu.to_vec());
            let n = p.len();
            drop(p);
            for (_, m) in self.inject.iter().filter(|(k, _)| *k == n) {
                self.inbox.lock().unwrap().push_back(m.clone());
                // Let the signaling reader pick it up before the next packet,
                // so the test does not race the virtual clock.
                let t0 = Instant::now();
                while !self.inbox.lock().unwrap().is_empty()
                    && t0.elapsed() < Duration::from_secs(5)
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Ok(())
        }
        fn recv(&mut self, _: Duration) -> Result<Vec<u8>, ChanError> {
            Err(ChanError::Timeout)
        }
        fn out_mtu(&self) -> usize {
            self.mtu
        }
    }

    fn delay_report_after(n: usize) -> (usize, Vec<u8>) {
        (n, msg::delay_report(0xA, INT_SEID, 2345).encode_single())
    }

    type FlowResult = (
        Result<Outcome, String>,
        Signaling<FakeSink>,
        Vec<Vec<u8>>,
        Vec<String>,
    );

    fn run_flow(
        a: &PlayArgs,
        fake: FakeSink,
        inject: Vec<(usize, Vec<u8>)>,
        disconnect_after: Option<usize>,
    ) -> FlowResult {
        run_flow_stop(a, fake, inject, disconnect_after, &AtomicBool::new(false))
    }

    fn run_flow_stop(
        a: &PlayArgs,
        fake: FakeSink,
        inject: Vec<(usize, Vec<u8>)>,
        disconnect_after: Option<usize>,
        stop: &AtomicBool,
    ) -> FlowResult {
        let inbox = fake.inbox.clone();
        let mut sig = Signaling::new(fake);
        let packets: Shared<Vec<Vec<u8>>> = Shared::default();
        let mut logs = Vec::new();
        let src = a.build_source().unwrap();
        let pk = packets.clone();
        let r = play(
            &mut sig,
            a,
            src,
            1021,
            move || {
                Ok(OpenedMedia {
                    ch: FakeMedia {
                        packets: pk,
                        inbox,
                        inject,
                        disconnect_after,
                        mtu: 1005,
                    },
                    out_mtu: 1021,
                    in_mtu: 65535,
                })
            },
            &mut VirtualClock::default(),
            stop,
            true,
            &mut |l| logs.push(l),
        );
        let p = packets.lock().unwrap().clone();
        (r, sig, p, logs)
    }

    fn buds() -> FakeSink {
        let mut f = FakeSink::new(672);
        f.ssc_flags = 0x3E;
        f.delay_after_set_config = Some(1500);
        f
    }

    #[test]
    fn whole_flow_against_fake_sink_hifi() {
        let a = args(&["--seconds", "0.3"]);
        let (r, sig, packets, logs) = run_flow(&a, buds(), vec![delay_report_after(5)], None);
        let out = r.unwrap_or_else(|e| panic!("{e}\n{}", logs.join("\n")));
        assert_eq!(out.reason, EndReason::Completed, "{}", logs.join("\n"));
        let n = a.total_frames(&a.build_source().unwrap()) as usize; // ceil(0.3*48000/864) = 17
        assert_eq!(n, 17);
        assert_eq!(packets.len(), n);
        assert_eq!(out.stats.packets as usize, n);
        assert_eq!(out.stats.frames as usize, n);
        assert_eq!(out.stats.bytes as usize, n * 496);
        assert_eq!(out.stats.write_errors, 0);
        assert_eq!(out.encode.frames as usize, n);
        for (k, p) in packets.iter().enumerate() {
            assert_eq!(p.len(), 12 + 484);
            let (h, _) = RtpHeader::decode(p).unwrap();
            assert_eq!(
                (h.sequence as usize, h.timestamp as usize, h.payload_type),
                (k, k, 96)
            );
            assert_eq!(p[12], 0xFF, "SSC sync byte right after the RTP header");
            assert_eq!(p[15], Bitrate::K192.code());
        }
        // Signaling: SSC SEP configured with config 0x0C + Delay Reporting, opened, started, closed.
        let f = &sig.ch;
        assert_eq!(f.configured, None, "CLOSE accepted -> fake cleared it");
        assert!(!f.opened && !f.started);
        let setc = f
            .commands
            .iter()
            .find_map(|c| match c {
                msg::Command::SetConfiguration {
                    acp_seid,
                    int_seid,
                    config,
                } => Some((*acp_seid, *int_seid, config.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!((setc.0, setc.1), (3, INT_SEID));
        assert_eq!(setc.2, caps::ssc_configuration(0x0C, true));
        let order: Vec<&str> = f
            .commands
            .iter()
            .map(|c| match c {
                msg::Command::Discover => "DISCOVER",
                msg::Command::GetAllCapabilities { .. } => "GET_ALL",
                msg::Command::SetConfiguration { .. } => "SET_CONFIG",
                msg::Command::Open { .. } => "OPEN",
                msg::Command::Start { .. } => "START",
                msg::Command::Close { .. } => "CLOSE",
                _ => "?",
            })
            .collect();
        assert_eq!(
            order,
            [
                "DISCOVER",
                "GET_ALL",
                "GET_ALL",
                "GET_ALL",
                "SET_CONFIG",
                "OPEN",
                "START",
                "CLOSE"
            ]
        );
        // Both DELAYREPORTs (after SET_CONFIGURATION, and mid-stream after packet 5) were accepted.
        assert!(
            f.sent.contains(&vec![0x92, 0x0D]),
            "accept of the label-9 DELAYREPORT"
        );
        assert!(
            f.sent.contains(&vec![0xA2, 0x0D]),
            "accept of the label-A DELAYREPORT"
        );
        // The setup-time report (after SET_CONFIGURATION) is kept too.
        assert_eq!(out.delays, vec![1500, 2345]);
        assert!(sig.events.is_empty(), "events are not accumulated");
        // Drained to the last packet's deadline: 17 x 18 ms.
        assert_eq!(out.elapsed, Duration::from_millis(17 * 18));
        assert_eq!(out.stats.late, 0);
        assert_eq!(out.stats.missed, 0);
        assert!(
            logs.iter()
                .any(|l| l.contains("sink DELAYREPORT: 150.0 ms")),
            "{}",
            logs.join("\n")
        );
        assert!(
            logs.iter()
                .any(|l| l.contains("sink DELAYREPORT: 234.5 ms")),
            "{}",
            logs.join("\n")
        );
        assert!(
            logs.iter()
                .any(|l| l.contains("stream released with CLOSE"))
        );
        assert!(
            logs.iter().any(|l| l.contains("limit 1005")),
            "{}",
            logs.join("\n")
        );
    }

    #[test]
    fn whole_flow_uhq_two_frames_sample_ts_no_delay_report() {
        let a = args(&[
            "--mode",
            "uhq",
            "--bitrate",
            "291",
            "--frames-per-packet",
            "2",
            "--ts",
            "samples",
            "--seconds",
            "0.1",
            "--no-delay-report",
        ]);
        let (r, sig, packets, logs) = run_flow(&a, buds(), vec![delay_report_after(5)], None);
        let out = r.unwrap();
        assert_eq!(out.reason, EndReason::Completed, "{}", logs.join("\n"));
        let fb = ssc_enc::frame_size(Mode::Uhq96k, Bitrate::K291);
        assert_eq!(packets.len(), 6); // ceil(0.1*96000/864)=12 frames -> 6 packets
        for (k, p) in packets.iter().enumerate() {
            assert_eq!(p.len(), 12 + 2 * fb);
            let (h, _) = RtpHeader::decode(p).unwrap();
            assert_eq!(h.timestamp as usize, k * 1728);
            assert_eq!((p[12], p[12 + fb]), (0xFF, 0xFF), "two frames back to back");
        }
        let cfg = sig.ch.commands.iter().find_map(|c| match c {
            msg::Command::SetConfiguration { config, .. } => Some(config.clone()),
            _ => None,
        });
        assert_eq!(cfg.unwrap(), caps::ssc_configuration(0x0A, false));
    }

    #[test]
    fn caps_must_allow_config() {
        let mut f = buds();
        f.ssc_flags = 0x16; // no 0x08
        let (r, sig, packets, _) = run_flow(&args(&["--seconds", "0.1"]), f, vec![], None);
        assert!(r.unwrap_err().contains("0x08"));
        assert!(packets.is_empty());
        assert!(sig.ch.configured.is_none(), "nothing configured");
    }

    #[test]
    fn oversized_packets_are_refused_before_start() {
        let a = args(&[
            "--mode",
            "uhq",
            "--frames-per-packet",
            "2",
            "--seconds",
            "0.1",
        ]);
        let (r, sig, packets, _) = run_flow(&a, buds(), vec![delay_report_after(5)], None);
        let e = r.unwrap_err();
        assert!(
            e.contains("1332-byte packets exceed the media MTU limit 1005"),
            "{e}"
        );
        assert!(packets.is_empty());
        assert!(
            !sig.ch
                .commands
                .iter()
                .any(|c| matches!(c, msg::Command::Start { .. }))
        );
        assert!(
            sig.ch
                .commands
                .iter()
                .any(|c| matches!(c, msg::Command::Close { .. })),
            "released"
        );
    }

    #[test]
    fn media_disconnect_stops_with_elapsed_time() {
        let (r, _sig, packets, logs) =
            run_flow(&args(&["--seconds", "1"]), buds(), vec![], Some(10));
        let out = r.unwrap();
        assert!(
            matches!(out.reason, EndReason::MediaLost(_)),
            "{:?}",
            out.reason
        );
        assert_eq!(packets.len(), 10);
        // 10 packets at 18 ms with a 3-packet lead: the 11th was due at 180 ms - 54 ms.
        assert!(
            logs.iter()
                .any(|l| l.starts_with("MEDIA CHANNEL LOST after 0.1 s")),
            "{}",
            logs.join("\n")
        );
    }

    #[test]
    fn sink_start_mid_stream_changes_nothing() {
        let a = args(&["--seconds", "0.5"]);
        let (plain, _, plain_packets, _) = run_flow(&a, buds(), vec![], None);
        let inject = vec![(6, msg::start(0xB, &[INT_SEID]).encode_single())];
        let (r, sig, packets, logs) = run_flow(&a, buds(), inject, None);
        let out = r.unwrap();
        assert!(
            sig.ch.sent.contains(&vec![0xB2, 0x07]),
            "accepted the sink's START"
        );
        assert_eq!(out.reason, EndReason::Completed, "{}", logs.join("\n"));
        assert_eq!(plain.unwrap().reason, EndReason::Completed);
        assert_eq!(packets, plain_packets, "the stream went on untouched");
        assert!(
            logs.iter().any(|l| l.contains("sink sent START")),
            "{}",
            logs.join("\n")
        );
    }

    #[test]
    fn sink_close_mid_stream_stops_without_our_close() {
        let inject = vec![
            delay_report_after(3),
            (6, msg::close(0xB, INT_SEID).encode_single()),
        ];
        let (r, sig, packets, logs) = run_flow(&args(&["--seconds", "0.5"]), buds(), inject, None);
        let out = r.unwrap();
        assert!(
            sig.ch.sent.contains(&vec![0xB2, 0x08]),
            "accepted the sink's CLOSE"
        );
        assert_eq!(
            out.reason,
            EndReason::SinkEnded(PeerEvent::Closed),
            "{}",
            logs.join(
                "
"
            )
        );
        assert_eq!(packets.len(), 6, "stopped right after the CLOSE");
        assert_eq!(out.delays, vec![1500, 2345]);
        let ours = sig
            .ch
            .commands
            .iter()
            .filter(|c| matches!(c, msg::Command::Close { .. }))
            .count();
        assert_eq!(
            ours, 0,
            "the sink closed the stream; we must not CLOSE it again"
        );
        assert!(
            logs.iter().any(|l| l.starts_with("sink sent CLOSE after")),
            "{}",
            logs.join(
                "
"
            )
        );
    }

    #[test]
    fn wall_clock_pacing_is_real_time_and_on_schedule() {
        struct Null(usize);
        impl SduChannel for Null {
            fn send(&mut self, _: &[u8]) -> Result<(), ChanError> {
                self.0 += 1;
                Ok(())
            }
            fn recv(&mut self, _: Duration) -> Result<Vec<u8>, ChanError> {
                Err(ChanError::Timeout)
            }
            fn out_mtu(&self) -> usize {
                1005
            }
        }
        // UHQ 152k x2: cheap to encode even in debug; 18 ms packets.
        let a = args(&[
            "--mode",
            "uhq",
            "--bitrate",
            "152",
            "--frames-per-packet",
            "2",
            "--seconds",
            "0.18",
            "--lead",
            "2",
        ]);
        let n = (a.total_frames(&a.build_source().unwrap()) / 2) as u32;
        assert_eq!(n, 10);
        let mut sink = Null(0);
        let mut clock = crate::pace::WallClock::new();
        let out = stream_packets(
            &a,
            a.build_source().unwrap(),
            &mut sink,
            &mut clock,
            &AtomicBool::new(false),
            None,
            false,
            &mut |_| {},
        );
        assert_eq!(out.reason, EndReason::Completed);
        assert_eq!(sink.0, 10);
        // `late` is now against the send time (0.5 ms tolerance): under
        // parallel tests a preempted spin may show up there, so only the
        // playout deadline is held strictly.
        assert_eq!(out.stats.missed, 0, "worst late {:?}", out.stats.max_late);
        assert!(out.stats.late <= 2, "{:?}", out.stats);
        // Drained: returns at the last packet's deadline, n periods in.
        let min = a.packet_duration() * n;
        assert!(out.elapsed >= min, "{:?} < {min:?}", out.elapsed);
        assert!(
            out.elapsed < min + Duration::from_millis(50),
            "{:?}",
            out.elapsed
        );
    }

    #[test]
    fn sink_abort_during_setup_stops_before_streaming() {
        let mut fake = buds();
        fake.delay_after_set_config = None;
        fake.abort_after_set_config = true;
        let (r, sig, packets, logs) = run_flow(&args(&["--seconds", "0.2"]), fake, vec![], None);
        let out = r.unwrap();
        assert_eq!(
            out.reason,
            EndReason::SinkEnded(PeerEvent::Aborted),
            "{}",
            logs.join("\n")
        );
        assert!(packets.is_empty());
        assert!(
            logs.iter().any(|l| l.contains("OPEN rejected: BAD_STATE")),
            "the fake now rejects OPEN after its own ABORT, like real buds: {}",
            logs.join("\n")
        );
        assert!(
            !logs.iter().any(|l| l.starts_with("media channel:")),
            "media channel never opened"
        );
        assert!(
            !sig.ch.commands.iter().any(|c| matches!(
                c,
                msg::Command::Close { .. }
                    | msg::Command::Abort { .. }
                    | msg::Command::Start { .. }
            )),
            "the sink dropped the stream: no CLOSE/ABORT/START from us"
        );
    }

    #[test]
    fn dry_run_streams_without_signaling() {
        struct Null(usize);
        impl SduChannel for Null {
            fn send(&mut self, _: &[u8]) -> Result<(), ChanError> {
                self.0 += 1;
                Ok(())
            }
            fn recv(&mut self, _: Duration) -> Result<Vec<u8>, ChanError> {
                Err(ChanError::Timeout)
            }
            fn out_mtu(&self) -> usize {
                1005
            }
        }
        let a = args(&["--mode", "ssc44", "--sweep", "--seconds", "0.2"]);
        let mut n = Null(0);
        let out = stream_packets(
            &a,
            a.build_source().unwrap(),
            &mut n,
            &mut VirtualClock::default(),
            &AtomicBool::new(false),
            None,
            false,
            &mut |_| {},
        );
        assert_eq!(out.reason, EndReason::Completed);
        assert_eq!(n.0, 11); // ceil(0.2*44100/864)
        assert!(out.encode.realtime_factor(44100) > 0.0);
        // Ctrl+C before the first packet.
        let out = stream_packets(
            &a,
            a.build_source().unwrap(),
            &mut n,
            &mut VirtualClock::default(),
            &AtomicBool::new(true),
            None,
            false,
            &mut |_| {},
        );
        assert_eq!(out.reason, EndReason::Interrupted);
    }

    fn sent_by_us(sig: &Signaling<FakeSink>) -> Vec<&'static str> {
        sig.ch
            .commands
            .iter()
            .filter_map(|c| match c {
                msg::Command::SetConfiguration { .. } => Some("SET_CONFIG"),
                msg::Command::Open { .. } => Some("OPEN"),
                msg::Command::Start { .. } => Some("START"),
                msg::Command::Close { .. } => Some("CLOSE"),
                msg::Command::Abort { .. } => Some("ABORT"),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn open_rejected_after_set_configuration_sends_abort() {
        let mut fake = buds();
        fake.reject_open = Some(0x12); // BAD_ACP_SEID
        let (r, sig, packets, logs) = run_flow(&args(&["--seconds", "0.1"]), fake, vec![], None);
        let e = r.unwrap_err();
        assert!(e.contains("OPEN rejected"), "{e}");
        assert!(packets.is_empty());
        // CLOSE is invalid in CONFIGURED; ABORT releases the SEP.
        assert_eq!(sent_by_us(&sig), ["SET_CONFIG", "OPEN", "ABORT"]);
        assert_eq!(sig.ch.configured, None, "SEP released");
        assert!(
            logs.iter().any(|l| l == "stream released with ABORT"),
            "{}",
            logs.join("\n")
        );
    }

    #[test]
    fn ctrl_c_during_setup_never_starts_and_releases_by_state() {
        use std::sync::Arc;
        // Pressed before anything: nothing is sent at all.
        let (r, sig, _, _) = run_flow_stop(
            &args(&["--seconds", "0.1"]),
            buds(),
            vec![],
            None,
            &AtomicBool::new(true),
        );
        assert_eq!(r.unwrap().reason, EndReason::Interrupted);
        assert!(sig.ch.commands.is_empty());
        // Pressed while SET_CONFIGURATION is in flight: ABORT (CONFIGURED).
        // Pressed while OPEN is in flight: CLOSE (OPEN), no media channel.
        for (when, release, delays) in [
            (
                SignalId::SetConfiguration,
                vec!["SET_CONFIG", "ABORT"],
                vec![],
            ),
            (
                SignalId::Open,
                vec!["SET_CONFIG", "OPEN", "CLOSE"],
                vec![1500],
            ),
        ] {
            let flag = Arc::new(AtomicBool::new(false));
            let mut fake = buds();
            fake.stop_on = Some((when, flag.clone()));
            let (r, sig, packets, logs) =
                run_flow_stop(&args(&["--seconds", "0.1"]), fake, vec![], None, &flag);
            let out = r.unwrap();
            assert_eq!(out.reason, EndReason::Interrupted, "{}", logs.join("\n"));
            assert_eq!(sent_by_us(&sig), release, "{when:?}");
            assert!(packets.is_empty());
            assert!(!logs.iter().any(|l| l.starts_with("media channel:")));
            // The fake queues its DELAYREPORT behind the SET_CONFIGURATION accept,
            // so it is only read (and kept) during OPEN.
            assert_eq!(out.delays, delays, "setup delay report kept");
            assert_eq!(sig.ch.configured, None);
        }
    }

    /// Virtual time shared between the pacing clock and a fake media channel
    /// (both are borrowed mutably at once), so a WRITE can "take" time.
    #[derive(Clone, Default)]
    struct SharedClock(std::sync::Arc<std::sync::Mutex<Duration>>);

    impl Clock for SharedClock {
        fn now(&self) -> Duration {
            *self.0.lock().unwrap()
        }
        fn sleep_until(&mut self, t: Duration) {
            let mut n = self.0.lock().unwrap();
            *n = (*n).max(t);
        }
        fn describe(&self) -> &'static str {
            "shared virtual"
        }
    }

    /// Records (send time, RTP sequence); the `stall_after`-th WRITE takes `stall`.
    struct StallMedia {
        clock: SharedClock,
        stall_after: usize,
        stall: Duration,
        sent: Vec<(Duration, u16)>,
    }

    impl SduChannel for StallMedia {
        fn send(&mut self, sdu: &[u8]) -> Result<(), ChanError> {
            let seq = RtpHeader::decode(sdu).unwrap().0.sequence;
            self.sent.push((self.clock.now(), seq));
            if self.sent.len() == self.stall_after {
                *self.clock.0.lock().unwrap() += self.stall;
            }
            Ok(())
        }
        fn recv(&mut self, _: Duration) -> Result<Vec<u8>, ChanError> {
            Err(ChanError::Timeout)
        }
        fn out_mtu(&self) -> usize {
            1005
        }
    }

    fn stalled_run(stall: Duration) -> (Outcome, Vec<(Duration, u16)>, Vec<String>) {
        // HiFi: 18 ms packets, lead 3, 56 packets; the 10th WRITE (k=9) stalls.
        let a = args(&["--seconds", "1", "--lead", "3"]);
        let clock = SharedClock::default();
        let mut m = StallMedia {
            clock: clock.clone(),
            stall_after: 10,
            stall,
            sent: Vec::new(),
        };
        let mut logs = Vec::new();
        let out = stream_packets(
            &a,
            a.build_source().unwrap(),
            &mut m,
            &mut clock.clone(),
            &AtomicBool::new(false),
            None,
            false,
            &mut |l| logs.push(l),
        );
        assert_eq!(out.reason, EndReason::Completed);
        (out, m.sent, logs)
    }

    #[test]
    fn stall_re_anchors_instead_of_bursting_the_backlog() {
        let (out, sent, logs) = stalled_run(Duration::from_secs(2));
        assert_eq!(sent.len(), 56);
        let seqs: Vec<u16> = sent.iter().map(|s| s.1).collect();
        assert_eq!(seqs, (0..56).collect::<Vec<u16>>(), "nothing dropped");
        // Largest number of packets sent at one instant: lead + 1, as at the
        // stream start (without the re-anchor it would be ~111).
        let mut max_burst = 0;
        let mut i = 0;
        while i < sent.len() {
            let n = sent[i..].iter().take_while(|s| s.0 == sent[i].0).count();
            max_burst = max_burst.max(n);
            i += n;
        }
        assert_eq!(max_burst, 4);
        let t_stall = Duration::from_millis(9 * 18 - 54) + Duration::from_secs(2);
        assert_eq!(
            &sent[10..14],
            &[(t_stall, 10), (t_stall, 11), (t_stall, 12), (t_stall, 13)]
        );
        assert_eq!(sent[14].0, t_stall + Duration::from_millis(18));
        assert_eq!((out.stats.missed, out.stats.late), (1, 1));
        assert!(logs.iter().any(|l| l.contains("re-anchoring")), "{logs:?}");
        // Drain to the re-anchored end: 46 more packets after k=10.
        assert_eq!(out.elapsed, t_stall + Duration::from_millis(46 * 18));
    }

    #[test]
    fn stall_within_the_lead_counts_late_but_not_missed() {
        // 30 ms > one 18 ms period but < the 54 ms lead.
        let (out, sent, logs) = stalled_run(Duration::from_millis(30));
        assert_eq!(
            (out.stats.late, out.stats.missed),
            (1, 0),
            "{:?}",
            out.stats
        );
        assert_eq!(out.stats.max_late, Duration::from_millis(12));
        assert!(!logs.iter().any(|l| l.contains("re-anchoring")));
        // Back on the original schedule right after.
        assert_eq!(sent[11].0, Duration::from_millis(11 * 18 - 54));
        assert_eq!(out.elapsed, Duration::from_millis(56 * 18));
    }

    /// What `SlowMedia` records of a packet: time, sequence, length, rate code.
    type Sent = (Duration, u16, usize, u8);

    /// A link that is slow for a while: WRITEs number `slow.0 .. slow.1`
    /// take `took` of the shared virtual time. Records (time, sequence,
    /// length, rate code) of every packet.
    struct SlowMedia {
        clock: SharedClock,
        slow: (usize, usize),
        took: Duration,
        fail_at: Option<usize>,
        sent: Vec<Sent>,
    }

    impl SduChannel for SlowMedia {
        fn send(&mut self, sdu: &[u8]) -> Result<(), ChanError> {
            let n = self.sent.len();
            if self.fail_at == Some(n) {
                self.fail_at = None;
                return Err(ChanError::Os {
                    code: 121,
                    what: "IOCTL",
                });
            }
            let seq = RtpHeader::decode(sdu).unwrap().0.sequence;
            assert_eq!(sdu[12], 0xFF);
            self.sent.push((self.clock.now(), seq, sdu.len(), sdu[15]));
            let d = if (self.slow.0..self.slow.1).contains(&n) {
                self.took
            } else {
                Duration::from_micros(200)
            };
            *self.clock.0.lock().unwrap() += d;
            Ok(())
        }
        fn recv(&mut self, _: Duration) -> Result<Vec<u8>, ChanError> {
            Err(ChanError::Timeout)
        }
        fn out_mtu(&self) -> usize {
            1005
        }
    }

    fn slow_run(
        argv: &[&str],
        slow: (usize, usize),
        fail_at: Option<usize>,
    ) -> (Outcome, Vec<Sent>, Vec<String>) {
        let a = args(argv);
        let clock = SharedClock::default();
        let mut m = SlowMedia {
            clock: clock.clone(),
            slow,
            took: Duration::from_millis(20),
            fail_at,
            sent: Vec::new(),
        };
        let mut logs = Vec::new();
        let out = stream_packets(
            &a,
            a.build_source().unwrap(),
            &mut m,
            &mut clock.clone(),
            &AtomicBool::new(false),
            None,
            false,
            &mut |l| logs.push(l),
        );
        assert_eq!(out.reason, EndReason::Completed);
        (out, m.sent, logs)
    }

    /// The packet lengths in the order they first appear.
    fn length_runs(sent: &[Sent]) -> Vec<usize> {
        let mut runs: Vec<usize> = Vec::new();
        for s in sent {
            if runs.last() != Some(&s.2) {
                runs.push(s.2);
            }
        }
        runs
    }

    #[test]
    fn adaptive_follows_write_latency_and_bitrate_stays_fixed() {
        // HiFi, 25 s; WRITEs 100..150 take 20 ms each (a packet plays 18).
        let (out, sent, logs) = slow_run(&["--seconds", "25", "--adaptive"], (100, 150), None);
        let changes: Vec<&String> = logs.iter().filter(|l| l.contains("bitrate ")).collect();
        assert_eq!(changes.len(), 4, "{logs:?}");
        assert!(changes[0].contains("bitrate 192 -> 128 kbps: link congested"));
        assert!(changes[0].ends_with("; 336-byte packets"), "{}", changes[0]);
        assert!(changes[1].contains("bitrate 128 -> 88 kbps: link congested"));
        assert!(changes[2].contains("bitrate 88 -> 128 kbps: link clean for 10 s"));
        assert!(changes[3].contains("bitrate 128 -> 192 kbps: link clean for 10 s"));
        // One RTP sequence, one frame per packet at every rate, nothing lost.
        let seqs: Vec<u16> = sent.iter().map(|s| s.1).collect();
        assert_eq!(seqs, (0..sent.len() as u16).collect::<Vec<_>>());
        assert_eq!(sent.len() as u64, out.stats.packets);
        assert_eq!(length_runs(&sent), [496, 336, 236, 336, 496]);
        for s in &sent {
            let code = match s.2 {
                496 => Bitrate::K192,
                336 => Bitrate::K128,
                _ => Bitrate::K88,
            }
            .code();
            assert_eq!(s.3, code, "length {} carries its own rate code", s.2);
        }
        // The first lower-rate packet is on the wire within the packets
        // that were already encoded: the queue, the one in hand, the one
        // being encoded.
        let first_low = sent.iter().position(|s| s.2 == 336).unwrap();
        assert!((106..=112).contains(&first_low), "{first_low}");
        assert_eq!(out.rate_changes, 4);
        assert!(
            out.rates.starts_with("192k=") && out.rates.contains(" 88k=1"),
            "{}",
            out.rates
        );
        // The same link with a fixed rate: nothing changes.
        for argv in [
            &["--seconds", "25"][..],
            &["--seconds", "25", "--bitrate", "192"][..],
        ] {
            let (out, sent, logs) = slow_run(argv, (100, 150), None);
            assert!(!logs.iter().any(|l| l.contains("bitrate ")), "{logs:?}");
            assert_eq!(length_runs(&sent), [496]);
            assert_eq!(out.rate_changes, 0);
            assert!(out.stats.write_max >= Duration::from_millis(20));
        }
    }

    #[test]
    fn adaptive_goes_to_the_lowest_rate_after_a_failed_write() {
        let (out, sent, logs) = slow_run(&["--seconds", "5", "--adaptive"], (0, 0), Some(50));
        assert_eq!(out.stats.write_errors, 1);
        assert!(
            logs.iter()
                .any(|l| l.contains("bitrate 192 -> 88 kbps: a WRITE timed out")),
            "{logs:?}"
        );
        assert_eq!(length_runs(&sent), [496, 236]);
        // UHQ: the ladder keeps one frame per packet here.
        let (_, sent, _) = slow_run(
            &["--mode", "uhq", "--seconds", "5", "--adaptive"],
            (100, 200),
            None,
        );
        assert_eq!(length_runs(&sent), [672, 342, 186]);
    }

    #[test]
    fn panic_while_streaming_does_not_hang() {
        struct Null;
        impl SduChannel for Null {
            fn send(&mut self, _: &[u8]) -> Result<(), ChanError> {
                Ok(())
            }
            fn recv(&mut self, _: Duration) -> Result<Vec<u8>, ChanError> {
                Err(ChanError::Timeout)
            }
            fn out_mtu(&self) -> usize {
                1005
            }
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut sig = Signaling::new(FakeSink::new(672));
                // 56 packets: more than the 16-deep encoder queue, so the
                // producer is blocked when the panic hits.
                let a = args(&["--seconds", "1"]);
                stream_with_signaling(
                    &mut sig,
                    &a,
                    a.build_source().unwrap(),
                    &mut Null,
                    &mut VirtualClock::default(),
                    &AtomicBool::new(false),
                    true,
                    &mut |l| {
                        if l.starts_with("first packet") {
                            std::thread::sleep(Duration::from_millis(100));
                            panic!("stdout is gone");
                        }
                    },
                )
            }));
            let _ = tx.send(r.is_err());
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(20)),
            Ok(true),
            "the panic must propagate instead of hanging on the reader/producer"
        );
    }

    #[test]
    fn empty_wav_is_refused_before_signaling() {
        let dir = std::env::temp_dir().join(format!("crossc-play-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("empty.wav");
        std::fs::write(&p, crate::wav::tests::build(1, 2, 48000, 16, None, &[])).unwrap();
        let e = args(&["--wav", p.to_str().unwrap()])
            .build_source()
            .err()
            .unwrap();
        assert!(e.contains("no audio"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hifi_above_229_warns() {
        let w = |k: &str| args(&["--bitrate", k]).warnings();
        assert!(w("192").is_empty());
        assert!(w("229").is_empty());
        let w328 = w("328");
        assert!(
            w328.iter().any(|x| x.contains("above 229 kbps")),
            "{w328:?}"
        );
        assert!(
            args(&["--mode", "ssc48", "--bitrate", "328"])
                .warnings()
                .is_empty()
        );
    }
}
