//! Session state-machine tests against the fake driver. Time is virtual, so
//! every expectation below (times in ms, packet counts) is exact.

use super::*;
use crate::fake::MemoryLog;
use crate::fake::driver::{
    FMT_48K_16, FMT_48K_24, FMT_96K_24, FakeClock, FakeDriver, FakeLink, FakeState,
};
use crate::resample::stream_frames_out;
use avdtp::caps::Capability;
use avdtp::codec::CodecInfo;
use avdtp::msg::Command;
use avdtp::rtp::RtpHeader;
use ssc_enc::Bitrate;

use StreamState::{Paused, Running};

struct Rig {
    fake: FakeDriver,
    log: Arc<MemoryLog>,
    s: Session<FakeDriver, FakeLink, FakeClock>,
}

fn rig() -> Rig {
    rig_with(Settings::default())
}

fn rig_with(settings: Settings) -> Rig {
    let fake = FakeDriver::new();
    let log = Arc::new(MemoryLog::new(fake.with(|s| s.clock_us.clone())));
    let s = Session::new(
        fake.clone(),
        fake.link_probe(),
        fake.clock(),
        settings,
        log.clone(),
        fake.stop(),
        "buds",
    );
    Rig { fake, log, s }
}

impl Rig {
    /// Script something.
    fn at(&self, secs: f64, f: impl FnOnce(&mut FakeState) + 'static) -> &Self {
        self.fake.at(secs, f);
        self
    }
    /// The render stream changes at `secs`.
    fn stream(&self, secs: f64, st: StreamState, fmt: PcmFormat) -> &Self {
        self.at(secs, move |s| s.set_stream(st, fmt))
    }
    /// Stop at `secs` and run the session to its end.
    fn run_to(&mut self, secs: f64) {
        self.fake.stop_at(secs);
        self.s.run();
        assert_eq!(self.s.phase(), Phase::Stopped);
        self.check_clean_stop();
    }
    /// After a stop nothing is left behind, whatever happened before.
    fn check_clean_stop(&self) {
        self.fake.with(|s| {
            let ctx = self.log.text();
            assert!(!s.link(), "link flag left on\n{ctx}");
            assert!(!s.attached(), "still attached\n{ctx}");
            assert_eq!((s.signaling, s.media), (None, None), "channels left\n{ctx}");
            assert!(
                s.sink.disconnected || s.sink.configured.is_none(),
                "the sink still holds our stream\n{ctx}"
            );
            assert_eq!(s.calls.last().map(String::as_str), Some("detach"));
        });
    }
    /// The stream commands we sent (the capability probe left out).
    fn cmds(&self) -> Vec<&'static str> {
        self.fake.with(|s| {
            s.sink
                .commands
                .iter()
                .filter_map(|c| match c {
                    Command::SetConfiguration { .. } => Some("SET_CONFIG"),
                    Command::Open { .. } => Some("OPEN"),
                    Command::Start { .. } => Some("START"),
                    Command::Suspend { .. } => Some("SUSPEND"),
                    Command::Close { .. } => Some("CLOSE"),
                    Command::Abort { .. } => Some("ABORT"),
                    _ => None,
                })
                .collect()
        })
    }
    fn count(&self, cmd: &str) -> usize {
        self.cmds().iter().filter(|c| **c == cmd).count()
    }
    /// The SSC configuration octet of every SET_CONFIGURATION we sent.
    fn configs(&self) -> Vec<u8> {
        self.fake.with(|s| {
            s.sink
                .commands
                .iter()
                .filter_map(|c| match c {
                    Command::SetConfiguration { config, .. } => {
                        config.iter().find_map(|cap| match cap {
                            Capability::MediaCodec {
                                codec: CodecInfo::Vendor(v),
                                ..
                            } => v.ssc().map(|x| x.flags.0),
                            _ => None,
                        })
                    }
                    _ => None,
                })
                .collect()
        })
    }
    /// State transitions as (ms, "from -> to").
    fn transitions(&self) -> Vec<(u64, String)> {
        self.log
            .find("state ")
            .into_iter()
            .map(|(t, m)| {
                let m = m.split("state ").nth(1).unwrap_or("").to_string();
                (t, m)
            })
            .collect()
    }
    fn packets(&self) -> Vec<Vec<u8>> {
        self.fake.with(|s| s.packets.clone())
    }
    /// SET_LINK calls as (ms, connected).
    fn links(&self) -> Vec<(u64, bool)> {
        self.fake.with(|s| {
            s.link_calls
                .iter()
                .map(|(t, c)| (t.as_millis() as u64, *c))
                .collect()
        })
    }
    fn open_ms(&self) -> Vec<u64> {
        self.fake
            .with(|s| s.open_times.iter().map(|t| t.as_millis() as u64).collect())
    }
    fn logged(&self, needle: &str) -> usize {
        self.log.find(needle).len()
    }
}

fn tr(v: &[(u64, &str)]) -> Vec<(u64, String)> {
    v.iter().map(|(t, s)| (*t, s.to_string())).collect()
}

/// RTP sequence numbers and sizes of `packets`.
fn seq_and_len(packets: &[Vec<u8>]) -> Vec<(u16, usize)> {
    packets
        .iter()
        .map(|p| (RtpHeader::decode(p).unwrap().0.sequence, p.len()))
        .collect()
}

// ---------------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------------

#[test]
fn happy_path_48k_hifi() {
    let mut r = rig();
    // No render stream at start: the default 48 kHz/24 format is configured.
    r.stream(1.0, Running, FMT_48K_24)
        .stream(2.0, Paused, FMT_48K_24);
    r.run_to(8.0);
    assert_eq!(
        r.transitions(),
        tr(&[
            (0, "wait-driver -> wait-link"),
            (0, "wait-link -> ready"),
            (1000, "ready -> streaming"),
            (5000, "streaming -> ready"), // 3 s after rendering stopped
            (8000, "ready -> stopped"),
        ]),
        "{}",
        r.log.text()
    );
    assert_eq!(
        r.cmds(),
        ["SET_CONFIG", "OPEN", "START", "SUSPEND", "CLOSE"]
    );
    // SET_CONFIGURATION: SSC SEP 3, our SEID, config 0x0C with Delay Reporting.
    r.fake.with(|s| {
        let setc = s.sink.commands.iter().find_map(|c| match c {
            Command::SetConfiguration {
                acp_seid,
                int_seid,
                config,
            } => Some((*acp_seid, *int_seid, config.clone())),
            _ => None,
        });
        assert_eq!(
            setc,
            Some((3, INT_SEID, caps::ssc_configuration(0x0C, true)))
        );
    });
    // 1 s of audio = 48000 frames = 55 SSC frames, one per packet, sent as
    // they complete.
    let p = r.packets();
    assert_eq!(p.len(), 55);
    for (k, pkt) in p.iter().enumerate() {
        assert_eq!(pkt.len(), 12 + 484);
        let (h, _) = RtpHeader::decode(pkt).unwrap();
        assert_eq!(
            (h.sequence as usize, h.timestamp as usize, h.payload_type),
            (k, k, 96)
        );
        assert_eq!((pkt[12], pkt[15]), (0xFF, Bitrate::K192.code()));
    }
    r.fake.with(|s| {
        // Paced by the endpoint: packet k leaves with the chunk that
        // completes frame k, i.e. at 1 s + ceil(864 (k+1) / 480) * 10 ms.
        for (k, t) in s.packet_times.iter().enumerate() {
            let chunks = (864 * (k as u64 + 1)).div_ceil(480);
            assert_eq!(t.as_millis() as u64, 1000 + 10 * chunks, "packet {k}");
        }
        assert_eq!(
            s.calls[..5],
            ["info", "attach", "open", "open", "set_link(true)"]
        );
        let n = s.calls.len();
        assert_eq!(
            s.calls[n - 4..],
            [
                "close(media)",
                "close(signaling)",
                "set_link(false)",
                "detach"
            ]
        );
        assert_eq!(s.probe_calls, 1, "no probing while the link is up");
    });
    assert_eq!(r.links(), [(0, true), (8000, false)]);
    assert_eq!(r.open_ms(), [0, 0]);
    // The interval's statistics are logged when the stream is suspended.
    let stats = r.log.find("stats:");
    assert_eq!(stats.len(), 1, "{}", r.log.text());
    assert!(
        stats[0].1.contains(
            "packets=55 frames_in=48000 dropped=0 discontinuities=0 write avg=0us max=0us"
        ),
        "{}",
        stats[0].1
    );
    assert!(r.logged("link up: HiFi 48 kHz/24: config 0x0c, 192 kbps") == 1);
    assert!(r.logged("attached: driver 0.1.0 (protocol v3), buds AA:BB:CC:DD:EE:FF") == 1);
}

