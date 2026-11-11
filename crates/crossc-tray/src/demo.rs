//! `--demo`: made-up states, so the icon and the flyout can be looked at
//! without the service, the driver or buds. A script runs through them in
//! a loop; `--demo-state <name>` holds one. The numbers move with time so
//! the windows are seen updating.

use crossc_status::{
    CodecMode, Counters, DeviceStatus, DriverInfo, Event, EventLevel, LinkPhase, LinkStatus,
    PROTOCOL_VERSION, PcmFormat, ServiceInfo, Snapshot, VolumeMode, VolumeStatus,
};

use crate::view::{Feed, Outage};

/// A made-up clock: the service started at this time.
const STARTED: u64 = 1_800_000_000;

/// The steps of the script: name, and for how long it is shown (ms).
pub const SCRIPT: [(&str, u64); 8] = [
    ("off", 3_000),
    ("waiting", 4_000),
    ("hifi", 6_000),
    ("uhq", 6_000),
    ("adaptive", 6_000),
    ("reconnect", 4_000),
    ("two", 8_000),
    ("mismatch", 3_000),
];

/// The names `--demo-state` takes.
#[cfg(test)]
pub fn state_names() -> Vec<&'static str> {
    SCRIPT
        .iter()
        .map(|(n, _)| *n)
        .chain(["nobuds", "untrusted"])
        .collect()
}

/// The step of the script at `elapsed_ms` (it loops), and how far into
/// the script's run that is.
pub fn step_at(elapsed_ms: u64) -> &'static str {
    let total: u64 = SCRIPT.iter().map(|(_, d)| d).sum();
    let mut t = elapsed_ms % total;
    for (name, d) in SCRIPT {
        if t < d {
            return name;
        }
        t -= d;
    }
    SCRIPT[0].0
}

/// The state of the script at `elapsed_ms`.
pub fn feed_at(elapsed_ms: u64) -> Feed {
    state(step_at(elapsed_ms), elapsed_ms).unwrap_or(Feed::Connecting)
}

fn service(elapsed_ms: u64) -> Snapshot {
    Snapshot {
        version: PROTOCOL_VERSION,
        seq: elapsed_ms / 500 + 1,
        time_unix: STARTED + 7_260 + elapsed_ms / 1000,
        service: ServiceInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_unix: STARTED,
            log_dir: r"C:\ProgramData\crossc".to_string(),
            last_event: None,
        },
        devices: Vec::new(),
    }
}

fn driver() -> Option<DriverInfo> {
    Some(DriverInfo {
        version: "0.2.0".into(),
        protocol: 3,
    })
}

/// Counters of a stream of `kbps` and `rate` Hz that has run for `ms`.
fn counters(ms: u64, rate: u64) -> Counters {
    // One 864-sample frame per packet.
    let frames = ms * rate / 1000;
    Counters {
        packets: frames / 864,
        pcm_frames: frames,
        dropped_frames: 0,
        discontinuities: 1,
        write_errors: 0,
        write_avg_us: 280 + (ms / 500 % 7) as u32 * 9,
        write_max_us: 3_900,
        late_packets: 0,
        reconnects: 0,
    }
}

fn buds4(elapsed_ms: u64) -> DeviceStatus {
    DeviceStatus {
        id: "AABBCCDDEEFF".into(),
        name: "Galaxy Buds4 Pro".into(),
        phase: LinkPhase::WaitingBuds,
        phase_reason: String::new(),
        endpoint: None,
        link: None,
        volume: Some(VolumeStatus {
            mode: VolumeMode::Buds,
            level_percent: Some(62),
            muted: false,
        }),
        counters: Counters::default(),
        driver: driver(),
        last_event: None,
    }
    .tap(|d| d.counters.reconnects = (elapsed_ms / 3_600_000) as u32)
}

trait Tap: Sized {
    fn tap(mut self, f: impl FnOnce(&mut Self)) -> Self {
        f(&mut self);
        self
    }
}
impl<T> Tap for T {}

