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
        assert_eq!(pkt.len(), 12 + 576);
        let (h, _) = RtpHeader::decode(pkt).unwrap();
        assert_eq!(
            (h.sequence as usize, h.timestamp as usize, h.payload_type),
            (k, k, 96)
        );
        assert_eq!((pkt[12], pkt[15]), (0xFF, Bitrate::K229.code()));
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
    assert!(r.logged("link up: HiFi 48 kHz/24: config 0x0c, 229 kbps") == 1);
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
    assert!(p.iter().all(|x| x.len() == 588));
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
        .map(|k| (k, 588))
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
    assert_eq!(got, (0..54).map(|k| (k, 588)).collect::<Vec<_>>());
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
    assert_eq!(got, (0..82).map(|k| (k, 588)).collect::<Vec<_>>());
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
        (0x3C, FMT_96K_24, 0x0C, 588, Some(48000)), // no UHQ: HiFi, resampled
        (0x3F, FMT_96K_24, 0x0C, 588, Some(48000)), // UHQ disabled by 0x01
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
        r.logged("584 kbps does not fit the media MTU limit 600; using 291 kbps"),
        1
    );
    assert!(r.packets().iter().all(|p| p.len() == 342));
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
        r.logged("media channel open: out_mtu=1021 in_mtu=65535, packet 588 bytes, limit 1005"),
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
        (0..54).map(|k| (k, 588)).collect::<Vec<_>>()
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
        .map(|k| (k, 588))
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

// ---------------------------------------------------------------------------
// Adaptive bitrate
// ---------------------------------------------------------------------------

mod adaptive {
    use super::*;
    use crate::abr::{DOWN_SETTLE, UP_HOLD_MIN};

    /// A healthy WRITE.
    const FAST: Duration = Duration::from_micros(200);

    /// The ladder most of these tests were written for: HiFi under a
    /// configured 192 kbps (192 / 128 / 88). The default top, 229 kbps, has
    /// its own tests at the end.
    fn top192() -> Settings {
        Settings {
            hifi_bitrate: Bitrate::K192,
            ..Settings::default()
        }
    }

    fn fast_rig(settings: Settings) -> Rig {
        let r = rig_with(settings);
        r.fake.with(|s| s.write_delay = FAST);
        r
    }

    /// Bitrate changes logged, as (ms, from kbps, to kbps).
    fn changes(r: &Rig) -> Vec<(u64, u32, u32)> {
        r.log
            .find("bitrate ")
            .into_iter()
            .filter_map(|(t, m)| {
                let m = m.split("bitrate ").nth(1)?;
                let mut w = m.split_whitespace();
                let from = w.next()?.parse().ok()?;
                let to = w.nth(1)?.parse().ok()?;
                Some((t, from, to))
            })
            .collect()
    }

    /// What every packet is: (sequence, kbps, frames). Checks that each is
    /// a whole number (1 or 2) of SSC frames of one rate, of the size that
    /// rate has, and that the RTP timestamp equals the sequence number.
    fn dissect(packets: &[Vec<u8>]) -> Vec<(u16, u32, usize)> {
        packets
            .iter()
            .enumerate()
            .map(|(k, p)| {
                let (h, off) = RtpHeader::decode(p).unwrap();
                assert_eq!((off, h.payload_type), (12, 96), "packet {k}");
                assert_eq!(h.timestamp, u32::from(h.sequence), "packet {k}");
                let code = p[15];
                let rate = if code & 0x1F < 0x10 {
                    Bitrate::from_code(0xE0 | (code & 0x1F))
                } else {
                    Bitrate::from_code(code)
                }
                .unwrap_or_else(|| panic!("packet {k}: rate code {code:#04x}"));
                let mode = if rate.is_e_family() {
                    ssc_enc::Mode::Uhq96k
                } else {
                    ssc_enc::Mode::Hifi48k
                };
                let fb = ssc_enc::frame_size(mode, rate);
                let payload = &p[12..];
                assert_eq!(payload.len() % fb, 0, "packet {k}: {} bytes", p.len());
                let frames = payload.len() / fb;
                assert!((1..=2).contains(&frames), "packet {k}: {frames} frames");
                for f in payload.chunks(fb) {
                    assert_eq!(f[0], 0xFF, "packet {k}: sync byte");
                    assert_eq!(f[3] & 0x1F, code & 0x1F, "packet {k}: one rate");
                }
                assert!(p.len() <= 1005, "packet {k}");
                (h.sequence, rate.bps() / 1000, frames)
            })
            .collect()
    }

    /// The (kbps, frames per packet) the stream went through, in order.
    fn rate_runs(d: &[(u16, u32, usize)]) -> Vec<(u32, usize)> {
        let mut runs: Vec<(u32, usize)> = Vec::new();
        for &(_, kbps, frames) in d {
            if runs.last() != Some(&(kbps, frames)) {
                runs.push((kbps, frames));
            }
        }
        runs
    }

    fn assert_continuous(d: &[(u16, u32, usize)]) {
        for (k, x) in d.iter().enumerate() {
            assert_eq!(usize::from(x.0), k, "RTP sequence at packet {k}");
        }
    }

    #[test]
    fn a_good_link_stays_at_the_configured_rate() {
        for (settings, fmt, kbps, bytes, ladder) in [
            (top192(), FMT_48K_24, 192, 496, "192 / 128 / 88 kbps"),
            (
                Settings::default(),
                FMT_48K_24,
                229,
                588,
                "229 / 192 / 128 / 88 kbps",
            ),
            (
                Settings::default(),
                FMT_96K_24,
                584,
                672,
                "584 / 291 (2 frames/packet) / 152 (2 frames/packet) kbps",
            ),
        ] {
            let mut r = fast_rig(settings);
            r.s.set_stats_period(Duration::from_secs(10));
            r.fake.with(|s| s.set_stream(Paused, fmt));
            r.stream(1.0, Running, fmt);
            r.run_to(31.5);
            assert!(changes(&r).is_empty(), "{}", r.log.text());
            let d = dissect(&r.packets());
            assert_continuous(&d);
            assert_eq!(rate_runs(&d), [(kbps, 1)]);
            assert!(r.packets().iter().all(|p| p.len() == bytes));
            assert_eq!(r.logged(&format!("adaptive bitrate: {ladder}")), 1);
            let stats = r.log.find("stats:");
            assert!(stats.len() >= 3);
            for (_, line) in &stats[..3] {
                assert!(
                    line.contains(&format!("rate={kbps}kbps at[{kbps}k=10.0s "))
                        && line.contains("rate_changes=0 slow_writes=0 late_max=0ms"),
                    "{line}"
                );
            }
        }
    }

