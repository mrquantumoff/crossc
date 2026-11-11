//! What the tray shows for a given state: which icon, which tooltip, which
//! cards and rows, which text for the clipboard. All pure: a state goes
//! in, text comes out. The windows only draw what this returns.

use crossc_status::{
    CodecMode, Counters, DeviceStatus, Event, EventLevel, LinkPhase, LinkStatus, PcmFormat,
    Snapshot, VolumeMode, VolumeStatus,
};

/// Why there is no status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outage {
    /// The service is not running (or has no status pipe).
    NotRunning,
    /// The service serves as many status clients as it will.
    Busy,
    /// This user may not read the status.
    Denied,
    /// The service speaks another version of the status protocol.
    Version { theirs: u32 },
    /// The service sent something that is no status.
    Unreadable,
    /// The status pipe is another program's, not the service's.
    Untrusted,
    /// The connection ended or went silent; it is being set up again.
    Lost,
}

/// Where the tray's knowledge comes from at the moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Feed {
    /// Nothing heard yet.
    Connecting,
    Live(Box<Snapshot>),
    Down(Outage),
}

/// How good the stream is: the number of bars in the icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Quality {
    /// Plain SSC, 16-bit (or a mode this version does not know).
    Ssc = 1,
    /// Seamless HiFi, 48 kHz / 24-bit.
    Hifi = 2,
    /// UHQ, 96 kHz / 24-bit.
    Uhq = 3,
}

impl Quality {
    pub fn bars(self) -> usize {
        self as usize
    }
}

/// The icon in the notification area.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Glyph {
    /// No service.
    Off,
    /// Something is wrong that waiting does not fix (protocol version,
    /// access, unreadable status, a status pipe that is not the service's).
    Problem,
    /// The service runs; no stream is open (no buds, driver missing,
    /// reconnecting).
    Waiting,
    /// A stream is open, nothing plays.
    Ready(Quality),
    /// Audio is being sent.
    Streaming(Quality),
}

/// How a piece of text is to be coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Muted,
    Good,
    Warn,
    Bad,
}

fn quality(l: &LinkStatus) -> Quality {
    match l.mode {
        CodecMode::Uhq => Quality::Uhq,
        CodecMode::Hifi => Quality::Hifi,
        CodecMode::Ssc | CodecMode::Unknown => Quality::Ssc,
    }
}

/// The glyph of one device.
fn device_glyph(d: &DeviceStatus) -> Glyph {
    match (d.phase, &d.link) {
        (LinkPhase::Streaming, Some(l)) => Glyph::Streaming(quality(l)),
        (LinkPhase::Ready, Some(l)) => Glyph::Ready(quality(l)),
        _ => Glyph::Waiting,
    }
}

/// How much a glyph says: the device with the most to say gives the icon.
fn rank(g: Glyph) -> (u8, u8) {
    match g {
        Glyph::Streaming(q) => (3, q as u8),
        Glyph::Ready(q) => (2, q as u8),
        Glyph::Waiting => (1, 0),
        Glyph::Problem | Glyph::Off => (0, 0),
    }
}

/// The icon for a state.
pub fn glyph(feed: &Feed) -> Glyph {
    match feed {
        Feed::Connecting | Feed::Down(Outage::NotRunning) => Glyph::Off,
        Feed::Down(Outage::Lost | Outage::Busy) => Glyph::Waiting,
        Feed::Down(
            Outage::Denied | Outage::Version { .. } | Outage::Unreadable | Outage::Untrusted,
        ) => Glyph::Problem,
        Feed::Live(s) => s
            .devices
            .iter()
            .map(device_glyph)
            .max_by_key(|g| rank(*g))
            .unwrap_or(Glyph::Waiting),
    }
}

// ---------------------------------------------------------------------------
// small formatters
// ---------------------------------------------------------------------------