fn uhq(bitrate: u32, rung: u32, streaming_ms: u64) -> LinkStatus {
    LinkStatus {
        mode: CodecMode::Uhq,
        config: 0x0A,
        caps: 0x3E,
        stream_rate: 96_000,
        stream_bits: 24,
        bitrate_kbps: bitrate,
        top_kbps: 584,
        adaptive: true,
        ladder_kbps: vec![584, 291, 152],
        rung,
        frames_per_packet: 1,
        packet_bytes: match bitrate {
            584 => 672,
            291 => 342,
            _ => 186,
        },
        mtu_limit: 1005,
        mtu_reason: "min of plausible reported MTUs (out_mtu=1021 in_mtu=1021) = 1021; 1021 is our own requested in_mtu, so capped at 1005".into(),
        sink_delay_tenth_ms: Some(1500),
        streaming_ms: Some(streaming_ms),
    }
}

fn hifi(streaming_ms: Option<u64>) -> LinkStatus {
    LinkStatus {
        mode: CodecMode::Hifi,
        config: 0x0C,
        caps: 0x3E,
        stream_rate: 48_000,
        stream_bits: 24,
        bitrate_kbps: 192,
        top_kbps: 192,
        adaptive: true,
        ladder_kbps: vec![192, 128, 88],
        rung: 0,
        frames_per_packet: 1,
        packet_bytes: 496,
        mtu_limit: 1005,
        mtu_reason: "min of plausible reported MTUs (out_mtu=1021 in_mtu=1021) = 1021; 1021 is our own requested in_mtu, so capped at 1005".into(),
        sink_delay_tenth_ms: Some(1500),
        streaming_ms,
    }
}

const F48: PcmFormat = PcmFormat {
    sample_rate: 48_000,
    bits: 24,
    channels: 2,
};
const F96: PcmFormat = PcmFormat {
    sample_rate: 96_000,
    bits: 24,
    channels: 2,
};