#[test]
fn happy_path_96k_uhq() {
    let mut r = rig();
    r.fake.with(|s| s.set_stream(Paused, FMT_96K_24));
    r.stream(1.0, Running, FMT_96K_24)
        .stream(1.5, Paused, FMT_96K_24);
    r.run_to(5.0);
    assert_eq!(r.configs(), [0x0A]);
    assert_eq!(
        r.cmds(),
        ["SET_CONFIG", "OPEN", "START", "SUSPEND", "CLOSE"]
    );
    // 0.5 s at 96 kHz = 48000 frames = 55 packets of 672 bytes.
    let p = r.packets();
    assert_eq!(p.len(), 55);
    assert!(p.iter().all(|x| x.len() == 672));
    assert_eq!(
        seq_and_len(&p).iter().map(|x| x.0).collect::<Vec<_>>(),
        (0..55).collect::<Vec<u16>>()
    );
    assert!(r.logged("UHQ 96 kHz/24: config 0x0a, 584 kbps") >= 1);
    assert_eq!(r.s.plan(), None, "no link after the stop");
}

#[test]
fn sixteen_bit_endpoint_is_widened_into_hifi() {
    let mut r = rig();
    r.fake.with(|s| s.set_stream(Paused, FMT_48K_16));
    r.stream(1.0, Running, FMT_48K_16)
        .stream(1.5, Paused, FMT_48K_16);
    r.run_to(2.0);
    assert_eq!(r.configs(), [0x0C]);
    let p = r.packets();
    assert_eq!(p.len(), 27);
    assert!(p.iter().all(|x| x.len() == 496));
}

#[test]
fn settings_reach_the_stream() {
    let mut r = rig_with(Settings {
        uhq_config: 0x0E,
        uhq_bitrate: Bitrate::K291,
        ..Settings::default()
    });
    r.fake.with(|s| s.set_stream(Paused, FMT_96K_24));
    r.stream(1.0, Running, FMT_96K_24);
    r.run_to(1.2);
    assert_eq!(r.configs(), [0x0E]);
    let fb = ssc_enc::frame_size(ssc_enc::Mode::Uhq96k, Bitrate::K291);
    assert!(r.packets().iter().all(|x| x.len() == 12 + fb));
    assert!(!r.packets().is_empty());
}

// ---------------------------------------------------------------------------
// Format changes
// ---------------------------------------------------------------------------

#[test]
fn format_switch_mid_stream_reconfigures() {
    let mut r = rig();
    r.stream(1.0, Running, FMT_48K_24)
        .stream(1.5, Running, FMT_96K_24)
        .stream(2.0, Paused, FMT_96K_24);
    r.run_to(6.0);
    assert_eq!(
        r.cmds(),
        [
            "SET_CONFIG",
            "OPEN",
            "START",
            "CLOSE",
            "SET_CONFIG",
            "OPEN",
            "START",
            "SUSPEND",
            "CLOSE"
        ],
        "{}",
        r.log.text()
    );
    assert_eq!(r.configs(), [0x0C, 0x0A]);
    assert_eq!(
        r.transitions(),
        tr(&[
            (0, "wait-driver -> wait-link"),
            (0, "wait-link -> ready"),
            (1000, "ready -> streaming"),
            (1500, "streaming -> ready"),
            (1500, "ready -> streaming"),
            (5000, "streaming -> ready"),
            (6000, "ready -> stopped"),
        ])
    );
    // 49 chunks of 48 kHz (23520 frames) -> 27 HiFi packets, then 0.5 s of
    // 96 kHz -> 55 UHQ packets in a new stream (sequence from 0 again).
    let got = seq_and_len(&r.packets());
    let want: Vec<(u16, usize)> = (0..27)
        .map(|k| (k, 496))
        .chain((0..55).map(|k| (k, 672)))
        .collect();
    assert_eq!(got, want);
    // The endpoint stayed plugged in throughout, on a second media channel.
    assert_eq!(r.links(), [(0, true), (6000, false)]);
    assert_eq!(r.open_ms(), [0, 0, 1500]);
    assert!(r.logged("reconfiguring (HiFi 48 kHz/24 -> UHQ 96 kHz/24)") == 1);
}

#[test]
fn format_change_within_the_same_mode_keeps_the_stream() {
    let mut r = rig();
    r.stream(1.0, Running, FMT_48K_24)
        .stream(1.5, Running, FMT_48K_16)
        .stream(2.0, Paused, FMT_48K_16);
    r.run_to(2.5);
    assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "START", "CLOSE"]);
    // 27 packets from the 24-bit part, 27 from the 16-bit part (the partial
    // frame at the switch is dropped), one RTP sequence.
    let got = seq_and_len(&r.packets());
    assert_eq!(got, (0..54).map(|k| (k, 496)).collect::<Vec<_>>());
    assert!(r.logged("same stream") >= 1);
    assert_eq!(r.open_ms(), [0, 0]);
}

#[test]
fn format_change_while_idle_reconfigures_before_the_next_start() {
    let mut r = rig();
    r.stream(1.0, Paused, FMT_96K_24)
        .stream(2.0, Running, FMT_96K_24);
    r.run_to(2.2);
    assert_eq!(
        r.cmds(),
        [
            "SET_CONFIG",
            "OPEN",
            "CLOSE",
            "SET_CONFIG",
            "OPEN",
            "START",
            "CLOSE"
        ]
    );
    assert_eq!(r.configs(), [0x0C, 0x0A]);
    assert!(r.packets().iter().all(|x| x.len() == 672));
    assert_eq!(r.packets().len(), 21); // 19 chunks * 960 = 18240 frames
}

// ---------------------------------------------------------------------------
// Pause, idle suspend, resume
// ---------------------------------------------------------------------------

#[test]
fn pause_idle_suspend_and_resume() {
    let mut r = rig();
    r.stream(1.0, Running, FMT_48K_24)
        .stream(1.5, Paused, FMT_48K_24)
        .stream(2.5, Running, FMT_48K_24) // back within 3 s: no SUSPEND
        .stream(3.0, Paused, FMT_48K_24) // stays paused: SUSPEND at 6.0
        .stream(8.0, Running, FMT_48K_24);
    r.run_to(8.5);
    assert_eq!(
        r.cmds(),
        ["SET_CONFIG", "OPEN", "START", "SUSPEND", "START", "CLOSE"]
    );
    assert_eq!(
        r.transitions(),
        tr(&[
            (0, "wait-driver -> wait-link"),
            (0, "wait-link -> ready"),
            (1000, "ready -> streaming"),
            (6000, "streaming -> ready"),
            (8000, "ready -> streaming"),
            (8500, "streaming -> stopped"),
        ])
    );
    // The short pause loses nothing: 0.5 s + 0.5 s = 48000 frames = 55
    // packets. After the SUSPEND the partial frame is gone: 49 chunks = 27.
    let got = seq_and_len(&r.packets());
    assert_eq!(got, (0..82).map(|k| (k, 496)).collect::<Vec<_>>());
    r.fake.with(|s| {
        let before = s
            .packet_times
            .iter()
            .filter(|t| t.as_millis() < 6000)
            .count();
        assert_eq!(before, 55);
    });
    assert_eq!(r.logged("no audio for 3 s: stream suspended"), 1);
}