    #[test]
    fn sustained_write_latency_steps_down_and_recovery_steps_up() {
        let mut r = fast_rig(top192());
        r.s.set_stats_period(Duration::from_secs(5));
        // Every WRITE takes 20 ms from 3.0 to 4.0 s: more than a packet
        // plays (18 ms).
        r.stream(1.0, Running, FMT_48K_24)
            .at(3.0, |s| s.write_delay = Duration::from_millis(20))
            .at(4.0, |s| s.write_delay = FAST);
        r.run_to(30.0);
        let c = changes(&r);
        let steps: Vec<(u32, u32)> = c.iter().map(|x| (x.1, x.2)).collect();
        assert_eq!(
            steps,
            [(192, 128), (128, 88), (88, 128), (128, 192)],
            "{}",
            r.log.text()
        );
        // Down within 200 ms of the first slow WRITE, and again once the
        // new rate had its chance.
        assert!((3060..=3200).contains(&c[0].0), "{c:?}");
        let settle = DOWN_SETTLE.as_millis() as u64;
        assert!((c[0].0 + settle..=c[0].0 + 500).contains(&c[1].0), "{c:?}");
        // Up one rung per 10 s of clean audio after the link recovered (the
        // backlog of the slow second is caught up first).
        let hold = UP_HOLD_MIN.as_millis() as u64;
        assert!((3900 + hold..=4300 + hold).contains(&c[2].0), "{c:?}");
        assert!(
            (c[2].0 + hold..=c[2].0 + hold + 50).contains(&c[3].0),
            "{c:?}"
        );
        assert_eq!(
            r.logged("bitrate 192 -> 128 kbps: link congested (WRITE blocked"),
            1
        );
        assert_eq!(r.logged("; 336-byte packets"), 2);
        assert_eq!(r.logged("bitrate 88 -> 128 kbps: link clean for 10 s"), 1);
        // The stream itself: one RTP sequence through every change, every
        // packet one frame of the size its rate has.
        let d = dissect(&r.packets());
        assert_continuous(&d);
        assert_eq!(
            rate_runs(&d),
            [(192, 1), (128, 1), (88, 1), (128, 1), (192, 1)]
        );
        let sizes: Vec<usize> = r.packets().iter().map(Vec::len).collect();
        assert!(sizes.iter().all(|n| [496, 336, 236].contains(n)));
        // No audio was lost on the way: the slow second fits the ring.
        assert_eq!(r.fake.with(|s| s.state.frames_dropped), 0);
        assert_eq!(r.logged("link down"), 0);
        // The statistics say where the time went.
        let stats = r.log.find("stats:");
        assert!(
            stats[0].1.contains("rate=88kbps at[192k=2.")
                && stats[0].1.contains("rate_changes=2 slow_writes="),
            "{}",
            stats[0].1
        );
        assert!(
            stats[1].1.contains(
                "rate=88kbps at[192k=0.0s 128k=0.0s 88k=5.0s] rate_changes=0 slow_writes=0"
            ),
            "{}",
            stats[1].1
        );
        assert_eq!(r.s.rate(), None, "stopped");
    }

    #[test]
    fn a_link_too_small_for_the_top_rate_is_probed_ever_more_rarely() {
        // The link carries 170 kbps from 5 s on: HiFi at 192 kbps is 220
        // on the wire, at 128 kbps it is 149.
        let mut r = fast_rig(top192());
        r.stream(1.0, Running, FMT_48K_24)
            .at(5.0, |s| s.link_bps = Some(170_000));
        r.run_to(85.0);
        let c = changes(&r);
        let steps: Vec<(u32, u32)> = c.iter().map(|x| (x.1, x.2)).collect();
        assert!(steps.len() >= 7, "{c:?}");
        for (k, s) in steps.iter().enumerate() {
            let want = if k % 2 == 0 { (192, 128) } else { (128, 192) };
            assert_eq!(*s, want, "{c:?}");
        }
        // Down about a second after the link shrank (its buffer has to
        // fill first), and within a second of every probe.
        assert!((5200..=6500).contains(&c[0].0), "{c:?}");
        let secs = |a: u64, b: u64| ((b - a) as f64 / 1000.0).round() as u64;
        let holds: Vec<u64> = (1..c.len())
            .step_by(2)
            .map(|k| secs(c[k - 1].0, c[k].0))
            .collect();
        assert_eq!(&holds[..3], [10, 20, 40], "{c:?}");
        for k in (2..c.len()).step_by(2) {
            assert!(c[k].0 - c[k - 1].0 <= 1500, "{c:?}");
        }
        // Never more than a few changes in any minute.
        for (i, x) in c.iter().enumerate() {
            let n = c[i..].iter().take_while(|y| y.0 < x.0 + 60_000).count();
            assert!(n <= 6, "{n} changes in the minute from {} ms: {c:?}", x.0);
        }
        // The probes cost no audio.
        assert_eq!(r.fake.with(|s| s.state.frames_dropped), 0);
        let d = dissect(&r.packets());
        assert_continuous(&d);
        assert!(d.iter().all(|x| x.1 >= 128), "128 kbps was enough");
    }

    #[test]
    fn uhq_steps_down_to_two_frames_per_packet() {
        // 400 kbps of link: UHQ at 584 kbps does not fit, 291 kbps does.
        let mut r = fast_rig(top192());
        r.fake.with(|s| s.set_stream(Paused, FMT_96K_24));
        r.stream(1.0, Running, FMT_96K_24)
            .at(3.0, |s| s.link_bps = Some(400_000))
            .at(6.0, |s| s.link_bps = None);
        r.run_to(20.0);
        let c = changes(&r);
        let steps: Vec<(u32, u32)> = c.iter().map(|x| (x.1, x.2)).collect();
        assert_eq!(steps, [(584, 291), (291, 584)], "{}", r.log.text());
        assert!((3050..=3600).contains(&c[0].0), "{c:?}");
        assert_eq!(
            r.logged("bitrate 584 -> 291 kbps, 2 frame(s)/packet: link congested"),
            1
        );
        assert_eq!(
            r.logged("bitrate 291 -> 584 kbps, 1 frame(s)/packet: link clean"),
            1
        );
        let d = dissect(&r.packets());
        assert_continuous(&d);
        assert_eq!(rate_runs(&d), [(584, 1), (291, 2), (584, 1)]);
        // 672 bytes either way: one frame of 660, or two of 330.
        assert!(r.packets().iter().all(|p| p.len() == 672));
        // Half as many packets while two frames share one.
        let two = d.iter().filter(|x| x.2 == 2).count();
        assert!((540..=600).contains(&two), "{two}");
        assert_eq!(r.fake.with(|s| s.state.frames_dropped), 0);
        assert_eq!(r.configs(), [0x0A], "no reconfiguration for a rate");
    }

    #[test]
    fn single_stalls_change_nothing() {
        // Under the phones' top rate and under the default one.
        for (settings, kbps) in [(top192(), 192), (Settings::default(), 229)] {
            let mut r = fast_rig(settings);
            // One WRITE of 30, 80 and 150 ms, seconds apart.
            r.fake.with(|s| {
                s.write_stalls = vec![
                    (100, Duration::from_millis(30)),
                    (400, Duration::from_millis(80)),
                    (700, Duration::from_millis(150)),
                ]
            });
            r.s.set_stats_period(Duration::from_secs(30));
            r.stream(1.0, Running, FMT_48K_24);
            r.run_to(31.5);
            assert!(changes(&r).is_empty(), "{}", r.log.text());
            let d = dissect(&r.packets());
            assert_continuous(&d);
            assert_eq!(rate_runs(&d), [(kbps, 1)]);
            let stats = r.log.find("stats:");
            assert!(
                stats[0].1.contains("max=150200us")
                    && stats[0].1.contains(&format!("rate={kbps}kbps"))
                    && stats[0]
                        .1
                        .contains("rate_changes=0 slow_writes=3 late_max=1"),
                "{}",
                stats[0].1
            );
        }
    }

    #[test]
    fn a_stall_that_loses_audio_steps_down_once() {
        // 300 ms in one WRITE: more than the driver keeps for a slow
        // reader, so audio is dropped and the next chunk says so.
        let mut r = fast_rig(top192());
        r.fake
            .with(|s| s.write_stalls = vec![(100, Duration::from_millis(300))]);
        r.stream(1.0, Running, FMT_48K_24);
        r.run_to(8.0);
        let c = changes(&r);
        assert_eq!(c.len(), 1, "{}", r.log.text());
        assert_eq!((c[0].1, c[0].2), (192, 128));
        assert_eq!(
            r.logged("bitrate 192 -> 128 kbps: audio was dropped while WRITEs blocked"),
            1
        );
        assert!(r.fake.with(|s| s.state.frames_dropped) > 0);
        // A discontinuity without a slow WRITE is not the link's doing.
        let mut r = fast_rig(top192());
        r.stream(1.0, Running, FMT_48K_24)
            .at(2.005, |s| s.drop_frames(5000))
            .at(3.005, |s| s.drop_frames(5000));
        r.run_to(8.0);
        assert!(changes(&r).is_empty(), "{}", r.log.text());
    }