/// The state called `name`, `elapsed_ms` into the demo. `None`: no such
/// state.
pub fn state(name: &str, elapsed_ms: u64) -> Option<Feed> {
    let mut s = service(elapsed_ms);
    // A stream that started a minute before the demo did.
    let run = 61_000 + elapsed_ms;
    match name {
        "off" => return Some(Feed::Down(Outage::NotRunning)),
        "mismatch" => return Some(Feed::Down(Outage::Version { theirs: 2 })),
        "untrusted" => return Some(Feed::Down(Outage::Untrusted)),
        "nobuds" => {}
        "waiting" => s.devices.push(buds4(elapsed_ms)),
        "hifi" => s.devices.push(buds4(elapsed_ms).tap(|d| {
            d.phase = LinkPhase::Streaming;
            d.endpoint = Some(F48);
            d.link = Some(hifi(Some(run)));
            d.counters = counters(run, 48_000);
        })),
        "uhq" => s.devices.push(buds4(elapsed_ms).tap(|d| {
            d.phase = LinkPhase::Streaming;
            d.endpoint = Some(F96);
            d.link = Some(uhq(584, 0, run));
            d.counters = counters(run, 96_000);
        })),
        "adaptive" => s.devices.push(buds4(elapsed_ms).tap(|d| {
            d.phase = LinkPhase::Streaming;
            d.endpoint = Some(F96);
            d.link = Some(uhq(291, 1, run));
            d.counters = counters(run, 96_000);
            d.counters.late_packets = 14;
            d.counters.write_max_us = 41_000;
            d.last_event = Some(Event {
                level: EventLevel::Warning,
                time_unix: s.time_unix.saturating_sub(4),
                text: "bitrate 584 -> 291 kbps: media writes are slow (link congested)".into(),
            });
        })),
        "reconnect" => s.devices.push(buds4(elapsed_ms).tap(|d| {
            d.phase = LinkPhase::Reconnecting;
            d.phase_reason = "link down: media write failed; next attempt in 2 s".into();
            d.endpoint = Some(F96);
            d.counters = counters(run, 96_000);
            d.counters.write_errors = 1;
            d.counters.reconnects = 1;
            d.last_event = Some(Event {
                level: EventLevel::Warning,
                time_unix: s.time_unix.saturating_sub(1),
                text: "link down: media write failed".into(),
            });
        })),
        "two" => {
            s.devices.push(buds4(elapsed_ms).tap(|d| {
                d.phase = LinkPhase::Streaming;
                d.endpoint = Some(F96);
                d.link = Some(uhq(584, 0, run));
                d.counters = counters(run, 96_000);
                d.counters.reconnects = 1;
                d.last_event = Some(Event {
                    level: EventLevel::Warning,
                    time_unix: s.time_unix.saturating_sub(95),
                    text: "link down: the buds closed the media channel".into(),
                });
            }));
            s.devices.push(DeviceStatus {
                id: "AABBCCDDEE00".into(),
                name: "Galaxy Buds3 Pro".into(),
                phase: LinkPhase::Ready,
                phase_reason: String::new(),
                endpoint: Some(PcmFormat {
                    sample_rate: 48_000,
                    bits: 16,
                    channels: 2,
                }),
                link: Some(hifi(None)),
                volume: Some(VolumeStatus {
                    mode: VolumeMode::Software,
                    level_percent: Some(40),
                    muted: true,
                }),
                counters: counters(18_000, 48_000),
                driver: driver(),
                last_event: None,
            });
        }
        _ => return None,
    }
    Some(Feed::Live(Box::new(s)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{Glyph, Quality, glyph};

    #[test]
    fn the_script_runs_through_every_step_and_loops() {
        let total: u64 = SCRIPT.iter().map(|(_, d)| d).sum();
        let mut seen = Vec::new();
        for t in (0..total).step_by(500) {
            let name = step_at(t);
            if seen.last() != Some(&name) {
                seen.push(name);
            }
        }
        assert_eq!(
            seen,
            [
                "off",
                "waiting",
                "hifi",
                "uhq",
                "adaptive",
                "reconnect",
                "two",
                "mismatch"
            ]
        );
        assert_eq!(step_at(total), "off");
        assert_eq!(step_at(total * 3 + 3_500), "waiting");
        assert_eq!(step_at(u64::MAX), step_at(u64::MAX % total));
    }

    #[test]
    fn every_state_exists_and_gives_the_icon_it_should() {
        let icons: Vec<Glyph> = state_names()
            .into_iter()
            .map(|n| glyph(&state(n, 0).unwrap_or_else(|| panic!("{n}"))))
            .collect();
        assert_eq!(
            icons,
            [
                Glyph::Off,
                Glyph::Waiting,
                Glyph::Streaming(Quality::Hifi),
                Glyph::Streaming(Quality::Uhq),
                Glyph::Streaming(Quality::Uhq),
                Glyph::Waiting,
                Glyph::Streaming(Quality::Uhq),
                Glyph::Problem,
                Glyph::Waiting,
                Glyph::Problem,
            ]
        );
        assert_eq!(state("no such state", 0), None);
    }

    #[test]
    fn demo_states_are_snapshots_the_wire_carries() {
        for n in state_names() {
            for t in [0, 1_234, 3_600_000] {
                if let Some(Feed::Live(s)) = state(n, t) {
                    let msg = crossc_status::wire::encode(&s).unwrap();
                    assert_eq!(crossc_status::wire::decode(&msg).unwrap(), *s, "{n}");
                }
            }
        }
        // Numbers move.
        let (Some(Feed::Live(a)), Some(Feed::Live(b))) = (state("uhq", 0), state("uhq", 2_000))
        else {
            panic!()
        };
        assert!(b.devices[0].counters.packets > a.devices[0].counters.packets);
        assert!(b.seq > a.seq);
    }

    #[test]
    fn no_real_address_or_user_path_in_the_demo() {
        for n in state_names() {
            let text = format!("{:?}", state(n, 0));
            assert!(!text.contains(r"C:\Users"), "{n}");
        }
    }
}