#[test]
fn stream_that_disappears_is_suspended_too() {
    let mut r = rig();
    r.stream(1.0, Running, FMT_48K_24)
        .stream(1.2, StreamState::None, FMT_48K_24);
    r.run_to(5.0);
    assert_eq!(
        r.cmds(),
        ["SET_CONFIG", "OPEN", "START", "SUSPEND", "CLOSE"]
    );
    assert_eq!(r.transitions()[3], (4200, "streaming -> ready".to_string()));
}

// ---------------------------------------------------------------------------
// The sink acts
// ---------------------------------------------------------------------------

#[test]
fn sink_suspend_is_honoured_and_start_retried_while_audio_plays() {
    let mut r = rig();
    r.stream(1.0, Running, FMT_48K_24)
        .at(1.5, |s| {
            s.sink_suspends();
            s.sink.reject_start = Some(0x31);
        })
        .at(5.0, |s| s.sink.reject_start = None);
    r.run_to(6.0);
    assert_eq!(
        r.transitions(),
        tr(&[
            (0, "wait-driver -> wait-link"),
            (0, "wait-link -> ready"),
            (1000, "ready -> streaming"),
            (1500, "streaming -> ready"),
            (5500, "ready -> streaming"), // START at 3.5 refused, 5.5 accepted
            (6000, "streaming -> stopped"),
        ]),
        "{}",
        r.log.text()
    );
    assert_eq!(
        r.cmds(),
        ["SET_CONFIG", "OPEN", "START", "START", "START", "CLOSE"]
    );
    r.fake.with(|s| {
        assert!(
            s.sink.sent.contains(&vec![0xC2, 0x09]),
            "the sink's SUSPEND was accepted"
        );
        // Nothing was sent while suspended.
        assert!(
            s.packet_times
                .iter()
                .all(|t| t.as_millis() < 1500 || t.as_millis() > 5500)
        );
    });
    assert_eq!(r.packets().len(), 27 + 27);
    assert_eq!(r.links(), [(0, true), (6000, false)], "the link stayed up");
    assert_eq!(r.logged("the sink suspended the stream"), 1);
    assert_eq!(r.logged("START rejected: BAD_STATE"), 1, "logged once");
}

#[test]
fn sink_suspend_then_silence_does_not_retry_start() {
    let mut r = rig();
    r.stream(1.0, Running, FMT_48K_24)
        .at(1.5, |s| s.sink_suspends())
        .stream(2.0, Paused, FMT_48K_24)
        .stream(9.0, Running, FMT_48K_24);
    r.run_to(9.2);
    // START at 1.0 and, when audio comes back, at 9.0. None in between.
    assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "START", "START", "CLOSE"]);
    assert_eq!(r.transitions()[4], (9000, "ready -> streaming".to_string()));
}

/// The accept of a START the sink sent with label 0xA, as bytes.
const START_ACCEPT: [u8; 2] = [0xA2, 0x07];

#[test]
fn sink_start_while_ready_is_a_started_stream() {
    let mut r = rig();
    r.at(3.0, |s| s.sink_starts())
        .stream(5.0, Running, FMT_48K_24);
    r.run_to(6.0);
    assert_eq!(
        r.transitions(),
        tr(&[
            (0, "wait-driver -> wait-link"),
            (0, "wait-link -> ready"),
            (3000, "ready -> streaming"), // the sink's START, not ours
            (6000, "streaming -> stopped"),
        ]),
        "{}",
        r.log.text()
    );
    // The sink is streaming and would refuse a START: none was sent.
    assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "CLOSE"]);
    r.fake.with(|s| {
        assert!(s.sink.sent.contains(&START_ACCEPT.to_vec()));
        assert!(s.packet_times.iter().all(|t| t.as_millis() >= 5000));
    });
    assert_eq!(r.packets().len(), 55, "one second of audio");
    assert_eq!(r.links(), [(0, true), (6000, false)], "the link stayed up");
    assert_eq!(r.logged("the sink started the stream"), 1);
    assert_eq!(r.logged("START rejected"), 0);
    assert_eq!(r.logged("link down"), 0);
}

#[test]
fn sink_start_without_audio_is_suspended_after_the_idle_time() {
    let mut r = rig();
    r.at(3.0, |s| s.sink_starts());
    r.run_to(8.0);
    assert_eq!(
        r.transitions(),
        tr(&[
            (0, "wait-driver -> wait-link"),
            (0, "wait-link -> ready"),
            (3000, "ready -> streaming"),
            (6000, "streaming -> ready"), // idle for IDLE_SUSPEND
            (8000, "ready -> stopped"),
        ]),
        "{}",
        r.log.text()
    );
    assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "SUSPEND", "CLOSE"]);
    assert!(r.packets().is_empty());
    assert_eq!(r.links(), [(0, true), (8000, false)]);
}

#[test]
fn sink_suspend_then_sink_start_resumes_the_stream() {
    let mut r = rig();
    r.stream(1.0, Running, FMT_48K_24)
        .at(1.5, |s| {
            s.sink_suspends();
            s.sink.reject_start = Some(0x31);
        })
        .at(4.0, |s| s.sink_starts());
    r.run_to(30.0);
    assert_eq!(
        r.transitions(),
        tr(&[
            (0, "wait-driver -> wait-link"),
            (0, "wait-link -> ready"),
            (1000, "ready -> streaming"),
            (1500, "streaming -> ready"),
            (4000, "ready -> streaming"), // the sink's START
            (30000, "streaming -> stopped"),
        ]),
        "{}",
        r.log.text()
    );
    // Ours at 1.0 and the refused retry at 3.5; none once the sink started.
    assert_eq!(r.count("START"), 2);
    r.fake.with(|s| {
        let ms: Vec<u128> = s.packet_times.iter().map(|t| t.as_millis()).collect();
        assert!(ms.iter().all(|t| *t < 1500 || *t > 4000));
        assert!(ms.iter().any(|t| *t > 29_900), "still streaming at the end");
    });
    assert_eq!(r.links(), [(0, true), (30000, false)], "the link stayed up");
    assert_eq!(r.logged("link down"), 0);
}

#[test]
fn sink_start_crossing_ours_counts_as_started() {
    // The sink sends START while ours is on its way. We accept the sink's;
    // the sink, streaming by then, refuses ours. The stream is started.
    let mut r = rig();
    r.fake.with(|s| {
        let own = avdtp::msg::start(0xA, &[crate::media::INT_SEID]);
        s.sink.cross_with = Some((avdtp::signal::SignalId::Start, own));
    });
    r.stream(1.0, Running, FMT_48K_24);
    r.run_to(11.0);
    assert_eq!(
        r.transitions(),
        tr(&[
            (0, "wait-driver -> wait-link"),
            (0, "wait-link -> ready"),
            (1000, "ready -> streaming"),
            (11000, "streaming -> stopped"),
        ]),
        "{}",
        r.log.text()
    );
    assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "START", "CLOSE"]);
    r.fake
        .with(|s| assert!(s.sink.sent.contains(&START_ACCEPT.to_vec())));
    assert_eq!(r.logged("START rejected"), 0, "not counted as a failure");
    assert_eq!(r.links(), [(0, true), (11000, false)]);
    assert_eq!(r.packets().len(), 555, "ten seconds of audio");
}