/// 96000 -> "96 kHz", 44100 -> "44.1 kHz".
pub fn rate(hz: u32) -> String {
    if hz.is_multiple_of(1000) {
        format!("{} kHz", hz / 1000)
    } else {
        format!("{}.{} kHz", hz / 1000, (hz % 1000) / 100)
    }
}

/// 1234567 -> "1,234,567".
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A length of time, two units at most: "12 s", "3 min 05 s", "2 h 07 min",
/// "4 d 3 h".
pub fn span(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 {
        format!("{d} d {h} h")
    } else if h > 0 {
        format!("{h} h {m:02} min")
    } else if m > 0 {
        format!("{m} min {s:02} s")
    } else {
        format!("{s} s")
    }
}

/// Microseconds as "0.31 ms" / "4.2 ms" / "120 ms".
pub fn millis(us: u32) -> String {
    let ms = f64::from(us) / 1000.0;
    if ms < 1.0 {
        format!("{ms:.2} ms")
    } else if ms < 100.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{ms:.0} ms")
    }
}

/// The codec mode in a word.
pub fn mode_name(m: CodecMode) -> &'static str {
    match m {
        CodecMode::Uhq => "UHQ",
        CodecMode::Hifi => "HiFi",
        CodecMode::Ssc => "SSC",
        CodecMode::Unknown => "unknown mode",
    }
}

/// The codec mode as the buds' settings call it.
pub fn mode_long(m: CodecMode) -> &'static str {
    match m {
        CodecMode::Uhq => "UHQ (Ultra High Quality)",
        CodecMode::Hifi => "Seamless HiFi",
        CodecMode::Ssc => "SSC (Samsung Seamless Codec)",
        CodecMode::Unknown => "a mode this tray app does not know",
    }
}

/// "Galaxy Buds4 Pro" -> "Buds4 Pro": what fits a tooltip.
pub fn short_name(d: &DeviceStatus) -> &str {
    let name = d.display_name();
    name.strip_prefix("Galaxy ")
        .filter(|rest| !rest.is_empty())
        .unwrap_or(name)
}

/// "UHQ 96 kHz/24 · 584 kbps · MTU 1005": mode, sample rate and depth,
/// bitrate, media MTU limit.
pub fn stream_line(l: &LinkStatus) -> String {
    let line = format!(
        "{} {}/{} · {} kbps",
        mode_name(l.mode),
        rate(l.stream_rate),
        l.stream_bits,
        l.bitrate_kbps
    );
    match l.mtu_limit {
        0 => line, // a service that does not say
        mtu => format!("{line} · MTU {mtu}"),
    }
}

/// The link state in a few words.
pub fn phase_text(d: &DeviceStatus) -> &'static str {
    match d.phase {
        LinkPhase::WaitingDriver => "waiting for the driver",
        LinkPhase::WaitingBuds => "buds not connected",
        LinkPhase::Ready => "ready",
        LinkPhase::Streaming => "streaming",
        LinkPhase::Reconnecting => "reconnecting",
        LinkPhase::Stopped => "stopped",
        LinkPhase::Unknown => "state unknown",
    }
}

fn phase_tone(p: LinkPhase) -> Tone {
    match p {
        LinkPhase::Streaming => Tone::Good,
        LinkPhase::Ready => Tone::Normal,
        LinkPhase::Reconnecting | LinkPhase::WaitingDriver => Tone::Warn,
        LinkPhase::WaitingBuds | LinkPhase::Stopped | LinkPhase::Unknown => Tone::Muted,
    }
}

// ---------------------------------------------------------------------------
// tooltip
// ---------------------------------------------------------------------------

/// The longest tooltip the shell takes, in UTF-16 units (the buffer has
/// 128 with the terminator).
pub const TOOLTIP_MAX: usize = 127;

/// One device in one line: "Buds4 Pro — UHQ 96 kHz/24 · 584 kbps · MTU 1005".
pub fn device_line(d: &DeviceStatus) -> String {
    let name = short_name(d);
    match (d.phase, &d.link) {
        (LinkPhase::Streaming, Some(l)) => format!("{name} — {}", stream_line(l)),
        (LinkPhase::Ready, Some(l)) => format!("{name} — {}, idle", stream_line(l)),
        _ => format!("{name} — {}", phase_text(d)),
    }
}