    #[test]
    fn a_backlog_without_slow_writes_does_not_step_down_but_delays_the_step_up() {
        // The reader does not get to run for 120 ms, every 4 s.
        let mut r = fast_rig(top192());
        r.s.set_stats_period(Duration::from_secs(30));
        r.stream(1.0, Running, FMT_48K_24);
        for k in 0..12 {
            r.at(2.0 + 4.0 * f64::from(k), |s| {
                s.wait_stall = Duration::from_millis(120)
            });
        }
        r.run_to(32.0);
        assert!(changes(&r).is_empty(), "{}", r.log.text());
        let stats = r.log.find("stats:");
        assert!(
            stats[0].1.contains("slow_writes=0 late_max=1"),
            "{}",
            stats[0].1
        );
        // The same while the rate is down: no step up before the sender
        // keeps up for 10 s.
        let mut r = fast_rig(top192());
        r.stream(1.0, Running, FMT_48K_24)
            .at(3.0, |s| s.write_delay = Duration::from_millis(20))
            .at(3.3, |s| s.write_delay = FAST);
        for k in 0..6 {
            r.at(5.0 + 6.0 * f64::from(k), |s| {
                s.wait_stall = Duration::from_millis(120)
            });
        }
        r.run_to(60.0);
        let c = changes(&r);
        let steps: Vec<(u32, u32)> = c.iter().map(|x| (x.1, x.2)).collect();
        assert_eq!(steps, [(192, 128), (128, 192)], "{}", r.log.text());
        // The last stall is at 35 s.
        assert!((45_000..=45_400).contains(&c[1].0), "{c:?}");
    }

    #[test]
    fn a_mode_switch_starts_the_controller_anew() {
        let mut r = fast_rig(top192());
        r.stream(1.0, Running, FMT_48K_24)
            .at(2.0, |s| s.write_delay = Duration::from_millis(20))
            .at(3.0, |s| s.write_delay = FAST)
            .stream(4.0, Running, FMT_96K_24)
            .stream(6.0, Running, FMT_48K_24);
        r.run_to(8.0);
        let c = changes(&r);
        let steps: Vec<(u32, u32)> = c.iter().map(|x| (x.1, x.2)).collect();
        assert_eq!(steps, [(192, 128), (128, 88)], "{}", r.log.text());
        assert_eq!(r.configs(), [0x0C, 0x0A, 0x0C]);
        assert_eq!(r.logged("adaptive bitrate: 192 / 128 / 88 kbps"), 2);
        assert_eq!(r.logged("adaptive bitrate: 584 / 291"), 1);
        // HiFi down to 88, UHQ from its top, HiFi from its top again. Each
        // stream configuration has its own RTP sequence.
        let d = dissect(&r.packets());
        assert_eq!(
            rate_runs(&d),
            [(192, 1), (128, 1), (88, 1), (584, 1), (192, 1)]
        );
        let restarts = d.iter().filter(|x| x.0 == 0).count();
        assert_eq!(restarts, 3);
        for w in d.windows(2) {
            assert!(w[1].0 == w[0].0 + 1 || w[1].0 == 0, "{w:?}");
        }
    }

    #[test]
    fn a_format_change_within_the_mode_keeps_the_rate() {
        let mut r = fast_rig(top192());
        r.stream(1.0, Running, FMT_48K_24)
            .at(2.0, |s| s.write_delay = Duration::from_millis(20))
            .at(3.0, |s| s.write_delay = FAST)
            .stream(4.0, Running, FMT_48K_16)
            .stream(5.0, Running, FMT_48K_24);
        r.run_to(6.0);
        assert_eq!(r.cmds(), ["SET_CONFIG", "OPEN", "START", "CLOSE"]);
        assert!(r.logged("same stream") >= 2);
        assert_eq!(r.logged("adaptive bitrate:"), 1);
        let d = dissect(&r.packets());
        assert_eq!(rate_runs(&d), [(192, 1), (128, 1), (88, 1)]);
        // The sequence goes on; only the frame cut at each change is lost.
        assert_continuous(&d);
        assert_eq!(changes(&r).len(), 2);
    }

    #[test]
    fn suspend_and_resume_keep_the_rate_and_idle_is_not_clean_time() {
        let mut r = fast_rig(top192());
        r.stream(1.0, Running, FMT_48K_24)
            .at(2.0, |s| s.write_delay = Duration::from_millis(20))
            .at(3.0, |s| s.write_delay = FAST)
            .stream(8.0, Paused, FMT_48K_24)
            .stream(100.0, Running, FMT_48K_24);
        r.run_to(112.0);
        assert_eq!(r.count("SUSPEND"), 1);
        let c = changes(&r);
        let steps: Vec<(u32, u32)> = c.iter().map(|x| (x.1, x.2)).collect();
        // 5 s of clean audio before the pause, 5 more after it, then 10.
        assert_eq!(steps, [(192, 128), (128, 88), (88, 128)], "{c:?}");
        assert!((104_800..=105_400).contains(&c[2].0), "{c:?}");
        let d = dissect(&r.packets());
        assert_continuous(&d);
    }

    #[test]
    fn adaptive_off_keeps_the_configured_rate_whatever_the_link_does() {
        let mut r = fast_rig(Settings {
            adaptive: false,
            ..Settings::default()
        });
        r.stream(1.0, Running, FMT_48K_24)
            .at(2.0, |s| s.write_delay = Duration::from_millis(20))
            .at(6.0, |s| s.write_delay = FAST);
        r.run_to(8.0);
        assert!(changes(&r).is_empty());
        assert_eq!(
            r.logged("adaptive bitrate is off (AdaptiveBitrate=0): always 229 kbps"),
            1
        );
        let d = dissect(&r.packets());
        assert_eq!(rate_runs(&d), [(229, 1)]);
        assert!(r.packets().iter().all(|p| p.len() == 588));
    }

    #[test]
    fn the_configured_bitrate_is_the_top_of_the_ladder() {
        let mut r = fast_rig(Settings {
            hifi_bitrate: Bitrate::K128,
            uhq_bitrate: Bitrate::K152,
            ..Settings::default()
        });
        r.stream(1.0, Running, FMT_48K_24)
            .at(2.0, |s| s.write_delay = Duration::from_millis(20))
            .at(3.0, |s| s.write_delay = FAST)
            .stream(40.0, Running, FMT_96K_24)
            .at(41.0, |s| s.write_delay = Duration::from_millis(20))
            .at(42.0, |s| s.write_delay = FAST);
        r.run_to(44.0);
        assert_eq!(r.logged("adaptive bitrate: 128 / 88 kbps"), 1);
        assert_eq!(
            r.logged(
                "adaptive bitrate: 152 kbps is the lowest rate of UHQ 96 kHz/24, nothing to adapt"
            ),
            1
        );
        let c = changes(&r);
        let steps: Vec<(u32, u32)> = c.iter().map(|x| (x.1, x.2)).collect();
        assert_eq!(steps, [(128, 88), (88, 128)], "{}", r.log.text());
        let d = dissect(&r.packets());
        assert_eq!(rate_runs(&d), [(128, 1), (88, 1), (128, 1), (152, 1)]);
        assert!(d.iter().all(|x| x.1 <= 152));
    }

    #[test]
    fn a_small_media_mtu_shapes_the_ladder() {
        // 600 bytes: UHQ starts at 291 kbps (342-byte packets), and two
        // frames of 152 kbps (360 bytes) fit below it.
        let mut r = fast_rig(top192());
        r.fake.with(|s| {
            s.report_mtus = Some((600, 65535));
            s.set_stream(Paused, FMT_96K_24);
        });
        r.stream(1.0, Running, FMT_96K_24)
            .at(2.0, |s| s.write_delay = Duration::from_millis(20));
        r.run_to(4.0);
        assert_eq!(
            r.logged("adaptive bitrate: 291 / 152 (2 frames/packet) kbps"),
            1
        );
        let d = dissect(&r.packets());
        assert_eq!(rate_runs(&d), [(291, 1), (152, 2)]);
        assert!(r.packets().iter().all(|p| p.len() <= 600));
    }