#[test]
fn sink_close_takes_the_link_down_and_it_is_rebuilt() {
    for abort in [false, true] {
        let mut r = rig();
        r.stream(1.0, Running, FMT_48K_24).at(1.5, move |s| {
            if abort {
                s.sink_aborts()
            } else {
                s.sink_closes()
            }
        });
        r.run_to(3.0);
        assert_eq!(
            r.transitions(),
            tr(&[
                (0, "wait-driver -> wait-link"),
                (0, "wait-link -> ready"),
                (1000, "ready -> streaming"),
                (1500, "streaming -> wait-link"),
                (2500, "wait-link -> ready"), // 1 s backoff
                (2500, "ready -> streaming"),
                (3000, "streaming -> stopped"),
            ]),
            "{}",
            r.log.text()
        );
        // The sink ended the stream: no CLOSE from us for it. The CLOSE at
        // the end is for the rebuilt one.
        assert_eq!(
            r.cmds(),
            [
                "SET_CONFIG",
                "OPEN",
                "START",
                "SET_CONFIG",
                "OPEN",
                "START",
                "CLOSE"
            ]
        );
        assert_eq!(
            r.links(),
            [(0, true), (1500, false), (2500, true), (3000, false)]
        );
        assert_eq!(r.open_ms(), [0, 0, 2500, 2500]);
        let what = if abort { "ABORT" } else { "CLOSE" };
        assert_eq!(r.logged(&format!("link down: sink sent {what}")), 1);
        assert_eq!(r.logged("reconnecting in 1 s"), 1);
        r.fake.with(|s| {
            let accept = if abort {
                vec![0xE2, 0x0A]
            } else {
                vec![0xD2, 0x08]
            };
            assert!(s.sink.sent.contains(&accept), "the command was accepted");
        });
    }
}

#[test]
fn delay_reports_are_logged_and_kept() {
    let mut r = rig();
    r.fake.with(|s| s.sink.delay_after_set_config = Some(3400));
    r.at(2.0, |s| s.sink_reports_delay(2020));
    r.fake.stop_at(3.0);
    r.s.run();
    assert_eq!(r.s.sink_delay(), Some(2020));
    assert_eq!(
        r.logged("[buds] sink DELAYREPORT: 340.0 ms (SEID 0x01)"),
        1,
        "{}",
        r.log.text()
    );
    assert_eq!(r.log.find("[buds] sink DELAYREPORT: 202.0 ms")[0].0, 2000);
    r.fake.with(|s| {
        assert!(s.sink.sent.contains(&vec![0x92, 0x0D]), "setup-time report");
        assert!(s.sink.sent.contains(&vec![0xB2, 0x0D]), "later report");
    });
    r.check_clean_stop();
}

#[test]
fn sink_commands_are_served_while_idle_and_garbage_is_ignored() {
    let mut r = rig();
    r.at(1.0, |s| {
        s.sink_sends(avdtp::msg::get_configuration(5, INT_SEID))
    })
    .at(2.0, |s| s.sink.inbox.lock().unwrap().push_back(vec![0xFF]))
    .at(3.0, |s| s.sink_sends(avdtp::msg::discover(6)));
    r.run_to(4.0);
    r.fake.with(|s| {
        let cfg = s
            .sink
            .sent
            .iter()
            .find(|p| p.starts_with(&[0x52, 0x04]))
            .expect("GET_CONFIGURATION answered");
        assert_eq!(
            avdtp::msg::parse_capabilities(&cfg[2..]).unwrap(),
            caps::ssc_configuration(0x0C, true)
        );
        assert!(s.sink.sent.iter().any(|p| p.starts_with(&[0x62, 0x01])));
    });
    assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "CLOSE"]);
    assert_eq!(r.transitions().len(), 3, "nothing else happened");
}

#[test]
fn start_refused_five_times_rebuilds_the_link() {
    let mut r = rig();
    r.fake.with(|s| s.sink.reject_start = Some(0x31));
    r.stream(1.0, Running, FMT_48K_24);
    r.run_to(9.5);
    // START at 1, 3, 5, 7, 9; then CLOSE and a new link after the backoff.
    assert_eq!(r.count("START"), 5);
    assert_eq!(r.count("CLOSE"), 1);
    assert_eq!(r.links(), [(0, true), (9000, false), (9500, false)]);
    assert_eq!(
        r.logged("link down: START rejected: BAD_STATE (0x31) (5 times in a row)"),
        1
    );
    assert!(r.packets().is_empty());
}

// ---------------------------------------------------------------------------
// The link goes away
// ---------------------------------------------------------------------------

#[test]
fn disconnect_then_reconnect_when_the_buds_are_back() {
    let mut r = rig();
    r.stream(1.0, Running, FMT_48K_24)
        .at(1.5, |s| s.buds_disconnect())
        .at(7.3, |s| s.buds_connect());
    r.run_to(9.0);
    assert_eq!(
        r.transitions(),
        tr(&[
            (0, "wait-driver -> wait-link"),
            (0, "wait-link -> ready"),
            (1000, "ready -> streaming"),
            (1500, "streaming -> wait-link"),
            // Probed at 2.5 (after the 1 s backoff), 4.5, 6.5: not there.
            (8500, "wait-link -> ready"),
            (8500, "ready -> streaming"),
            (9000, "streaming -> stopped"),
        ]),
        "{}",
        r.log.text()
    );
    // No L2CAP open while Windows said "not connected".
    assert_eq!(r.open_ms(), [0, 0, 8500, 8500]);
    assert_eq!(
        r.links(),
        [(0, true), (1500, false), (8500, true), (9000, false)]
    );
    // Nobody to say CLOSE to after the disconnect.
    assert_eq!(
        r.cmds(),
        [
            "SET_CONFIG",
            "OPEN",
            "START",
            "SET_CONFIG",
            "OPEN",
            "START",
            "CLOSE"
        ]
    );
    assert_eq!(r.logged("link down: signaling channel lost"), 1);
    assert_eq!(r.logged("the buds are not connected"), 1);
    assert_eq!(r.logged("the buds are connected"), 2);
    assert_eq!(r.packets().len(), 27 + 27);
    r.fake.with(|s| assert_eq!(s.probe_calls, 1 + 4));
}

#[test]
fn backoff_is_1_2_5_then_10_seconds() {
    let mut r = rig();
    r.fake
        .with(|s| s.sink.reject_set_configuration = Some(0x29));
    r.at(29.0, |s| s.sink.reject_set_configuration = None);
    r.run_to(50.0);
    // Each attempt opens the signaling channel; the media channel only at
    // the attempt that succeeds.
    assert_eq!(
        r.open_ms(),
        [0, 1000, 3000, 8000, 18000, 28000, 38000, 38000]
    );
    assert_eq!(r.count("SET_CONFIG"), 7);
    assert_eq!(r.links(), [(38000, true), (50000, false)]);
    assert_eq!(
        r.logged("cannot set up the stream: SET_CONFIGURATION rejected"),
        1,
        "the same error is logged once a minute: {}",
        r.log.text()
    );
    assert!(r.logged("next attempt in 1 s") == 1);
    r.fake.with(|s| {
        let closes = s.calls.iter().filter(|c| *c == "close(signaling)").count();
        assert_eq!(closes, 7, "every failed attempt closed its channel");
    });
}

#[test]
fn a_stable_link_resets_the_backoff() {
    let mut r = rig();
    // Two quick failures use up the 1 s and 2 s steps; the link then stays
    // up for 15 s, so the next failure starts at 1 s again (not 5 s).
    r.at(1.0, |s| s.sink_closes())
        .at(3.0, |s| s.sink_closes())
        .at(20.0, |s| s.sink_closes());
    r.run_to(22.0);
    assert_eq!(
        r.links(),
        [
            (0, true),
            (1000, false),
            (2000, true), // after 1 s
            (3000, false),
            (5000, true), // after 2 s
            (20000, false),
            (21000, true), // after 1 s again
            (22000, false)
        ]
    );
}

#[test]
fn not_connected_means_no_l2cap_at_all() {
    let mut r = rig();
    r.fake.with(|s| s.connected = false);
    r.stream(1.0, Running, FMT_48K_24);
    r.run_to(30.0);
    r.fake.with(|s| {
        assert_eq!(s.open_calls, 0, "never paged the buds");
        assert_eq!(s.probe_calls, 15, "asked every 2 s");
        assert!(s.link_calls.iter().all(|(_, c)| !c));
    });
    assert_eq!(
        r.transitions(),
        tr(&[
            (0, "wait-driver -> wait-link"),
            (30000, "wait-link -> stopped")
        ])
    );
    assert!(r.cmds().is_empty());
    assert_eq!(r.logged("the buds are not connected"), 1);
}