/// What to say when there are no cards to show: a title and a sentence.
pub fn headline(feed: &Feed) -> Option<(Tone, &'static str, String)> {
    let (tone, title, text) = match feed {
        Feed::Connecting => (
            Tone::Muted,
            "Connecting",
            "Asking the crossc service for its status.".to_string(),
        ),
        Feed::Down(Outage::NotRunning) => (
            Tone::Bad,
            "Service not running",
            "The crossc service is not running (or is an older version that does not report \
             its status). Open the crossc installer and choose Install: that sets it up and \
             starts it."
                .to_string(),
        ),
        Feed::Down(Outage::Busy) => (
            Tone::Warn,
            "Service busy",
            "The crossc service is serving as many status windows as it will. Trying again."
                .to_string(),
        ),
        Feed::Down(Outage::Denied) => (
            Tone::Bad,
            "Access denied",
            "This user account may not read the crossc service's status.".to_string(),
        ),
        Feed::Down(Outage::Version { theirs }) => (
            Tone::Warn,
            "Version mismatch",
            format!(
                "The crossc service and this tray app are of different releases and do not \
                 understand each other (the service reports status version {theirs}, this \
                 tray app reads version {}). Use the crossc-tray.exe that came with the \
                 service.",
                crossc_status::PROTOCOL_VERSION
            ),
        ),
        Feed::Down(Outage::Unreadable) => (
            Tone::Warn,
            "Status unreadable",
            "What the crossc service sent is not a status this tray app understands. \
             Use the crossc-tray.exe that came with the service."
                .to_string(),
        ),
        Feed::Down(Outage::Untrusted) => (
            Tone::Bad,
            "Status not trusted",
            "Another program, not the crossc service, is answering where the service reports \
             its status, so nothing it says is shown. The service itself is probably not \
             running: open the crossc installer and choose Install."
                .to_string(),
        ),
        Feed::Down(Outage::Lost) => (
            Tone::Warn,
            "Reconnecting",
            "The connection to the crossc service ended. Connecting again.".to_string(),
        ),
        Feed::Live(s) if s.devices.is_empty() => (
            Tone::Muted,
            "No buds",
            "The crossc service is running, but no buds with the crossc driver are present. \
             Install the driver on your paired Galaxy Buds with the crossc installer."
                .to_string(),
        ),
        Feed::Live(_) => return None,
    };
    Some((tone, title, text))
}

/// Cut `s` to at most `max` UTF-16 units, at a character boundary, with an
/// ellipsis when something was cut.
pub fn fit_utf16(s: &str, max: usize) -> String {
    if s.encode_utf16().count() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        // One unit is kept for the ellipsis.
        if used + c.len_utf16() + 1 > max {
            break;
        }
        used += c.len_utf16();
        out.push(c);
    }
    if max > 0 {
        out.push('…');
    }
    out
}

/// The icon's tooltip: one line per device, or what is wrong.
pub fn tooltip(feed: &Feed) -> String {
    let text = match (feed, headline(feed)) {
        (Feed::Live(s), None) => s
            .devices
            .iter()
            .map(device_line)
            .collect::<Vec<_>>()
            .join("\n"),
        (_, Some((_, title, _))) => format!("crossc — {}", lower_first(title)),
        (_, None) => "crossc".to_string(),
    };
    fit_utf16(&text, TOOLTIP_MAX)
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_lowercase().chain(c).collect(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// cards
// ---------------------------------------------------------------------------

/// One line of a card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub label: &'static str,
    pub value: String,
    pub tone: Tone,
    /// A second, smaller line under the value.
    pub detail: Option<String>,
}

fn row(label: &'static str, value: impl Into<String>) -> Row {
    Row {
        label,
        value: value.into(),
        tone: Tone::Normal,
        detail: None,
    }
}

impl Row {
    fn tone(mut self, tone: Tone) -> Row {
        self.tone = tone;
        self
    }
    fn detail(mut self, detail: impl Into<String>) -> Row {
        let d: String = detail.into();
        self.detail = (!d.is_empty()).then_some(d);
        self
    }
}

/// One of the large numbers at the top of a card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stat {
    pub label: &'static str,
    pub value: String,
    pub tone: Tone,
}