    #[test]
    fn a_write_that_timed_out_makes_the_next_link_start_low() {
        // Win32 error 121 (a WRITE that timed out) on the 100th packet.
        let mut r = fast_rig(top192());
        r.fake.with(|s| s.write_error = Some((100, 121)));
        r.stream(1.0, Running, FMT_48K_24)
            .at(3.0, |s| s.write_error = None);
        r.run_to(30.0);
        assert_eq!(r.logged("link down: media write failed"), 1);
        assert_eq!(
            r.logged("a media write timed out on the last link: starting at 88 kbps"),
            1
        );
        let d = dissect(&r.packets());
        // Up again after 20 and 20 clean seconds (a WRITE that timed out
        // doubles the wait for every rung).
        assert_eq!(rate_runs(&d), [(192, 1), (88, 1), (128, 1)]);
        let c = changes(&r);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].1, c[0].2), (88, 128));
        // A channel that just went away is no reason for that.
        let mut r = fast_rig(top192());
        r.stream(1.0, Running, FMT_48K_24)
            .at(2.0, |s| s.media_disconnected = true);
        r.run_to(6.0);
        assert_eq!(r.logged("link down: media write failed"), 1);
        assert_eq!(r.logged("starting at 88 kbps"), 0);
        let d = dissect(&r.packets());
        assert_eq!(rate_runs(&d), [(192, 1)]);
        // Nor is a link that went away under the WRITE (the buds into the
        // case): not connected, aborted, a stale channel, anything else.
        for code in [1167, 1236, 6, 31] {
            let mut r = fast_rig(top192());
            r.fake.with(|s| s.write_error = Some((100, code)));
            r.stream(1.0, Running, FMT_48K_24)
                .at(3.0, |s| s.write_error = None);
            r.run_to(8.0);
            assert_eq!(r.logged("link down: media write failed"), 1, "{code}");
            assert_eq!(r.logged("starting at 88 kbps"), 0, "{code}");
            let d = dissect(&r.packets());
            assert_eq!(rate_runs(&d), [(192, 1)], "{code}");
            assert!(d.len() > 200, "{code}: the link came back");
        }
    }

    #[test]
    fn rate_changes_are_rate_limited_in_the_log() {
        use crate::abr::{Change, Rate, Why};
        let mut r = rig();
        let c = Change {
            from: Rate {
                bitrate: Bitrate::K192,
                frames_per_packet: 1,
            },
            to: Rate {
                bitrate: Bitrate::K128,
                frames_per_packet: 1,
            },
            why: Why::Dropout,
        };
        let s = Duration::from_secs;
        for k in 0..20 {
            r.s.log_rate_change(&c, 336, s(k));
        }
        assert_eq!(r.logged("bitrate 192 -> 128 kbps"), 20, "debug has all");
        let info = |r: &Rig| {
            r.log
                .lines
                .lock()
                .unwrap()
                .iter()
                .filter(|l| l.1 == Level::Info && l.2.contains("bitrate 192"))
                .count()
        };
        assert_eq!(info(&r), RATE_LOG_BURST as usize);
        r.s.log_rate_change(&c, 336, s(60));
        assert_eq!(info(&r), RATE_LOG_BURST as usize + 1);
        assert_eq!(
            r.logged("(12 change(s) before this one were not logged)"),
            1
        );
    }

    /// A link whose capacity jumps around for two minutes, with stalls and
    /// format switches thrown in: the session survives, every packet is
    /// sound, and the rate does not flap.
    #[test]
    fn a_wild_link_never_makes_the_rate_flap() {
        for seed in [11u32, 12] {
            let mut r = fast_rig(top192());
            let mut x = seed.wrapping_mul(2_654_435_761).max(1);
            let mut rnd = move |n: u32| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x % n
            };
            let fmts = [FMT_48K_24, FMT_96K_24, FMT_48K_16];
            r.stream(1.0, Running, FMT_48K_24);
            let mut t = 1.0;
            let mut switches = Vec::new();
            while t < 120.0 {
                t += 0.2 + f64::from(rnd(6000)) / 1000.0;
                match rnd(8) {
                    0..=3 => {
                        let bps = [
                            None,
                            Some(60_000),
                            Some(120_000),
                            Some(170_000),
                            Some(400_000),
                        ][rnd(5) as usize];
                        r.at(t, move |s| s.link_bps = bps);
                    }
                    4 => {
                        let ms = u64::from(20 + rnd(400));
                        r.at(t, move |s| {
                            let n = s.packets.len() + 2;
                            s.write_stalls.push((n, Duration::from_millis(ms)));
                        });
                    }
                    5 => {
                        let ms = u64::from(20 + rnd(200));
                        r.at(t, move |s| s.wait_stall = Duration::from_millis(ms));
                    }
                    6 => {
                        let fmt = fmts[rnd(3) as usize];
                        r.stream(t, Running, fmt);
                        switches.push((t * 1000.0) as u64);
                    }
                    _ => {
                        let d = Duration::from_micros(u64::from(100 + rnd(3000)));
                        r.at(t, move |s| s.write_delay = d);
                    }
                }
            }
            r.run_to(122.0);
            let d = dissect(&r.packets());
            for w in d.windows(2) {
                assert!(w[1].0 == w[0].0.wrapping_add(1) || w[1].0 == 0, "{w:?}");
            }
            // Between two mode switches (each starts a new controller at
            // the top) no minute has more than a handful of changes.
            let c = changes(&r);
            for (i, a) in c.iter().enumerate() {
                let next_switch = switches
                    .iter()
                    .copied()
                    .find(|s| *s > a.0)
                    .unwrap_or(u64::MAX);
                let n = c[i..]
                    .iter()
                    .take_while(|b| b.0 < a.0 + 60_000 && b.0 < next_switch)
                    .count();
                assert!(n <= 7, "seed {seed}: {n} changes from {} ms: {c:?}", a.0);
            }
            assert!(!c.is_empty(), "seed {seed}: the link was bad at times");
        }
    }

    /// Most bitrate changes in any 60 s.
    fn worst_minute(c: &[(u64, u32, u32)]) -> usize {
        (0..c.len())
            .map(|i| c[i..].iter().take_while(|y| y.0 < c[i].0 + 60_000).count())
            .max()
            .unwrap_or(0)
    }

    fn steps(c: &[(u64, u32, u32)]) -> Vec<(u32, u32)> {
        c.iter().map(|x| (x.1, x.2)).collect()
    }

    /// A rig whose driver keeps 250 ms of PCM, like the real one.
    fn real_ring_rig(fmt: PcmFormat) -> Rig {
        real_ring_rig_with(top192(), fmt)
    }

    fn real_ring_rig_with(settings: Settings, fmt: PcmFormat) -> Rig {
        let r = fast_rig(settings);
        r.fake.with(|s| {
            s.set_stream(Paused, fmt);
            s.ring_chunks = 25;
        });
        r
    }

    /// The cases of a test that must hold under either HiFi top rate: each
    /// `(format, link capacity in bps)` under the 192 kbps top, and the
    /// 48 kHz ones again under the default 229 kbps top, on a link with the
    /// same room to spare (229 kbps puts 588 bytes on the wire where 192
    /// puts 496).
    fn under_both_tops(fmt: PcmFormat, bps: u32) -> Vec<(Settings, u32)> {
        let mut v = vec![(top192(), bps)];
        if fmt == FMT_48K_24 {
            let scaled = (u64::from(bps) * 588).div_ceil(496) as u32;
            v.push((Settings::default(), scaled));
        }
        v
    }

    #[test]
    fn a_link_that_carries_the_middle_rate_settles_there() {
        // The link carries the middle rate (HiFi 128 kbps is 149.3 on the
        // wire, UHQ 291 kbps 298.7) but not the top one. When a probe of
        // the top fails, its backlog goes out back to back at the middle
        // rate into a link whose buffer is full; those WRITEs take most of
        // a packet time each and are the link doing what it can, not a
        // reason to go on to the bottom rate.
        for (fmt, bps, top, mid) in [
            (FMT_48K_24, 160_000u32, 192, 128),
            (FMT_96K_24, 330_000, 584, 291),
        ] {
            let mut r = real_ring_rig(fmt);
            r.stream(1.0, Running, fmt)
                .at(5.0, move |s| s.link_bps = Some(bps));
            r.run_to(305.0);
            let c = changes(&r);
            let ctx = format!("{bps} bps: {c:?}");
            for (k, s) in steps(&c).iter().enumerate() {
                let want = if k % 2 == 0 { (top, mid) } else { (mid, top) };
                assert_eq!(*s, want, "{ctx}");
            }
            // The top is tried after 10, 20, 40 and then every 80 s (and
            // the second or two in which the backlog of the probe goes out).
            let holds: Vec<u64> = (1..c.len())
                .step_by(2)
                .map(|k| (c[k].0 - c[k - 1].0) / 1000)
                .collect();
            assert_eq!(holds.len(), 5, "{ctx}");
            for (h, want) in holds.iter().zip([10, 20, 40, 80, 80]) {
                assert!((want..want + 3).contains(h), "{ctx}");
            }
            assert!(worst_minute(&c) <= 6, "{ctx}");
            let d = dissect(&r.packets());
            assert_continuous(&d);
            assert!(d.iter().all(|x| x.1 >= mid), "{ctx}");
            let at_top: usize = d[300..].iter().filter(|x| x.1 == top).map(|x| x.2).sum();
            let all: usize = d[300..].iter().map(|x| x.2).sum();
            assert!(at_top * 20 < all, "{ctx}: {at_top} of {all} frames");
            assert_eq!(r.fake.with(|s| s.state.frames_dropped), 0, "{ctx}");
        }
    }

    #[test]
    fn one_stall_on_a_good_but_limited_link_changes_nothing() {
        // A link with room to spare, but not without limit, and one
        // stalled WRITE. The backlog then goes out back to back, every
        // WRITE waiting for room in the link's buffer: a sender catching
        // up, not a congested link.
        for (fmt, bps, stall, n) in [
            (FMT_96K_24, 2_000_000u32, 150u64, 600usize),
            (FMT_96K_24, 1_400_000, 80, 600),
            (FMT_96K_24, 1_000_000, 80, 600),
            (FMT_96K_24, 700_000, 200, 600),
            (FMT_48K_24, 700_000, 150, 300),
            (FMT_48K_24, 1_000_000, 150, 300),
            (FMT_48K_24, 260_000, 200, 300),
        ] {
            for (buffer, (settings, bps)) in [2000usize, 8000]
                .into_iter()
                .flat_map(|b| under_both_tops(fmt, bps).into_iter().map(move |c| (b, c)))
            {
                let mut r = real_ring_rig_with(settings, fmt);
                r.fake.with(|s| {
                    s.link_bps = Some(bps);
                    s.link_buffer = buffer;
                    s.write_stalls = vec![(n, Duration::from_millis(stall))];
                });
                r.s.set_stats_period(Duration::from_secs(20));
                r.stream(1.0, Running, fmt);
                r.run_to(30.0);
                let ctx = format!(
                    "{} Hz, top {} kbps, {bps} bps, buffer {buffer}, {stall} ms: {}",
                    fmt.sample_rate,
                    settings.hifi_bitrate.bps() / 1000,
                    r.log.text()
                );
                assert!(changes(&r).is_empty(), "{ctx}");
                assert_eq!(r.fake.with(|s| s.state.frames_dropped), 0, "{ctx}");
                let stats = r.log.find("stats:");
                assert!(stats[0].1.contains("rate_changes=0 slow_writes="), "{ctx}");
                assert!(!stats[0].1.contains("slow_writes=0 "), "{ctx}");
            }
        }
    }

    #[test]
    fn audio_waiting_at_start_is_sent_without_stepping_down() {
        // The signaling requests take time, so the driver's ring holds up
        // to 250 ms when START is accepted: that goes out back to back.
        for (fmt, bps, buf, delay) in [
            (FMT_96K_24, 1_400_000u32, 2000usize, 250u64),
            (FMT_96K_24, 1_000_000, 2000, 150),
            (FMT_96K_24, 1_000_000, 8000, 250),
            (FMT_96K_24, 700_000, 8000, 150),
            (FMT_96K_24, 700_000, 2000, 250),
            (FMT_96K_24, 2_000_000, 2000, 250),
            (FMT_48K_24, 260_000, 2000, 250),
            (FMT_48K_24, 700_000, 8000, 250),
        ] {
            for (settings, bps) in under_both_tops(fmt, bps) {
                let mut r = real_ring_rig_with(settings, fmt);
                r.fake.with(|s| {
                    s.link_bps = Some(bps);
                    s.link_buffer = buf;
                    s.request_delay = Duration::from_millis(delay);
                });
                r.stream(1.0, Running, fmt);
                r.run_to(30.0);
                let ctx = format!(
                    "{} Hz, top {} kbps, {bps} bps, buffer {buf}, {delay} ms",
                    fmt.sample_rate,
                    settings.hifi_bitrate.bps() / 1000
                );
                assert!(changes(&r).is_empty(), "{ctx}: {}", r.log.text());
                assert_eq!(r.logged("link down"), 0, "{ctx}");
            }
        }
    }

    #[test]
    fn a_short_first_write_into_a_full_ring_is_not_congestion() {
        // The ring is full at START; the first WRITE takes 20 or 30 ms (as
        // seen on real buds) and audio is dropped. That is not the link.
        for (fmt, delay, k, stall) in [
            (FMT_48K_24, 300u64, 0usize, 30u64),
            (FMT_48K_24, 300, 2, 30),
            (FMT_96K_24, 300, 0, 20),
            (FMT_96K_24, 300, 3, 25),
            (FMT_48K_24, 300, 0, 60),
        ] {
            for (settings, _) in under_both_tops(fmt, 0) {
                let mut r = real_ring_rig_with(settings, fmt);
                r.fake.with(|s| {
                    s.request_delay = Duration::from_millis(delay);
                    s.write_stalls = vec![(k, Duration::from_millis(stall))];
                });
                r.stream(1.0, Running, fmt);
                r.run_to(8.0);
                let ctx = format!(
                    "{} Hz, top {} kbps, WRITE {k} takes {stall} ms",
                    fmt.sample_rate,
                    settings.hifi_bitrate.bps() / 1000
                );
                assert!(changes(&r).is_empty(), "{ctx}: {}", r.log.text());
            }
        }
    }

    #[test]
    fn stray_slow_writes_do_not_keep_the_rate_down() {
        // One second of congestion, then a perfect link with one WRITE of
        // 5 ms every 9 s. That never moves a stream at the top, and must
        // not hold one at the bottom.
        let stray = |r: &Rig| {
            for k in 0..30 {
                r.at(6.0 + 9.0 * f64::from(k), |s| {
                    let n = s.packets.len();
                    s.write_stalls.push((n + 1, Duration::from_millis(5)));
                });
            }
        };
        let mut r = fast_rig(top192());
        r.stream(1.0, Running, FMT_48K_24)
            .at(3.0, |s| s.write_delay = Duration::from_millis(20))
            .at(4.0, |s| s.write_delay = FAST);
        stray(&r);
        r.run_to(240.0);
        let c = changes(&r);
        assert_eq!(
            steps(&c),
            [(192, 128), (128, 88), (88, 128), (128, 192)],
            "{}",
            r.log.text()
        );
        // Each stray WRITE costs a quarter of a second of the 10 s.
        assert!((14_000..=15_500).contains(&c[2].0), "{c:?}");
        assert!((24_000..=26_500).contains(&c[3].0), "{c:?}");
        // The same without the congestion.
        let mut r = fast_rig(top192());
        r.stream(1.0, Running, FMT_48K_24);
        stray(&r);
        r.run_to(240.0);
        assert!(changes(&r).is_empty());
    }

    #[test]
    fn a_link_that_is_busy_half_of_the_time_steps_down_without_a_backlog() {
        // Every WRITE takes 9 ms of the 18 ms a packet plays: the sender
        // keeps up, nothing is late, and the rate still gives way.
        let mut r = fast_rig(top192());
        r.s.set_stats_period(Duration::from_secs(5));
        r.stream(1.0, Running, FMT_48K_24)
            .at(3.0, |s| s.write_delay = Duration::from_millis(9));
        r.run_to(20.0);
        let c = changes(&r);
        // One rung: at the lower rate the WRITEs still take 9 ms, less
        // than a packet plays, so nothing says that it is too much.
        assert_eq!(steps(&c), [(192, 128)], "{}", r.log.text());
        assert!((3100..=4000).contains(&c[0].0), "{c:?}");
        assert_eq!(r.fake.with(|s| s.state.frames_dropped), 0);
        let stats = r.log.find("stats:");
        assert!(stats[1].1.contains("late_max=0ms"), "{}", stats[1].1);
    }

    #[test]
    fn a_pause_too_short_for_a_suspend_is_not_lateness() {
        // The player pauses for a moment (a track change, scrubbing, a UI
        // sound ending): the endpoint stands still for less than the 3 s
        // after which the stream is suspended. The audio after it is not
        // late, and the pauses do not keep the rate from coming back.
        for pause in [1.0, 0.1] {
            let mut r = fast_rig(top192());
            r.s.set_stats_period(Duration::from_secs(20));
            r.stream(1.0, Running, FMT_48K_24)
                .at(3.0, |s| s.write_delay = Duration::from_millis(20))
                .at(3.3, |s| s.write_delay = FAST);
            for k in 0..12 {
                let t = 8.0 + 9.0 * f64::from(k);
                r.stream(t, Paused, FMT_48K_24)
                    .stream(t + pause, Running, FMT_48K_24);
            }
            r.run_to(120.0);
            assert_eq!(r.count("SUSPEND"), 0);
            let c = changes(&r);
            assert_eq!(steps(&c), [(192, 128), (128, 192)], "{}", r.log.text());
            // 10 s of audio after the congestion, and one or two pauses.
            let up = 13_300 + (2.0 * pause * 1000.0) as u64;
            assert!((13_300..=up + 300).contains(&c[1].0), "{pause}: {c:?}");
            let stats = r.log.find("stats:");
            assert!(stats.len() >= 5);
            for (_, line) in &stats[1..] {
                assert!(line.contains("late_max=0ms"), "{pause}: {line}");
            }
        }
    }

    // -- the default HiFi ladder: 229 / 192 / 128 / 88 -----------------------

    #[test]
    fn the_default_hifi_ladder_walks_229_192_128_88_and_back() {
        let mut r = fast_rig(Settings::default());
        // Every WRITE takes 20 ms from 3.0 to 4.0 s: more than a packet
        // plays (18 ms), whatever its rate.
        r.stream(1.0, Running, FMT_48K_24)
            .at(3.0, |s| s.write_delay = Duration::from_millis(20))
            .at(4.0, |s| s.write_delay = FAST);
        r.run_to(40.0);
        assert_eq!(r.logged("adaptive bitrate: 229 / 192 / 128 / 88 kbps"), 1);
        let c = changes(&r);
        assert_eq!(
            steps(&c),
            [
                (229, 192),
                (192, 128),
                (128, 88),
                (88, 128),
                (128, 192),
                (192, 229)
            ],
            "{}",
            r.log.text()
        );
        // Down within 200 ms of the first slow WRITE, then one rung each
        // time the new rate had its chance and the WRITEs still block.
        assert!((3060..=3200).contains(&c[0].0), "{c:?}");
        let settle = DOWN_SETTLE.as_millis() as u64;
        for k in 1..3 {
            let since = c[k].0 - c[k - 1].0;
            assert!((settle..=500).contains(&since), "{c:?}");
        }
        // Up one rung per 10 s of clean audio once the link recovered.
        let hold = UP_HOLD_MIN.as_millis() as u64;
        assert!((3900 + hold..=4300 + hold).contains(&c[3].0), "{c:?}");
        for k in 4..6 {
            let since = c[k].0 - c[k - 1].0;
            assert!((hold..=hold + 700).contains(&since), "{c:?}");
        }
        // One RTP sequence through every change, every packet one frame of
        // the size its rate has, the top being the 229 kbps frame.
        let d = dissect(&r.packets());
        assert_continuous(&d);
        assert_eq!(
            rate_runs(&d),
            [
                (229, 1),
                (192, 1),
                (128, 1),
                (88, 1),
                (128, 1),
                (192, 1),
                (229, 1)
            ]
        );
        let top = 12 + ssc_enc::frame_size(ssc_enc::Mode::Hifi48k, Bitrate::K229);
        assert_eq!(top, 588);
        let sizes: Vec<usize> = r.packets().iter().map(Vec::len).collect();
        assert_eq!(sizes[0], top);
        assert_eq!(sizes.last(), Some(&top));
        assert!(sizes.iter().all(|n| [588, 496, 336, 236].contains(n)));
        assert_eq!(r.fake.with(|s| s.state.frames_dropped), 0);
        assert_eq!(r.logged("link down"), 0);
    }

    #[test]
    fn a_link_that_carries_192_but_not_229_settles_at_192() {
        // HiFi at 229 kbps is 588 bytes every 18 ms, 261.3 kbps on the
        // wire; at 192 kbps it is 496 bytes, 220.4 kbps. A link of 240 kbps
        // carries the second rung and not the top: the stream settles one
        // rung down, never lower, and tries the top ever more rarely.
        let r = fast_rig(Settings::default());
        r.fake.with(|s| {
            s.set_stream(Paused, FMT_48K_24);
            s.ring_chunks = 25;
        });
        let mut r = r;
        r.stream(1.0, Running, FMT_48K_24)
            .at(5.0, |s| s.link_bps = Some(240_000));
        r.run_to(305.0);
        let c = changes(&r);
        let ctx = format!("{c:?}");
        assert!(c.len() >= 3, "{ctx}");
        for (k, s) in steps(&c).iter().enumerate() {
            let want = if k % 2 == 0 { (229, 192) } else { (192, 229) };
            assert_eq!(*s, want, "{ctx}");
        }
        // Each failed probe of the top doubles the wait for the next.
        let holds: Vec<u64> = (1..c.len())
            .step_by(2)
            .map(|k| (c[k].0 - c[k - 1].0) / 1000)
            .collect();
        for (h, want) in holds.iter().zip([10, 20, 40, 80, 80]) {
            assert!((want..want + 3).contains(h), "{ctx}");
        }
        assert!(worst_minute(&c) <= 6, "{ctx}");
        let d = dissect(&r.packets());
        assert_continuous(&d);
        assert!(d.iter().all(|x| x.1 >= 192), "{ctx}");
        assert_eq!(r.fake.with(|s| s.state.frames_dropped), 0, "{ctx}");
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

mod status {
    use super::*;

    /// Step the session until the fake's time has reached `secs`.
    fn run_until(r: &mut Rig, secs: f64) {
        let t = Duration::from_secs_f64(secs);
        while r.fake.now() < t && r.s.phase() != Phase::Stopped {
            r.s.step();
        }
    }

    #[test]
    fn status_follows_the_session() {
        let mut r = rig();
        r.fake.with(|s| s.attach_error = Some(170));
        r.at(5.0, |s| s.attach_error = None)
            .stream(11.0, Running, FMT_48K_24)
            .at(12.0, |s| s.buds_disconnect());
        // Before the first step: nothing known.
        let st = r.s.status();
        assert_eq!(
            (st.phase, st.connected, st.retrying),
            (Phase::WaitDriver, None, false)
        );
        assert_eq!(
            (st.reason.as_str(), st.endpoint, st.driver),
            ("", None, None)
        );
        assert!(st.link.is_none());
        assert_eq!(st.totals, Totals::default());
        // The driver is busy: waiting for it, and why.
        r.s.step();
        let st = r.s.status();
        assert_eq!(st.phase, Phase::WaitDriver);
        assert!(
            st.reason.starts_with("driver not ready: AUDIO_ATTACH:"),
            "{}",
            st.reason
        );
        assert_eq!(st.driver, None);
        // Attached at the next retry (10 s) and linked at once.
        run_until(&mut r, 10.5);
        let st = r.s.status();
        assert_eq!(
            (st.phase, st.connected, st.retrying),
            (Phase::Ready, Some(true), false)
        );
        assert_eq!(st.reason, "");
        assert_eq!(st.driver, Some((1 << 16, 3)));
        // The fake driver's state names a format from the start.
        assert_eq!(st.endpoint, Some(FMT_48K_24));
        let l = st.link.expect("a link");
        assert_eq!((l.plan.mode, l.plan.config), (SscMode::Hifi, 0x0C));
        assert_eq!(l.caps, r.fake.with(|s| s.sink.ssc_flags));
        assert_eq!((l.rate.kbps(), l.rung), (229, 0));
        let ladder: Vec<u32> = l.ladder.rungs().iter().map(|x| x.kbps()).collect();
        assert_eq!(ladder, [229, 192, 128, 88]);
        assert_eq!(l.mtu_limit, 1005);
        assert!(l.mtu_why.contains("capped at 1005"), "{}", l.mtu_why);
        assert_eq!(l.streaming_for, None);
        assert_eq!(st.totals, Totals::default());
        // Nearly a second of audio (the buds go away at 12 s).
        run_until(&mut r, 11.9);
        let st = r.s.status();
        assert_eq!(st.phase, Phase::Streaming);
        assert_eq!(st.endpoint, Some(FMT_48K_24));
        let l = st.link.expect("a link");
        let for_ms = l.streaming_for.expect("streaming").as_millis();
        assert!((890..=920).contains(&for_ms), "{for_ms}");
        let sent = r.packets().len() as u64;
        assert!((48..=50).contains(&sent), "{sent}");
        assert_eq!(st.totals.packets, sent);
        assert_eq!(st.totals.write_count, sent);
        assert!((43_000..=44_000).contains(&st.totals.frames_in));
        assert_eq!(
            (
                st.totals.reconnects,
                st.totals.write_errors,
                st.totals.late_packets
            ),
            (0, 0, 0)
        );
        // Asking costs nothing: no driver call, no log line.
        let before = (r.fake.with(|s| s.calls.len()), r.log.text().len());
        let again = r.s.status();
        assert_eq!(again, r.s.status());
        assert_eq!(before, (r.fake.with(|s| s.calls.len()), r.log.text().len()));
        // The buds go away: the link is down and is being set up again...
        while r.s.phase() != Phase::WaitLink {
            r.s.step();
        }
        assert_eq!(r.fake.now(), Duration::from_secs(12));
        let st = r.s.status();
        assert_eq!(
            (st.phase, st.connected, st.retrying),
            (Phase::WaitLink, Some(true), true)
        );
        assert!(st.reason.starts_with("link down: "), "{}", st.reason);
        assert!(st.link.is_none());
        assert_eq!(st.totals.reconnects, 1);
        assert!(st.totals.packets >= sent, "the counters outlive the link");
        assert!(st.totals.write_errors <= 1);
        // ... until Windows says they are not connected.
        run_until(&mut r, 14.0);
        let st = r.s.status();
        assert_eq!(
            (st.phase, st.connected, st.retrying),
            (Phase::WaitLink, Some(false), false)
        );
        r.run_to(15.0);
        assert_eq!(r.s.status().phase, Phase::Stopped);
    }

    #[test]
    fn the_stream_latency_starts_anew_with_every_start() {
        let mut r = rig();
        r.fake.with(|s| s.write_delay = Duration::from_millis(2));
        r.stream(1.0, Running, FMT_48K_24)
            .stream(2.0, Paused, FMT_48K_24)
            .at(5.5, |s| s.write_delay = Duration::from_micros(100))
            .stream(6.0, Running, FMT_48K_24);
        run_until(&mut r, 2.0);
        let t = r.s.status().totals;
        assert_eq!(t.write_max, Duration::from_millis(2));
        assert_eq!(
            t.write_total,
            Duration::from_millis(2) * t.write_count as u32
        );
        let first = t.packets;
        // Suspended after 3 s of silence, started again at 6 s.
        run_until(&mut r, 6.5);
        let st = r.s.status();
        assert_eq!(st.phase, Phase::Streaming);
        let t = st.totals;
        assert!(t.packets > first, "the packet count goes on");
        assert_eq!(t.write_count, t.packets - first, "the latency does not");
        assert_eq!(t.write_max, Duration::from_micros(100));
        r.run_to(7.0);
    }

    #[test]
    fn a_rate_step_and_a_mode_switch_show_in_the_status() {
        let mut r = rig();
        r.fake.with(|s| s.write_delay = Duration::from_micros(200));
        r.stream(1.0, Running, FMT_48K_24)
            .at(3.0, |s| s.write_delay = Duration::from_millis(20))
            .at(3.25, |s| s.write_delay = Duration::from_micros(200))
            .stream(5.0, Running, FMT_96K_24);
        run_until(&mut r, 4.0);
        let l = r.s.status().link.expect("a link");
        assert_eq!((l.rate.kbps(), l.rung), (192, 1), "one rung down");
        assert_eq!(l.ladder.top().kbps(), 229);
        assert_eq!(l.rate.packet_bytes(l.plan.encoder), 496);
        // Another mode: another stream, another ladder, at its top.
        run_until(&mut r, 5.5);
        let st = r.s.status();
        assert_eq!(st.phase, Phase::Streaming);
        assert_eq!(st.endpoint, Some(FMT_96K_24));
        let l = st.link.expect("a link");
        assert_eq!((l.plan.mode, l.plan.config), (SscMode::Uhq, 0x0A));
        assert_eq!((l.rate.kbps(), l.rung), (584, 0));
        let ladder: Vec<u32> = l.ladder.rungs().iter().map(|x| x.kbps()).collect();
        assert_eq!(ladder, [584, 291, 152]);
        let for_ms = l.streaming_for.expect("streaming").as_millis();
        assert!(for_ms <= 510, "{for_ms}: counted from the new START");
        assert_eq!(
            st.totals.reconnects, 0,
            "a reconfiguration is not a reconnect"
        );
        r.run_to(6.0);
    }

    #[test]
    fn dropped_frames_are_only_what_a_started_stream_lost() {
        let mut r = rig();
        // The driver's counter is never reset: it carries what was dropped
        // before this session existed.
        r.fake.with(|s| s.drop_frames(5000));
        // The sink suspends at 1.5 while audio keeps playing: the driver's
        // ring overflows, with nobody reading, until START is accepted
        // again at 3.5. None of that is audio the session failed to read.
        r.stream(1.0, Running, FMT_48K_24)
            .at(1.5, |s| s.sink_suspends())
            .at(4.105, |s| s.drop_frames(1000))
            .stream(4.5, Paused, FMT_48K_24);
        run_until(&mut r, 3.4);
        let driver = r.fake.with(|s| s.state.frames_dropped);
        assert!(driver > 5000 + 48_000, "{driver}: the ring overflowed");
        assert_eq!(r.s.status().phase, Phase::Ready);
        assert_eq!(r.s.status().totals.dropped_frames, 0);
        run_until(&mut r, 4.0);
        let st = r.s.status();
        assert_eq!(st.phase, Phase::Streaming);
        assert_eq!(st.totals.dropped_frames, 0, "what was lost before START");
        // 1000 frames are lost from the started stream: they count, and are
        // known with the next state the driver announces.
        run_until(&mut r, 4.6);
        assert_eq!(r.s.status().totals.dropped_frames, 1000);
        r.run_to(5.0);
        assert_eq!(r.s.status().totals.dropped_frames, 1000);
    }

    #[test]
    fn frames_dropped_just_before_the_stream_stops_are_counted() {
        // No state event and no statistics line between the loss and the
        // sink's SUSPEND: the counter is read when the stream stops.
        let mut r = rig();
        r.stream(1.0, Running, FMT_48K_24)
            .at(1.305, |s| s.drop_frames(1000))
            .at(1.4, |s| s.sink_suspends());
        run_until(&mut r, 1.3);
        assert_eq!(r.s.status().totals.dropped_frames, 0);
        run_until(&mut r, 1.45);
        assert_eq!(r.s.status().phase, Phase::Ready);
        assert_eq!(r.s.status().totals.dropped_frames, 1000);
        // ... and when the link goes down under a started stream.
        let mut r = rig();
        r.stream(1.0, Running, FMT_48K_24)
            .at(1.305, |s| s.drop_frames(700))
            .at(1.4, |s| s.sink_closes());
        run_until(&mut r, 1.45);
        assert_eq!(r.s.status().phase, Phase::WaitLink);
        assert_eq!(r.s.status().totals.dropped_frames, 700);
        r.run_to(2.0);
    }

    #[test]
    fn the_sink_delay_is_that_of_the_current_stream() {
        let mut r = rig();
        r.stream(1.0, Running, FMT_48K_24)
            .at(2.0, |s| s.sink_reports_delay(2020))
            .stream(3.0, Running, FMT_96K_24)
            .at(5.0, |s| s.sink_reports_delay(1500))
            .at(6.0, |s| s.sink_closes());
        run_until(&mut r, 2.5);
        assert_eq!(r.s.status().sink_delay, Some(2020));
        // Another mode is another stream: the old one's delay is not its.
        run_until(&mut r, 3.5);
        let st = r.s.status();
        assert_eq!(st.link.map(|l| l.plan.mode), Some(SscMode::Uhq));
        assert_eq!(st.sink_delay, None);
        run_until(&mut r, 5.5);
        assert_eq!(r.s.status().sink_delay, Some(1500));
        // The link goes down: nothing is left of it, also once the stream
        // is set up again and the sink has not reported.
        while r.s.phase() != Phase::WaitLink {
            r.s.step();
        }
        assert_eq!(r.s.status().sink_delay, None);
        while r.s.phase() == Phase::WaitLink {
            r.s.step();
        }
        assert!(r.s.status().link.is_some());
        assert_eq!(r.s.status().sink_delay, None);
        r.run_to(r.fake.now().as_secs_f64() + 1.0);

        // A sink that reports while the new stream is set up: that report
        // is the new stream's and stays.
        let mut r = rig();
        r.stream(1.0, Running, FMT_48K_24)
            .at(2.0, |s| s.sink_reports_delay(2020))
            .at(2.5, |s| s.sink.delay_after_set_config = Some(3400))
            .stream(3.0, Running, FMT_96K_24);
        run_until(&mut r, 3.5);
        assert_eq!(r.s.status().sink_delay, Some(3400));
        r.run_to(4.0);
    }

    #[test]
    fn a_delay_reported_for_a_stream_that_did_not_come_about_is_dropped() {
        // The sink reports after SET_CONFIGURATION and then refuses OPEN.
        let mut r = rig();
        r.fake.with(|s| {
            s.sink.delay_after_set_config = Some(3400);
            s.sink.reject_open = Some(0x31);
        });
        r.at(0.6, |s| {
            s.sink.delay_after_set_config = None;
            s.sink.reject_open = None;
        });
        run_until(&mut r, 0.5);
        let st = r.s.status();
        assert!(st.link.is_none(), "{}", r.log.text());
        assert_eq!(
            r.logged("[buds] sink DELAYREPORT: 340.0 ms"),
            1,
            "{}",
            r.log.text()
        );
        assert_eq!(st.sink_delay, None);
        // The next attempt works and the sink says nothing: still none.
        while r.s.phase() == Phase::WaitLink {
            assert!(r.fake.now() < Duration::from_secs(60), "never set up");
            r.s.step();
        }
        assert!(r.s.status().link.is_some());
        assert_eq!(r.s.status().sink_delay, None);
        r.run_to(r.fake.now().as_secs_f64() + 1.0);
    }

    #[test]
    fn an_older_state_does_not_count_dropped_frames_twice() {
        // The statistics line asks the driver (1000 lost by then). A state
        // announced before that, and taken up after it, carries the counter
        // as it was: the frames are counted once.
        let mut r = rig();
        r.s.set_stats_period(Duration::from_millis(200));
        r.stream(1.0, Running, FMT_48K_24)
            .at(1.105, |s| s.drop_frames(1000));
        run_until(&mut r, 1.3);
        assert_eq!(r.s.status().totals.dropped_frames, 1000);
        let mut link = r.s.link.take().expect("a link");
        let stale = r.fake.with(|s| s.state.frames_dropped) - 1000;
        r.s.count_dropped(&mut link, stale);
        r.s.count_dropped(&mut link, stale + 1000);
        assert_eq!(r.s.status().totals.dropped_frames, 1000);
        r.s.count_dropped(&mut link, stale + 1300);
        assert_eq!(r.s.status().totals.dropped_frames, 1300);
        r.s.link = Some(link);
        r.run_to(1.5);
    }

    #[test]
    fn the_reason_of_a_failure_goes_when_the_buds_do() {
        let mut r = rig();
        r.fake.with(|s| s.sink.reject_open = Some(0x31));
        r.at(3.0, |s| s.buds_disconnect());
        run_until(&mut r, 0.5);
        let st = r.s.status();
        assert_eq!((st.phase, st.connected), (Phase::WaitLink, Some(true)));
        assert!(
            st.reason.starts_with("cannot set up the stream: "),
            "{}",
            st.reason
        );
        // In their case: "not connected" is the whole story, not a failure
        // from when they were there.
        while r.s.status().connected != Some(false) {
            assert!(r.fake.now() < Duration::from_secs(60), "never seen gone");
            r.s.step();
        }
        let st = r.s.status();
        assert_eq!((st.phase, st.reason.as_str()), (Phase::WaitLink, ""));
        r.run_to(r.fake.now().as_secs_f64() + 1.0);

        // The same for a link that went down because they left.
        let mut r = rig();
        r.stream(1.0, Running, FMT_48K_24)
            .at(2.0, |s| s.buds_disconnect());
        run_until(&mut r, 1.5);
        assert_eq!(r.s.phase(), Phase::Streaming);
        while r.s.phase() != Phase::WaitLink {
            r.s.step();
        }
        assert!(r.s.status().reason.starts_with("link down: "));
        while r.s.status().connected != Some(false) {
            assert!(r.fake.now() < Duration::from_secs(60), "never seen gone");
            r.s.step();
        }
        assert_eq!(r.s.status().reason, "");
        r.run_to(r.fake.now().as_secs_f64() + 1.0);
    }
}