#[test]
fn a_failing_link_probe_opens_nothing() {
    let mut r = rig();
    r.fake.with(|s| s.probe_error = true);
    r.at(5.0, |s| s.probe_error = false);
    r.run_to(7.0);
    assert_eq!(r.open_ms(), [6000, 6000]);
    assert_eq!(
        r.logged("cannot tell whether the buds are connected: no radio"),
        1
    );
}

#[test]
fn media_write_error_takes_the_link_down() {
    let mut r = rig();
    r.fake.with(|s| s.write_error = Some((10, 31)));
    r.stream(1.0, Running, FMT_48K_24)
        .at(1.9, |s| s.write_error = None);
    r.run_to(2.5);
    assert_eq!(
        r.transitions(),
        tr(&[
            (0, "wait-driver -> wait-link"),
            (0, "wait-link -> ready"),
            (1000, "ready -> streaming"),
            (1200, "streaming -> wait-link"), // the 11th packet's WRITE failed
            (2200, "wait-link -> ready"),
            (2200, "ready -> streaming"),
            (2500, "streaming -> stopped"),
        ]),
        "{}",
        r.log.text()
    );
    assert_eq!(
        r.links(),
        [(0, true), (1200, false), (2200, true), (2500, false)]
    );
    // The stream was still ours: CLOSE before the channels went.
    assert_eq!(
        r.cmds(),
        [
            "SET_CONFIG",
            "OPEN",
            "START",
            "CLOSE",
            "SET_CONFIG",
            "OPEN",
            "START",
            "CLOSE"
        ]
    );
    assert_eq!(r.packets().len(), 10 + 16);
    assert_eq!(r.logged("link down: media write failed"), 1);
}

#[test]
fn media_channel_loss_takes_the_link_down_and_closes_the_stream() {
    // Only the media channel dies; the signaling channel still works, so
    // the stream is given back with CLOSE.
    let mut r = rig();
    r.stream(1.0, Running, FMT_48K_24)
        .at(1.25, |s| s.media_disconnected = true);
    r.run_to(2.0);
    assert_eq!(r.transitions()[3], (1260, "streaming -> wait-link".into()));
    assert_eq!(r.count("CLOSE"), 1, "{}", r.log.text());
    assert_eq!(r.links()[1], (1260, false));
    assert_eq!(
        r.logged("link down: media write failed: remote disconnected the channel"),
        1
    );
    // The link flag went off before the CLOSE exchange.
    r.fake.with(|s| {
        let off = s.calls.iter().position(|c| c == "set_link(false)").unwrap();
        let closed = s.calls.iter().position(|c| c == "close(media)").unwrap();
        assert!(off < closed);
    });
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

#[test]
fn driver_detach_starts_over() {
    let mut r = rig();
    r.stream(1.0, Running, FMT_48K_24)
        .at(1.5, |s| s.force_detach(995));
    r.run_to(12.0);
    assert_eq!(
        r.transitions(),
        tr(&[
            (0, "wait-driver -> wait-link"),
            (0, "wait-link -> ready"),
            (1000, "ready -> streaming"),
            (1500, "streaming -> wait-driver"),
            (11500, "wait-driver -> wait-link"), // retried after 10 s
            (11500, "wait-link -> ready"),
            (11500, "ready -> streaming"),
            (12000, "streaming -> stopped"),
        ]),
        "{}",
        r.log.text()
    );
    assert_eq!(r.logged("the driver detached the session"), 1);
    r.fake.with(|s| {
        assert_eq!(s.calls.iter().filter(|c| *c == "attach").count(), 2);
        // The driver had closed the channels itself.
        assert_eq!(s.calls.iter().filter(|c| *c == "close(stale)").count(), 2);
    });
    // The driver cleared the link flag itself at the detach.
    assert_eq!(r.links(), [(0, true), (11500, true), (12000, false)]);
    assert_eq!(r.packets().len(), 27 + 27);
}

#[test]
fn old_protocol_is_logged_once_and_retried() {
    let mut r = rig();
    r.fake.with(|s| s.protocol_version = 1);
    r.at(35.0, |s| s.protocol_version = 2);
    r.run_to(45.0);
    assert_eq!(
        r.transitions(),
        tr(&[
            (40000, "wait-driver -> wait-link"),
            (40000, "wait-link -> ready"),
            (45000, "ready -> stopped"),
        ])
    );
    let lines = r.log.find("driver not ready");
    assert_eq!(lines.len(), 1, "{}", r.log.text());
    assert!(
        lines[0]
            .1
            .contains("protocol v1 and has no audio path (v2 needed); retrying every 10 s")
    );
    r.fake.with(|s| {
        assert_eq!(s.calls.iter().filter(|c| *c == "info").count(), 5);
        assert_eq!(s.calls.iter().filter(|c| *c == "attach").count(), 1);
    });
}

#[test]
fn acx_off_busy_and_unknown_address_are_retried() {
    // ACX disabled.
    let mut r = rig();
    r.fake.with(|s| s.set_acx(false));
    r.at(15.0, |s| s.set_acx(true));
    r.run_to(21.0);
    assert_eq!(r.transitions()[0].0, 20000);
    assert_eq!(r.logged("audio endpoint is off (ACX disabled)"), 1);
    // Another process holds the session.
    let mut r = rig();
    r.fake.with(|s| s.attach_error = Some(170));
    r.at(5.0, |s| s.attach_error = None);
    r.run_to(11.0);
    assert_eq!(r.transitions()[0].0, 10000);
    assert_eq!(
        r.logged("AUDIO_ATTACH: DeviceIoControl failed: Win32 error 170"),
        1
    );
    // The driver does not know the remote address.
    let mut r = rig();
    r.fake.with(|s| s.addr_valid = false);
    r.run_to(11.0);
    assert_eq!(r.transitions().len(), 1, "never left wait-driver");
    assert_eq!(r.logged("does not know the buds' Bluetooth address"), 1);
    r.fake.with(|s| assert_eq!(s.probe_calls, 0));
}

#[test]
fn a_device_that_cannot_be_opened_is_retried() {
    let mut r = rig();
    r.fake.with(|s| s.info_error = Some(2));
    r.at(5.0, |s| s.info_error = None);
    r.run_to(11.0);
    assert_eq!(r.transitions()[0].0, 10000);
    assert_eq!(r.logged("cannot open the device"), 1);
    r.fake.with(|s| {
        assert_eq!(
            s.calls[..4],
            ["info", "reopen", "info", "attach"],
            "the handle is reopened when GET_INFO fails"
        );
    });
}

#[test]
fn set_link_failure_releases_the_stream() {
    let mut r = rig();
    r.fake.with(|s| s.set_link_error = Some(31));
    r.at(0.5, |s| s.set_link_error = None);
    r.run_to(2.0);
    assert_eq!(
        r.cmds(),
        ["SET_CONFIG", "OPEN", "CLOSE", "SET_CONFIG", "OPEN", "CLOSE"]
    );
    assert_eq!(r.links(), [(1000, true), (2000, false)]);
    assert_eq!(r.logged("AUDIO_SET_LINK: DeviceIoControl failed"), 1);
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

#[test]
fn caps_fallbacks_and_resampling_paths() {
    // (caps, endpoint format, config sent, packet bytes, resampled to)
    let cases: [(u8, PcmFormat, u8, usize, Option<u32>); 8] = [
        (0x3C, FMT_96K_24, 0x0C, 496, Some(48000)), // no UHQ: HiFi, resampled
        (0x3F, FMT_96K_24, 0x0C, 496, Some(48000)), // UHQ disabled by 0x01
        (0x38, FMT_48K_24, 0x18, 588, None),        // no HiFi: SSC 48k, dithered
        (0x30, FMT_48K_16, 0x10, 588, None),        // ... 16-bit straight in
        (0x30, FMT_96K_24, 0x10, 588, Some(48000)), // ... resampled
        (0x20, FMT_48K_24, 0x20, 588, Some(44100)), // 44.1 kHz only
        (0x28, FMT_96K_24, 0x28, 588, Some(44100)),
        (0x16, FMT_96K_24, 0x02, 672, None), // UHQ without 0x08 on offer
    ];
    for (caps, fmt, config, bytes, to) in cases {
        let mut r = rig();
        r.fake.with(|s| {
            s.sink.ssc_flags = caps;
            s.set_stream(Paused, fmt);
        });
        r.stream(1.0, Running, fmt).stream(1.5, Paused, fmt);
        r.run_to(2.0);
        let ctx = format!("caps {caps:#04x} {fmt:?}\n{}", r.log.text());
        assert_eq!(r.configs(), [config], "{ctx}");
        // 50 chunks of 10 ms reach the pipeline.
        let frames_in = u64::from(fmt.sample_rate) / 2;
        let frames = match to {
            Some(to) => stream_frames_out(frames_in, fmt.sample_rate, to),
            None => frames_in,
        };
        let p = r.packets();
        assert_eq!(p.len() as u64, frames / 864, "{ctx}");
        assert!(p.len() >= 25, "{ctx}");
        assert!(p.iter().all(|x| x.len() == bytes), "{ctx}");
        assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "START", "CLOSE"], "{ctx}");
    }
}