/// A titled group of rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub title: &'static str,
    pub rows: Vec<Row>,
}

/// One device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    pub id: String,
    pub title: String,
    /// The state, next to the title.
    pub badge: String,
    pub badge_tone: Tone,
    /// The line under the title: the stream's mode, or what is being
    /// waited for.
    pub summary: String,
    /// The numbers that matter most, shown large at the top: mode, sample
    /// rate, bitrate, media MTU (bytes). Empty without a stream.
    pub stats: Vec<Stat>,
    pub sections: Vec<Section>,
    /// The last warning or error, if any.
    pub note: Option<(Tone, String)>,
}

fn format_row(f: &PcmFormat) -> String {
    format!(
        "{} · {}-bit · {} ch",
        rate(f.sample_rate),
        f.bits,
        f.channels
    )
}

fn sent_row(l: &LinkStatus, endpoint: Option<&PcmFormat>) -> Row {
    let r = row(
        "Sent to the buds",
        format!("{} · {}-bit", rate(l.stream_rate), l.stream_bits),
    );
    match endpoint {
        Some(e) if e.sample_rate != 0 && e.sample_rate != l.stream_rate => {
            r.detail(format!("resampled from {}", rate(e.sample_rate)))
        }
        Some(e) if e.bits != 0 && e.bits < l.stream_bits => {
            r.detail(format!("{}-bit input widened", e.bits))
        }
        Some(e) if e.bits > l.stream_bits && l.stream_bits != 0 => {
            r.detail(format!("{}-bit input dithered down", e.bits))
        }
        _ => r,
    }
}

/// "291 kbps" with where that is on the ladder.
fn bitrate_row(l: &LinkStatus) -> Row {
    let r = row("Bitrate", format!("{} kbps", l.bitrate_kbps));
    if !l.adaptive {
        return r.detail("fixed");
    }
    let steps = l.ladder_kbps.len();
    let top = if l.top_kbps != 0 {
        l.top_kbps
    } else {
        l.ladder_kbps.first().copied().unwrap_or(l.bitrate_kbps)
    };
    let ladder: Vec<String> = l.ladder_kbps.iter().map(u32::to_string).collect();
    if l.bitrate_kbps >= top {
        return if ladder.len() > 1 {
            r.detail(format!("adaptive · ladder {}", ladder.join(" / ")))
        } else {
            r.detail("adaptive · at the top")
        };
    }
    let step = (l.rung as usize).saturating_add(1);
    let r = r.tone(Tone::Warn);
    if steps > 0 && step <= steps {
        r.detail(format!("stepped down · step {step} of {steps} · top {top}"))
    } else {
        r.detail(format!("stepped down · top {top}"))
    }
}

fn volume_row(v: &VolumeStatus) -> Row {
    let level = v.level_percent.map(|p| format!("{p} %"));
    let who = match v.mode {
        VolumeMode::Buds => "set on the buds",
        VolumeMode::Software => "applied by the service",
        VolumeMode::Windows => "applied by Windows",
        VolumeMode::Unknown => "unknown",
    };
    let mut value = match level {
        Some(l) => format!("{l} · {who}"),
        None => who.to_string(),
    };
    if v.muted {
        value.push_str(" · muted");
    }
    let r = row("Volume", value).detail(match v.mode {
        VolumeMode::Buds => "the Windows slider moves the buds' own volume",
        VolumeMode::Software => "the buds did not take it: the service scales the audio",
        VolumeMode::Windows => "Windows scales the audio before it is sent",
        VolumeMode::Unknown => "",
    });
    if v.muted { r.tone(Tone::Warn) } else { r }
}