#[test]
fn caps_without_a_usable_mode_leave_the_link_off() {
    let mut r = rig();
    r.fake.with(|s| s.sink.ssc_flags = 0x00);
    r.stream(1.0, Running, FMT_48K_24);
    r.run_to(61.0);
    assert!(r.cmds().is_empty(), "nothing configured");
    assert_eq!(r.open_ms(), [0, 60000], "retried once a minute");
    assert!(r.links().iter().all(|(_, c)| !c), "the link flag stays off");
    let lines = r.log.find("cannot set up the stream");
    assert!(
        lines[0]
            .1
            .contains("SSC capability octet 0x00 offers none of UHQ"),
        "{}",
        lines[0].1
    );
    assert_eq!(r.transitions().len(), 2);
}

#[test]
fn no_ssc_endpoint_leaves_the_link_off() {
    let mut r = rig();
    r.fake.with(|s| s.sink.ssc_sep = false);
    r.run_to(30.0);
    assert!(r.cmds().is_empty());
    assert_eq!(r.open_ms(), [0]);
    assert!(r.links().iter().all(|(_, c)| !c));
    assert_eq!(
        r.logged("the sink has no SSC (vendor 0x75 / codec 0x0103) sink SEP; next attempt in 60 s"),
        1,
        "{}",
        r.log.text()
    );
}

#[test]
fn setup_failures_release_what_was_set_up() {
    // OPEN refused: ABORT (CLOSE is not valid before OPEN).
    let mut r = rig();
    r.fake.with(|s| s.sink.reject_open = Some(0x31));
    r.run_to(0.5);
    assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "ABORT"]);
    assert!(r.links().iter().all(|(_, c)| !c));
    // The media channel does not open: CLOSE.
    let mut r = rig();
    r.fake.with(|s| s.media_open_error = Some(121));
    r.run_to(0.5);
    assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "CLOSE"]);
    assert_eq!(r.logged("opening the media channel"), 1);
    // The sink aborts right after SET_CONFIGURATION: nothing to release.
    let mut r = rig();
    r.fake.with(|s| s.sink.abort_after_set_config = true);
    r.run_to(0.5);
    assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN"], "{}", r.log.text());
    assert_eq!(
        r.logged("cannot set up the stream: OPEN rejected: BAD_STATE (0x31) (sink sent ABORT)"),
        1
    );
    // The signaling channel does not open at all.
    let mut r = rig();
    r.fake.with(|s| s.open_error = Some(1225));
    r.run_to(0.5);
    assert!(r.cmds().is_empty());
    assert_eq!(r.logged("opening the signaling channel"), 1);
    // The sink never answers.
    let mut r = rig();
    r.fake.with(|s| s.sink.silent = true);
    r.run_to(8.0);
    assert_eq!(r.logged("capability probe: timed out"), 1);
    assert_eq!(r.open_ms(), [0, 6000], "5 s response timeout + 1 s backoff");
}

#[test]
fn media_mtu_limits_the_bitrate() {
    // A 600-byte media MTU: UHQ at 584 kbps (672 bytes) does not fit.
    let mut r = rig();
    r.fake.with(|s| {
        s.report_mtus = Some((600, 65535));
        s.set_stream(Paused, FMT_96K_24);
    });
    r.stream(1.0, Running, FMT_96K_24);
    r.run_to(1.2);
    assert_eq!(
        r.logged("584 kbps does not fit the media MTU limit 600; using 442 kbps"),
        1
    );
    assert!(r.packets().iter().all(|p| p.len() == 512));
    assert!(!r.packets().is_empty());
    // Nothing fits: no link.
    let mut r = rig();
    r.fake.with(|s| s.report_mtus = Some((60, 65535)));
    r.run_to(0.5);
    assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "CLOSE"]);
    assert_eq!(r.logged("exceed the media MTU limit 60"), 1);
    // No plausible MTU reported at all.
    let mut r = rig();
    r.fake.with(|s| s.report_mtus = Some((0, 65535)));
    r.run_to(0.5);
    assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "CLOSE"]);
    assert_eq!(r.logged("reported no plausible MTU"), 1);
    // The live values (1021 echoed, 65535): limit 1005.
    let mut r = rig();
    r.run_to(0.5);
    assert_eq!(
        r.logged("media channel open: out_mtu=1021 in_mtu=65535, packet 496 bytes, limit 1005"),
        1
    );
}

// ---------------------------------------------------------------------------
// Audio details
// ---------------------------------------------------------------------------

#[test]
fn discontinuity_flushes_the_partial_frame() {
    let mut r = rig();
    r.stream(1.0, Running, FMT_48K_24)
        .at(1.305, |s| s.drop_frames(1000))
        .stream(2.0, Paused, FMT_48K_24);
    r.run_to(2.5);
    // 30 chunks (14400 frames) -> 16 packets and 576 frames that are dropped
    // at the gap; then 70 chunks (33600 frames) -> 38 packets. Without the
    // flush it would be 55.
    assert_eq!(r.packets().len(), 16 + 38);
    assert_eq!(
        r.logged("discontinuity before frame 15400"),
        1,
        "{}",
        r.log.text()
    );
    // The RTP sequence does not restart.
    assert_eq!(
        seq_and_len(&r.packets()),
        (0..54).map(|k| (k, 496)).collect::<Vec<_>>()
    );
}

#[test]
fn statistics_are_logged_periodically_while_streaming() {
    let mut r = rig();
    r.s.set_stats_period(Duration::from_millis(200));
    r.fake.with(|s| s.write_delay = Duration::from_micros(150));
    r.stream(1.0, Running, FMT_48K_24)
        .at(1.105, |s| s.drop_frames(1000))
        .stream(1.5, Paused, FMT_48K_24);
    r.run_to(1.7);
    let stats = r.log.find("stats:");
    assert!(stats.len() >= 2, "{}", r.log.text());
    // First interval: 20 chunks in, 10 packets out (5 before the gap, 5
    // after), 1000 frames the driver dropped, one discontinuity; each WRITE
    // took 150 us.
    assert!(stats[0].0 >= 1200 && stats[0].0 < 1210, "{stats:?}");
    assert!(
        stats[0].1.contains(
            "stats: packets=10 frames_in=9600 dropped=1000 discontinuities=1 write avg=150us max=150us"
        ),
        "{}",
        stats[0].1
    );
    assert!(
        stats[1]
            .1
            .contains("frames_in=9600 dropped=0 discontinuities=0"),
        "{}",
        stats[1].1
    );
}

#[test]
fn a_backlog_in_the_ring_is_skipped_when_the_stream_restarts() {
    let mut r = rig();
    // The sink suspends at 1.5; audio keeps running, so the driver's ring
    // fills up (16 chunks) with audio from 1.51 on and then drops. START is
    // accepted again at 3.5: the ring's content is two seconds old by then.
    r.stream(1.0, Running, FMT_48K_24)
        .at(1.5, |s| s.sink_suspends());
    r.run_to(3.58);
    r.fake.with(|s| {
        assert!(s.state.frames_dropped > 0, "the driver counted the loss");
        let after: Vec<u128> = s
            .packet_times
            .iter()
            .map(|t| t.as_millis())
            .filter(|t| *t > 1500)
            .collect();
        // 7 fresh chunks after the restart at 3.5 = 3360 frames = 3 packets,
        // the first when the second fresh chunk completes a frame. With the
        // backlog in front there would be 8 more, all at 3.5.
        assert_eq!(after, [3520, 3540, 3560]);
    });
    assert_eq!(r.packets().len(), 27 + 3);
    assert_eq!(
        r.logged("skipped 7680 frame(s) rendered before START"),
        1,
        "{}",
        r.log.text()
    );
}

#[test]
fn a_backlog_in_another_format_is_skipped_without_reconfiguring() {
    let mut r = rig();
    // Suspended by the sink at 1.5 with 48 kHz audio piling up in the ring;
    // at 2.0 the endpoint switches to 96 kHz. The stream is reconfigured
    // once, for the new format. The old 48 kHz chunks are still in the ring
    // when it starts: they must neither be sent nor taken for a format
    // change back.
    r.stream(1.0, Running, FMT_48K_24)
        .at(1.5, |s| s.sink_suspends())
        .stream(2.0, Running, FMT_96K_24)
        .stream(2.5, Paused, FMT_96K_24);
    r.run_to(3.0);
    assert_eq!(r.configs(), [0x0C, 0x0A], "{}", r.log.text());
    assert_eq!(
        r.cmds(),
        [
            "SET_CONFIG",
            "OPEN",
            "START",
            "CLOSE",
            "SET_CONFIG",
            "OPEN",
            "START",
            "CLOSE"
        ]
    );
    assert_eq!(r.logged("skipped 7680 frame(s) rendered before START"), 1);
    let got = seq_and_len(&r.packets());
    let want: Vec<(u16, usize)> = (0..27)
        .map(|k| (k, 496))
        .chain((0..55).map(|k| (k, 672)))
        .collect();
    assert_eq!(got, want);
}

#[test]
fn audio_rendered_during_the_start_exchange_is_kept() {
    let mut r = rig();
    // Every signaling exchange takes 60 ms from 0.9 on. The endpoint starts
    // rendering at 1.0; by the time START is accepted (1.06) six chunks sit
    // in the driver's ring. They are the beginning of the audio: sent.
    r.at(0.9, |s| s.request_delay = Duration::from_millis(60))
        .stream(1.0, Running, FMT_48K_24)
        .stream(1.5, Paused, FMT_48K_24);
    r.run_to(2.0);
    r.fake.with(|s| {
        let t: Vec<u128> = s.packet_times.iter().map(|t| t.as_millis()).collect();
        // 6 chunks = 2880 frames = 3 packets at once, then one per frame.
        assert_eq!(t[..4], [1060, 1060, 1060, 1080], "{t:?}");
    });
    // Nothing of the 0.5 s was lost: 24000 frames = 27 packets.
    assert_eq!(r.packets().len(), 27);
    assert_eq!(r.logged("rendered before START"), 0);
    assert_eq!(r.transitions()[2], (1060, "ready -> streaming".to_string()));
}

#[test]
fn a_chunk_that_straddles_the_start_position_is_trimmed() {
    let mut r = rig();
    // The endpoint is seen to start rendering at frame 100, and the ring
    // holds a chunk of frames 0..480 from just before: its first 100 frames
    // predate the run, the other 380 belong to it.
    r.fake.with(|s| s.set_stream(Paused, FMT_48K_24));
    r.at(0.5, |s| s.state.frames_rendered = 100)
        .at(1.0, |s| {
            s.push_pcm(0, 480);
            s.set_stream(Running, FMT_48K_24);
        })
        .stream(1.5, Paused, FMT_48K_24);
    r.run_to(5.0);
    assert_eq!(
        r.logged("skipped 100 frame(s) rendered before START"),
        1,
        "{}",
        r.log.text()
    );
    // 380 frames of that chunk + 50 chunks of 480.
    let stats = r.log.find("stats:");
    assert!(stats[0].1.contains("frames_in=24380 "), "{}", stats[0].1);
    // With 380 frames in hand the first frame is complete after two more
    // chunks (it would be after one if the whole chunk had been kept).
    r.fake
        .with(|s| assert_eq!(s.packet_times[0].as_millis(), 1020));
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

#[test]
fn shutdown_mid_stream_closes_the_stream_and_clears_the_link() {
    let mut r = rig();
    r.stream(1.0, Running, FMT_48K_24);
    r.run_to(1.5);
    assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "START", "CLOSE"]);
    r.fake.with(|s| {
        assert!(!s.sink.opened && !s.sink.started && s.sink.configured.is_none());
        assert!(!s.link() && !s.attached());
        assert_eq!((s.signaling, s.media), (None, None));
        let n = s.calls.len();
        assert_eq!(
            s.calls[n - 4..],
            [
                "close(media)",
                "close(signaling)",
                "set_link(false)",
                "detach"
            ],
            "CLOSE, channels, link flag off, ATTACH cancelled"
        );
    });
    assert_eq!(r.transitions().last().unwrap().1, "streaming -> stopped");
    assert_eq!(r.logged("stream released with CLOSE"), 1);
    // A second run does nothing.
    r.s.step();
    r.s.run();
    assert_eq!(r.s.phase(), Phase::Stopped);
}

#[test]
fn shutdown_in_every_other_state_is_clean() {
    // Before the driver is there.
    let mut r = rig();
    r.fake.with(|s| s.protocol_version = 1);
    r.run_to(3.0);
    r.fake
        .with(|s| assert!(!s.calls.iter().any(|c| c.starts_with("set_link"))));
    // Waiting for the buds.
    let mut r = rig();
    r.fake.with(|s| s.connected = false);
    r.run_to(3.0);
    // Stop requested before the first step: nothing is touched but detach.
    let mut r = rig();
    r.fake.stop().request();
    r.s.run();
    r.fake.with(|s| assert_eq!(s.calls, ["detach"]));
    // CLOSE refused at shutdown: ABORT.
    let mut r = rig();
    r.fake.with(|s| s.sink.reject_close = true);
    r.run_to(1.0);
    assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "CLOSE", "ABORT"]);
    assert_eq!(r.logged("stream released with ABORT"), 1);
}

// ---------------------------------------------------------------------------
// Everything at once
// ---------------------------------------------------------------------------