fn ago(now: u64, then: u64) -> String {
    if then == 0 || now < then {
        "earlier".to_string()
    } else if now - then < 5 {
        "just now".to_string()
    } else {
        format!("{} ago", span(now - then))
    }
}

fn note(e: &Event, now: u64) -> (Tone, String) {
    let tone = match e.level {
        EventLevel::Error => Tone::Bad,
        EventLevel::Warning | EventLevel::Unknown => Tone::Warn,
    };
    (tone, format!("{}: {}", ago(now, e.time_unix), e.text))
}

/// What the card says under the title when there is no stream to
/// describe: what is being waited for.
fn waiting_for(p: LinkPhase) -> &'static str {
    match p {
        LinkPhase::WaitingDriver => "Waiting for the crossc driver",
        LinkPhase::WaitingBuds => "Waiting for the buds to connect to this PC",
        LinkPhase::Reconnecting => "Setting the stream up again",
        LinkPhase::Stopped => "The service has stopped this stream",
        LinkPhase::Ready | LinkPhase::Streaming | LinkPhase::Unknown => "",
    }
}

/// The four numbers of a stream, for the top of its card.
fn stats(l: &LinkStatus) -> Vec<Stat> {
    let stat = |label, value: String| Stat {
        label,
        value,
        tone: Tone::Normal,
    };
    let top = if l.top_kbps != 0 {
        l.top_kbps
    } else {
        l.bitrate_kbps
    };
    vec![
        stat(
            "Mode",
            match l.mode {
                CodecMode::Unknown => "Unknown".to_string(),
                m => mode_name(m).to_string(),
            },
        ),
        stat("Sample rate", rate(l.stream_rate)),
        Stat {
            tone: if l.bitrate_kbps < top {
                Tone::Warn
            } else {
                Tone::Normal
            },
            ..stat("Bitrate", format!("{} kbps", l.bitrate_kbps))
        },
        match l.mtu_limit {
            0 => Stat {
                tone: Tone::Muted,
                ..stat("Media MTU", "unknown".to_string())
            },
            mtu => stat("Media MTU", format!("{mtu} B")),
        },
    ]
}