/// Random events for two virtual minutes: whatever the order, the session
/// neither panics nor spins, and a stop always ends clean.
#[test]
fn random_event_storms_never_panic_and_always_stop_clean() {
    for seed in [1u32, 2, 3, 4, 5, 6] {
        let mut r = rig();
        let mut x = seed.wrapping_mul(2_654_435_761).max(1);
        let mut rnd = move |n: u32| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x % n
        };
        let fmts = [FMT_48K_24, FMT_96K_24, FMT_48K_16];
        let mut t = 0.0;
        while t < 120.0 {
            t += 0.05 + f64::from(rnd(3000)) / 1000.0;
            let fmt = fmts[rnd(3) as usize];
            match rnd(17) {
                0..=3 => {
                    // A short burst of audio.
                    r.stream(t, Running, fmt);
                    r.stream(t + 0.05 + f64::from(rnd(200)) / 1000.0, Paused, fmt);
                }
                4 => {
                    r.stream(t, StreamState::None, fmt);
                }
                5 => {
                    r.at(t, |s| s.sink_closes());
                }
                6 => {
                    r.at(t, |s| s.sink_aborts());
                }
                7 => {
                    r.at(t, |s| s.sink_suspends());
                }
                8 => {
                    r.at(t, |s| s.buds_disconnect());
                    r.at(t + f64::from(rnd(8000)) / 1000.0, |s| s.buds_connect());
                }
                9 => {
                    r.at(t, |s| s.force_detach(995));
                }
                10 => {
                    let code = [None, Some(0x31)][rnd(2) as usize];
                    r.at(t, move |s| s.sink.reject_start = code);
                }
                11 => {
                    let caps = [0x3E, 0x3C, 0x30, 0x20, 0x00, 0x3F][rnd(6) as usize];
                    r.at(t, move |s| s.sink.ssc_flags = caps);
                }
                12 => {
                    r.at(t, |s| s.write_error = Some((s.packets.len() + 3, 31)));
                    r.at(t + 2.0, |s| s.write_error = None);
                }
                13 => {
                    r.at(t, |s| s.sink_reports_delay(2020));
                }
                14 => {
                    let code = [None, Some(0x29)][rnd(2) as usize];
                    r.at(t, move |s| s.sink.reject_set_configuration = code);
                }
                15 => {
                    // Only an open stream can be started by the sink.
                    r.at(t, |s| {
                        if s.sink.opened {
                            s.sink_starts();
                        }
                    });
                }
                _ => {
                    r.at(t, |s| s.drop_frames(777));
                }
            }
        }
        r.run_to(125.0 + f64::from(rnd(5000)) / 1000.0);
        // Errors stayed rate-limited however bad it got.
        let errors = r
            .log
            .lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.1 == Level::Error)
            .count();
        assert!(errors < 60, "seed {seed}: {errors} error lines");
    }
}

// ---------------------------------------------------------------------------
// Volume (protocol 3)
// ---------------------------------------------------------------------------

mod volume {
    use super::*;
    use crate::volume::VolumeMode;
    use crossc_ioctl::audio::{
        AUDIO_FLAG_AVRCP, AUDIO_FLAG_HW_VOLUME, AUDIO_FLAG_MUTED, VolumeState,
    };

    const HW: u32 = AUDIO_FLAG_HW_VOLUME;
    const BUDS: u32 = AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP;

    fn db(x: f64) -> i32 {
        (x * 65536.0).round() as i32
    }

    fn vol(level: f64) -> VolumeState {
        VolumeState {
            level: db(level),
            left: db(level),
            right: db(level),
            avrcp: 64,
            ..VolumeState::default()
        }
    }

    /// 2 s of 48 kHz/24 audio from t = 1 s, after `setup`; returns the rig.
    fn run(setup: impl FnOnce(&Rig)) -> Rig {
        let mut r = rig();
        setup(&r);
        r.stream(1.0, Running, FMT_48K_24)
            .stream(3.0, Paused, FMT_48K_24);
        r.run_to(7.0);
        assert_eq!(r.packets().len(), 111, "{}", r.log.text());
        r
    }

    fn baseline() -> Vec<Vec<u8>> {
        run(|_| {}).packets()
    }

    #[test]
    fn buds_mode_streams_bit_exact() {
        let base = baseline();
        // AVRCP active at -20 dB: the buds apply it, the PCM is untouched.
        let r = run(|r| {
            r.fake.with(|s| s.set_volume(BUDS, vol(-20.0)));
        });
        assert_eq!(r.packets(), base);
        assert_eq!(r.s.volume().mode, VolumeMode::Buds);
        assert_eq!(r.s.volume().gain, [1.0, 1.0]);
        assert_eq!(
            r.logged("volume: the buds apply the volume"),
            1,
            "{}",
            r.log.text()
        );
        // Right after a buds report: exact buds level, channels on the
        // 0.5 dB grid. Still untouched.
        let r = run(|r| {
            r.fake.with(|s| {
                s.set_volume(
                    BUDS,
                    VolumeState {
                        level: -667_751,
                        left: db(-10.5),
                        right: db(-10.5),
                        ..VolumeState::default()
                    },
                )
            });
        });
        assert_eq!(r.packets(), base);
        // No hardware volume (v2 driver, kill switch): bit-exact as well,
        // whatever the volume bytes hold.
        let r = run(|r| {
            r.fake.with(|s| s.set_volume(0, vol(-20.0)));
        });
        assert_eq!(r.packets(), base);
        assert_eq!(r.s.volume().mode, VolumeMode::Windows);
    }

    #[test]
    fn software_gain_without_a_subscriber() {
        let base = baseline();
        let r = run(|r| {
            r.fake.with(|s| s.set_volume(HW, vol(-20.0)));
        });
        assert_eq!(r.s.volume().mode, VolumeMode::Software);
        let g = r.s.volume().gain;
        assert!(
            (g[0] - 0.1).abs() < 1e-9 && (g[1] - 0.1).abs() < 1e-9,
            "{g:?}"
        );
        let p = r.packets();
        assert_eq!(p.len(), base.len());
        assert!(
            p.iter().zip(&base).all(|(a, b)| a != b),
            "every packet is scaled"
        );
        assert_eq!(r.logged("volume: software gain"), 1, "{}", r.log.text());
        // A slider move while streaming: logged at debug level, applied.
        let r = run(|r| {
            r.fake.with(|s| s.set_volume(HW, vol(-20.0)));
            r.at(2.0, |s| s.set_volume(HW, vol(-6.0)));
        });
        assert!((r.s.volume().gain[0] - crate::volume::level_to_gain(db(-6.0))).abs() < 1e-12);
        assert_eq!(r.logged("volume: software gain"), 1);
    }

    #[test]
    fn switching_modes_mid_stream() {
        let base = baseline();
        let r = run(|r| {
            r.fake.with(|s| s.set_volume(BUDS, vol(-20.0)));
            // The subscriber goes (AVRCP lost), then comes back.
            r.at(2.0, |s| s.set_volume(HW, vol(-20.0)));
            r.at(2.5, |s| s.set_volume(BUDS, vol(-20.0)));
        });
        let p = r.packets();
        // Until the switch the stream is the unscaled one (packet 55 holds
        // frames from both sides of t = 2 s).
        assert_eq!(p[..54], base[..54]);
        // Scaled from shortly after (the 20 ms ramp) until the switch back.
        for k in 57..80 {
            assert_ne!(p[k], base[k], "packet {k}");
        }
        assert_eq!(r.s.volume().mode, VolumeMode::Buds);
        let modes: Vec<String> = r
            .log
            .find("volume: ")
            .into_iter()
            .map(|(t, m)| format!("{t} {}", m.split("volume: ").nth(1).unwrap_or("")))
            .collect();
        assert_eq!(modes.len(), 3, "{modes:?}");
        assert!(modes[0].starts_with("0 the buds apply"), "{modes:?}");
        assert!(modes[1].starts_with("2000 software gain"), "{modes:?}");
        assert!(modes[2].starts_with("2500 the buds apply"), "{modes:?}");
    }

    #[test]
    fn mute_silences_in_both_hardware_modes() {
        for flags in [HW, BUDS] {
            let r = run(|r| {
                r.fake
                    .with(move |s| s.set_volume(flags | AUDIO_FLAG_MUTED, vol(-3.0)));
            });
            assert_eq!(r.s.volume().gain, [0.0, 0.0]);
            assert!(r.logged(", muted") >= 1, "{}", r.log.text());
        }
        // A muted stream's packets are all the same encoded silence after
        // the pipeline starts muted (no ramp from unity at creation).
        let r = run(|r| {
            r.fake
                .with(|s| s.set_volume(BUDS | AUDIO_FLAG_MUTED, vol(0.0)));
        });
        let p = r.packets();
        let silent = run(|r| {
            r.fake.with(|s| {
                s.set_volume(
                    HW,
                    VolumeState {
                        level: crossc_ioctl::audio::VOLUME_LEVEL_MIN,
                        left: crossc_ioctl::audio::VOLUME_LEVEL_MIN,
                        right: crossc_ioctl::audio::VOLUME_LEVEL_MIN,
                        ..VolumeState::default()
                    },
                )
            });
        })
        .packets();
        assert_eq!(p, silent, "mute == slider at the bottom == silence");
    }
}