/// The card of one device. `now`: the sender's clock (seconds since the
/// Unix epoch), for "… ago".
pub fn card(d: &DeviceStatus, now: u64) -> Card {
    let mut sections = Vec::new();

    let mut audio = Vec::new();
    audio.push(match &d.endpoint {
        Some(f) => row("Windows format", format_row(f)),
        None => row("Windows format", "nothing played yet").tone(Tone::Muted),
    });
    if let Some(l) = &d.link {
        audio.push(sent_row(l, d.endpoint.as_ref()));
        audio.push(bitrate_row(l));
        audio.push(match l.sink_delay_tenth_ms {
            Some(t) => {
                row("Delay", format!("{}.{} ms", t / 10, t % 10)).detail("reported by the buds")
            }
            None => row("Delay", "not reported").tone(Tone::Muted),
        });
    }
    if let Some(v) = &d.volume {
        audio.push(volume_row(v));
    }
    sections.push(Section {
        title: "Audio",
        rows: audio,
    });

    let mut link = Vec::new();
    // The state itself is the badge; the row is for the reason.
    if !d.phase_reason.is_empty() {
        link.push(
            row("State", upper_first(phase_text(d)))
                .tone(phase_tone(d.phase))
                .detail(d.phase_reason.clone()),
        );
    }
    if let Some(l) = &d.link {
        link.push(row(
            "Packet",
            format!(
                "{} bytes · {} frame{}",
                l.packet_bytes,
                l.frames_per_packet,
                if l.frames_per_packet == 1 { "" } else { "s" }
            ),
        ));
        link.push(
            row("Media MTU", format!("{} bytes", l.mtu_limit))
                .detail("the largest packet this link takes"),
        );
        link.push(
            row(
                "Codec options",
                format!("{:#04x} of {:#04x}", l.config, l.caps),
            )
            .detail("in use, of what the buds offer"),
        );
    }
    link.push(match &d.driver {
        Some(drv) => {
            row("Driver", drv.version.clone()).detail(format!("interface version {}", drv.protocol))
        }
        None => row("Driver", "no answer").tone(Tone::Muted),
    });
    sections.push(Section {
        title: "Link",
        rows: link,
    });

    // Nothing has happened on a device that never had a stream: a block
    // of zeros would only look like measurements.
    let c = &d.counters;
    if d.link.is_some() || *c != Counters::default() {
        let bad = |n: u64| if n > 0 { Tone::Warn } else { Tone::Normal };
        let mut activity = vec![
            row("Packets sent", grouped(c.packets)),
            row("Frames read", grouped(c.pcm_frames)),
            row("Dropped frames", grouped(c.dropped_frames)).tone(bad(c.dropped_frames)),
            row("Stream gaps", grouped(c.discontinuities)),
            row("Write errors", grouped(c.write_errors)).tone(bad(c.write_errors)),
            row("Late packets", grouped(c.late_packets)).tone(bad(c.late_packets)),
        ];
        // The two latencies describe the stream that runs.
        if let Some(ms) = d.link.as_ref().and_then(|l| l.streaming_ms) {
            activity.push(row(
                "Write latency",
                format!(
                    "{} avg · {} max",
                    millis(c.write_avg_us),
                    millis(c.write_max_us)
                ),
            ));
            activity.push(row("Streaming for", span(ms / 1000)));
        }
        activity.push(
            row("Reconnects", grouped(u64::from(c.reconnects))).tone(bad(u64::from(c.reconnects))),
        );
        sections.push(Section {
            title: "Activity",
            rows: activity,
        });
    }

    let summary = match (d.phase, &d.link) {
        (LinkPhase::Streaming | LinkPhase::Ready, Some(l)) => {
            format!("{} · {}-bit", mode_long(l.mode), l.stream_bits)
        }
        _ => waiting_for(d.phase).to_string(),
    };
    // A warning that only repeats the reason shown under "State" is left
    // out.
    let note = d
        .last_event
        .as_ref()
        .filter(|e| d.phase_reason.is_empty() || !d.phase_reason.contains(e.text.as_str()))
        .map(|e| note(e, now));
    Card {
        id: d.id.clone(),
        title: d.display_name().to_string(),
        badge: upper_first(phase_text(d)),
        badge_tone: phase_tone(d.phase),
        summary,
        stats: d.link.as_ref().map(stats).unwrap_or_default(),
        sections,
        note,
    }
}

fn upper_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// The cards of a state (none unless it is live with devices).
pub fn cards(feed: &Feed) -> Vec<Card> {
    match feed {
        Feed::Live(s) => s.devices.iter().map(|d| card(d, s.time_unix)).collect(),
        _ => Vec::new(),
    }
}

/// The line at the bottom: the service.
pub fn footer(feed: &Feed) -> String {
    match feed {
        Feed::Live(s) => {
            let mut parts = vec![format!(
                "crossc service {}",
                if s.service.version.is_empty() {
                    "(version unknown)"
                } else {
                    &s.service.version
                }
            )];
            if s.service.started_unix != 0 && s.time_unix >= s.service.started_unix {
                parts.push(format!(
                    "running for {}",
                    span(s.time_unix - s.service.started_unix)
                ));
            }
            parts.join(" · ")
        }
        _ => format!("crossc tray {}", env!("CARGO_PKG_VERSION")),
    }
}

/// The service's own last warning, if any.
pub fn service_note(feed: &Feed) -> Option<(Tone, String)> {
    match feed {
        Feed::Live(s) => s.service.last_event.as_ref().map(|e| note(e, s.time_unix)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// text report ("Copy details", --print-once)
// ---------------------------------------------------------------------------

/// Everything as plain text.
pub fn report(feed: &Feed) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "crossc status");
    if let Some((_, title, text)) = headline(feed) {
        let _ = writeln!(out, "{title}: {text}");
    }
    if let Feed::Live(s) = feed {
        let _ = writeln!(out, "{} · status version {}", footer(feed), s.version);
        if !s.service.log_dir.is_empty() {
            let _ = writeln!(out, "log: {}", s.service.log_dir);
        }
        if let Some((_, text)) = service_note(feed) {
            let _ = writeln!(out, "last service warning: {text}");
        }
        for d in &s.devices {
            let c = card(d, s.time_unix);
            let _ = writeln!(out);
            let _ = writeln!(out, "{} [{}] — {}", c.title, d.id, c.badge);
            if !c.summary.is_empty() {
                let _ = writeln!(out, "  {}", c.summary);
            }
            for section in &c.sections {
                for r in &section.rows {
                    let _ = match &r.detail {
                        Some(detail) => {
                            writeln!(out, "  {:<17} {} ({detail})", r.label, r.value)
                        }
                        None => writeln!(out, "  {:<17} {}", r.label, r.value),
                    };
                }
            }
            if let Some(l) = &d.link {
                if !l.ladder_kbps.is_empty() {
                    let ladder: Vec<String> = l.ladder_kbps.iter().map(u32::to_string).collect();
                    let _ = writeln!(
                        out,
                        "  {:<17} {} kbps",
                        "Bitrate ladder",
                        ladder.join(" / ")
                    );
                }
                if !l.mtu_reason.is_empty() {
                    let _ = writeln!(out, "  {:<17} {}", "MTU rule", l.mtu_reason);
                }
                let _ = writeln!(
                    out,
                    "  {:<17} configuration {:#04x}, capabilities {:#04x}",
                    "SSC octets", l.config, l.caps
                );
            }
            // The card leaves out a warning that repeats the state's
            // reason; a report has it all the same, with its time.
            if let Some(e) = &d.last_event {
                let _ = writeln!(out, "  {:<17} {}", "Last warning", note(e, s.time_unix).1);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// flyout geometry
// ---------------------------------------------------------------------------

/// Width of one card's column, logical pixels.
pub const COLUMN_WIDTH: u32 = 380;
/// Cards side by side at most.
pub const MAX_COLUMNS: u32 = 2;

/// Space around the flyout's content.
pub const PADDING: u32 = 14;
/// Space between cards.
pub const GAP: u32 = 12;
/// Height of a row's first line.
pub const ROW: u32 = 20;
/// Height of a row's detail line.
pub const DETAIL: u32 = 16;
/// Height of a section title, with the space above it.
pub const SECTION_TITLE: u32 = 26;
/// Height of a card's title and summary.
pub const CARD_HEAD: u32 = 50;
/// Height of the row of large numbers, with the space above it.
pub const CARD_STATS: u32 = 58;
/// Height of a warning (two lines of text), with the space above it.
pub const CARD_NOTE: u32 = 50;
/// A card's own padding and border, top and bottom together.
pub const CARD_PAD: u32 = 26;
/// Height of the footer, with the space above it.
pub const FOOTER: u32 = 30;
/// Height of the box shown instead of cards.
pub const HEADLINE: u32 = 150;

/// Characters of a detail line that surely fit the value column.
const DETAIL_CHARS: usize = 40;
/// A detail takes this many lines at most; the rest is cut.
const DETAIL_MAX_LINES: u32 = 3;

/// The lines a row's detail is given (0: it has none). Counted by words,
/// the way the text wraps.
pub fn detail_lines(row: &Row) -> u32 {
    let Some(detail) = &row.detail else {
        return 0;
    };
    let (mut lines, mut used) = (1u32, 0usize);
    for word in detail.split_whitespace() {
        let len = word.chars().count();
        if used > 0 && used + 1 + len > DETAIL_CHARS {
            lines += 1;
            used = len;
        } else {
            used += len + usize::from(used > 0);
        }
        // A word longer than a line breaks inside.
        lines += (used.saturating_sub(1) / DETAIL_CHARS) as u32;
        used = (used.saturating_sub(1)) % DETAIL_CHARS + 1;
    }
    lines.min(DETAIL_MAX_LINES)
}

fn card_height(c: &Card) -> u32 {
    let rows: u32 = c
        .sections
        .iter()
        .map(|s| {
            SECTION_TITLE
                + s.rows
                    .iter()
                    .map(|r| ROW + DETAIL * detail_lines(r))
                    .sum::<u32>()
        })
        .sum();
    CARD_HEAD
        + if c.stats.is_empty() { 0 } else { CARD_STATS }
        + rows
        + if c.note.is_some() { CARD_NOTE } else { 0 }
        + CARD_PAD
}

/// The size the flyout wants for a state, logical pixels (width, height).
pub fn flyout_size(feed: &Feed) -> (u32, u32) {
    let cards = cards(feed);
    if cards.is_empty() {
        return (COLUMN_WIDTH + 2 * PADDING, HEADLINE + FOOTER + 2 * PADDING);
    }
    let columns = (cards.len() as u32).min(MAX_COLUMNS);
    let width = columns * COLUMN_WIDTH + (columns - 1) * GAP + 2 * PADDING;
    // Cards are laid out in rows of `columns`; a row is as high as its
    // highest card.
    let heights: Vec<u32> = cards.iter().map(card_height).collect();
    let rows: u32 = heights
        .chunks(columns as usize)
        .map(|row| row.iter().copied().max().unwrap_or(0))
        .sum();
    let row_count = heights.len().div_ceil(columns as usize) as u32;
    let note = if service_note(feed).is_some() {
        CARD_NOTE
    } else {
        0
    };
    (
        width,
        rows + (row_count - 1) * GAP + note + FOOTER + 2 * PADDING,
    )
}

/// A rectangle in physical screen pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

/// Where a flyout of `size` (physical pixels; cut to the work area if it
/// is larger) goes: next to the notification icon `anchor`, inside `work`
/// (the monitor's area without the taskbar), `margin` away from its edges.
pub fn place(anchor: Rect, work: Rect, size: (i32, i32), margin: i32) -> Rect {
    let w = size.0.min(work.width() - 2 * margin).max(1);
    let h = size.1.min(work.height() - 2 * margin).max(1);
    let clamp = |v: i32, lo: i32, hi: i32| v.max(lo).min(hi.max(lo));
    let centre_x = anchor.left + anchor.width() / 2;
    let centre_y = anchor.top + anchor.height() / 2;
    let (min_x, max_x) = (work.left + margin, work.right - margin - w);
    let (min_y, max_y) = (work.top + margin, work.bottom - margin - h);
    // The taskbar is on the side of the work area the icon lies beyond;
    // the flyout sits against that side and is centred on the icon along
    // it, as far as the work area lets it.
    let (x, y) = if anchor.top >= work.bottom {
        (clamp(centre_x - w / 2, min_x, max_x), max_y)
    } else if anchor.bottom <= work.top {
        (clamp(centre_x - w / 2, min_x, max_x), min_y)
    } else if anchor.left >= work.right {
        (max_x, clamp(centre_y - h / 2, min_y, max_y))
    } else if anchor.right <= work.left {
        (min_x, clamp(centre_y - h / 2, min_y, max_y))
    } else {
        // The icon is inside the work area (the overflow flyout, or an
        // auto-hidden taskbar): above it if there is room, else below.
        let above = anchor.top - margin - h;
        let y = if above >= min_y {
            above
        } else {
            clamp(anchor.bottom + margin, min_y, max_y)
        };
        (clamp(centre_x - w / 2, min_x, max_x), y)
    };
    Rect {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
