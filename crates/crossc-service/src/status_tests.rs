//! The status feed against the fake driver: the hub's snapshot has to follow
//! a session through everything it does. Time is the fake's virtual time,
//! which is also the clock the publisher's 250 ms come from.

use super::*;

use std::cell::Cell;
use std::rc::Rc;

use crossc_core::abr::{Ladder, Rate};
use crossc_core::fake::MemoryLog;
use crossc_core::fake::driver::{FMT_48K_24, FMT_96K_24, FakeClock, FakeDriver, FakeLink};
use crossc_core::log::NullLog;
use crossc_core::plan::select_plan;
use crossc_core::session::{LinkView, Totals};
use crossc_core::settings::Settings;
use crossc_ioctl::audio::{
    AUDIO_FLAG_ACX, AUDIO_FLAG_AVRCP, AUDIO_FLAG_HW_VOLUME, AUDIO_FLAG_MUTED, AudioState,
    StreamState, VolumeState,
};
use ssc_enc::Bitrate;

const ID: &str = "AABBCCDDEEFF";
/// A healthy media WRITE.
const FAST: Duration = Duration::from_micros(200);

type FakeSession = Session<FakeDriver, FakeLink, FakeClock>;

struct Rig {
    fake: FakeDriver,
    hub: Hub,
    log: Arc<MemoryLog>,
    s: FakeSession,
    /// How often the Bluetooth name was looked up.
    lookups: Rc<Cell<u32>>,
}

/// A session on the fake driver that logs through a [`StatusLog`] into a
/// hub, as the service's sessions do.
fn rig() -> Rig {
    let fake = FakeDriver::new();
    fake.with(|s| s.write_delay = FAST);
    let hub = new_hub(String::new());
    let log = Arc::new(MemoryLog::new(fake.with(|s| s.clock_us.clone())));
    let s = Session::new(
        fake.clone(),
        fake.link_probe(),
        fake.clock(),
        Settings::default(),
        Arc::new(StatusLog::new(log.clone(), hub.clone())),
        fake.stop(),
        ID,
    );
    Rig {
        fake,
        hub,
        log,
        s,
        lookups: Rc::default(),
    }
}

impl Rig {
    /// The name lookup of the tests: nothing the first time (Windows has
    /// not seen the buds), a name from then on.
    fn lookup(&self) -> impl FnMut() -> String + use<> {
        let n = self.lookups.clone();
        move || {
            n.set(n.get() + 1);
            if n.get() == 1 {
                String::new()
            } else {
                "Test Buds".to_string()
            }
        }
    }
}

/// The one device in the hub.
fn dev(hub: &Hub) -> DeviceStatus {
    let mut d = hub.snapshot().devices;
    assert_eq!(d.len(), 1, "{d:?}");
    d.remove(0)
}

/// Step until the fake's time has reached `secs`.
fn until<N: FnMut() -> String>(
    p: &mut Publisher<'_, N>,
    s: &mut FakeSession,
    fake: &FakeDriver,
    secs: f64,
) {
    let t = Duration::from_secs_f64(secs);
    while fake.now() < t && s.phase() != Phase::Stopped {
        p.step(s);
    }
}

/// Step until `done` says so (at most a minute of the fake's time).
fn until_that<N: FnMut() -> String>(
    p: &mut Publisher<'_, N>,
    s: &mut FakeSession,
    fake: &FakeDriver,
    done: impl Fn(&FakeSession) -> bool,
) {
    let limit = fake.now() + Duration::from_secs(60);
    while !done(s) {
        assert!(fake.now() < limit, "it never happened");
        p.step(s);
    }
}

#[test]
fn the_snapshot_follows_the_session() {
    let r = rig();
    r.fake.with(|f| {
        f.attach_error = Some(170); // another process holds the session
        f.connected = false;
    });
    let lookup = r.lookup();
    let Rig {
        fake,
        hub,
        log,
        mut s,
        lookups,
    } = r;
    fake.at(5.0, |f| f.attach_error = None);
    // Seen by the probe at 12 s (the buds are asked for every 2 s).
    fake.at(11.0, |f| f.buds_connect());
    fake.at(13.0, |f| f.set_stream(StreamState::Running, FMT_48K_24));
    // Every WRITE takes longer than a packet plays for a quarter second.
    fake.at(15.0, |f| f.write_delay = Duration::from_millis(20));
    fake.at(15.25, |f| f.write_delay = FAST);
    fake.at(17.0, |f| f.set_stream(StreamState::Running, FMT_96K_24));
    fake.at(18.0, |f| {
        f.set_volume(
            AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP,
            VolumeState {
                avrcp: 64,
                ..VolumeState::default()
            },
        )
    });
    fake.at(18.5, |f| {
        f.set_volume(
            AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP | AUDIO_FLAG_MUTED,
            VolumeState::default(),
        )
    });
    fake.at(19.0, |f| f.buds_disconnect());

    // -- before the first step: the device is there, nothing is known -------
    let mut p = Publisher::start(&hub, ID, lookup, &s);
    let d = dev(&hub);
    assert_eq!((d.id.as_str(), d.name.as_str()), (ID, ""));
    assert_eq!(d.phase, LinkPhase::WaitingDriver);
    assert_eq!(d.phase_reason, "");
    assert_eq!((&d.link, &d.volume, &d.driver), (&None, &None, &None));
    assert_eq!((d.endpoint, &d.last_event), (None, &None));
    assert_eq!(d.counters, Counters::default());

    // -- WaitDriver: why, and the error as the device's last event ----------
    p.step(&mut s);
    let d = dev(&hub);
    assert_eq!(d.phase, LinkPhase::WaitingDriver);
    assert!(
        d.phase_reason
            .starts_with("driver not ready: AUDIO_ATTACH:"),
        "{}",
        d.phase_reason
    );
    let e = d.last_event.expect("the error the session logged");
    assert_eq!(e.level, EventLevel::Error);
    assert!(
        e.text.starts_with("driver not ready: AUDIO_ATTACH:") && e.text.contains("retrying"),
        "{}",
        e.text
    );
    assert!(e.time_unix > 1_600_000_000);
    assert_eq!(hub.snapshot().service.last_event, None);

    // -- WaitLink: attached, the buds are not connected ---------------------
    until(&mut p, &mut s, &fake, 10.5);
    let d = dev(&hub);
    assert_eq!(d.phase, LinkPhase::WaitingBuds);
    assert_eq!(d.phase_reason, "");
    assert_eq!(
        d.driver,
        Some(DriverInfo {
            version: "0.1.0".into(),
            protocol: 3
        })
    );
    assert_eq!(
        d.volume,
        Some(VolumeStatus {
            mode: VolumeMode::Windows,
            level_percent: None,
            muted: false
        })
    );
    assert!(d.link.is_none());
    assert!(d.last_event.is_some(), "the last error stays on show");
    assert_eq!((d.name.as_str(), lookups.get()), ("", 1));

    // -- Ready: the stream is configured ------------------------------------
    until(&mut p, &mut s, &fake, 12.5);
    let d = dev(&hub);
    assert_eq!(d.phase, LinkPhase::Ready, "{}", log.text());
    assert_eq!(d.phase_reason, "");
    assert_eq!(
        (d.name.as_str(), lookups.get()),
        ("Test Buds", 2),
        "asked again once the buds were there"
    );
    let l = d.link.expect("a link");
    assert_eq!(
        (l.mode, l.config, l.caps),
        (CodecMode::Hifi, 0x0C, fake.with(|f| f.sink.ssc_flags))
    );
    assert_eq!((l.stream_rate, l.stream_bits), (48_000, 24));
    assert_eq!((l.bitrate_kbps, l.top_kbps, l.rung), (229, 229, 0));
    assert!(l.adaptive);
    assert_eq!(l.ladder_kbps, [229, 192, 128, 88]);
    assert_eq!((l.frames_per_packet, l.packet_bytes), (1, 588));
    assert_eq!(l.mtu_limit, 1005);
    assert!(l.mtu_reason.contains("capped at 1005"), "{}", l.mtu_reason);
    assert_eq!((l.streaming_ms, l.sink_delay_tenth_ms), (None, None));
    assert_eq!(d.counters, Counters::default());

    // -- Streaming: the numbers run -----------------------------------------
    until(&mut p, &mut s, &fake, 14.0);
    let d = dev(&hub);
    assert_eq!(d.phase, LinkPhase::Streaming);
    assert_eq!(
        d.endpoint,
        Some(PcmFormat {
            sample_rate: 48_000,
            bits: 24,
            channels: 2
        })
    );
    let l = d.link.clone().expect("a link");
    let ms = l.streaming_ms.expect("streaming");
    assert!((750..=1000).contains(&ms), "{ms}: at most 250 ms old");
    let sent = fake.with(|f| f.packets.len() as u64);
    assert!(d.counters.packets > 40 && d.counters.packets <= sent);
    assert!(
        sent - d.counters.packets <= 14,
        "at most 250 ms of packets behind"
    );
    assert!(d.counters.pcm_frames >= d.counters.packets * 864);
    assert_eq!(d.counters.write_avg_us, 200);
    assert_eq!(d.counters.write_max_us, 200);
    assert_eq!(
        (
            d.counters.dropped_frames,
            d.counters.write_errors,
            d.counters.reconnects
        ),
        (0, 0, 0)
    );

    // -- while it streams: at most one status per 250 ms --------------------
    let (g0, t0, n0) = (hub.generation(), fake.now(), fake.with(|f| f.packets.len()));
    until(&mut p, &mut s, &fake, 15.0);
    let statuses = hub.generation() - g0;
    let quarters = ((fake.now() - t0).as_millis() / 250) as u64;
    let packets = fake.with(|f| f.packets.len()) - n0;
    assert!(packets > 40, "{packets} packets went out meanwhile");
    assert!(
        statuses >= quarters.saturating_sub(1) && statuses <= quarters + 1,
        "{statuses} statuses in {quarters} quarter seconds"
    );

    // -- a rate step shows with the step that made it -----------------------
    until_that(&mut p, &mut s, &fake, |s| {
        s.rate().is_some_and(|r| r.kbps() != 229)
    });
    let l = dev(&hub).link.expect("a link");
    assert_eq!((l.bitrate_kbps, l.top_kbps, l.rung), (192, 229, 1));
    assert_eq!(l.packet_bytes, 496);
    assert_eq!(l.ladder_kbps, [229, 192, 128, 88]);
    assert!(l.adaptive);
    assert!(
        fake.now() < Duration::from_millis(15_300),
        "{:?}",
        fake.now()
    );

    // -- a mode switch: another stream, at the top of another ladder --------
    until(&mut p, &mut s, &fake, 17.5);
    let d = dev(&hub);
    assert_eq!(d.phase, LinkPhase::Streaming, "{}", log.text());
    assert_eq!(d.endpoint.map(|f| f.sample_rate), Some(96_000));
    let l = d.link.expect("a link");
    assert_eq!((l.mode, l.config), (CodecMode::Uhq, 0x0A));
    assert_eq!((l.stream_rate, l.stream_bits), (96_000, 24));
    assert_eq!((l.bitrate_kbps, l.top_kbps, l.rung), (584, 584, 0));
    assert_eq!(l.ladder_kbps, [584, 291, 152]);
    assert_eq!((l.frames_per_packet, l.packet_bytes), (1, 672));
    assert!(l.streaming_ms.is_some_and(|ms| ms <= 500));
    assert_eq!(d.counters.reconnects, 0);

    // -- the volume: who applies it, where the slider is, mute --------------
    until(&mut p, &mut s, &fake, 18.3);
    assert_eq!(
        dev(&hub).volume,
        Some(VolumeStatus {
            mode: VolumeMode::Buds,
            level_percent: Some(50),
            muted: false
        })
    );
    until(&mut p, &mut s, &fake, 18.8);
    let v = dev(&hub).volume.expect("a volume");
    assert_eq!((v.level_percent, v.muted), (None, true));

    // -- the buds go away: reconnecting, with the reason and the warning ----
    let before = dev(&hub).counters;
    until_that(&mut p, &mut s, &fake, |s| s.phase() == Phase::WaitLink);
    let at = fake.now().as_millis();
    assert!((19_000..=19_050).contains(&at), "{at}");
    let d = dev(&hub);
    assert_eq!(d.phase, LinkPhase::Reconnecting);
    assert!(
        d.phase_reason.starts_with("link down: "),
        "{}",
        d.phase_reason
    );
    assert!(d.link.is_none());
    assert_eq!(d.counters.reconnects, 1);
    assert!(
        d.counters.packets >= before.packets,
        "the counters outlive the link"
    );
    let e = d.last_event.expect("the warning the session logged");
    assert_eq!(e.level, EventLevel::Warning);
    assert_eq!(e.text, d.phase_reason);
    // ... and are not connected any more.
    until(&mut p, &mut s, &fake, 21.0);
    let d = dev(&hub);
    assert_eq!(d.phase, LinkPhase::WaitingBuds, "{}", log.text());
    assert_eq!(
        d.phase_reason, "",
        "not a failure from when they were there"
    );
    assert!(d.link.is_none());
    assert_eq!(d.name, "Test Buds", "the name stays");
    assert!(
        d.last_event
            .is_some_and(|e| e.text.starts_with("link down: ")),
        "the warning stays on show"
    );

    // -- stop: the device leaves the hub ------------------------------------
    fake.stop_at(22.0);
    until(&mut p, &mut s, &fake, 30.0);
    assert_eq!(s.phase(), Phase::Stopped);
    assert_eq!(dev(&hub).phase, LinkPhase::Stopped);
    p.finish();
    assert!(hub.snapshot().devices.is_empty());
}

#[test]
fn run_published_runs_to_the_stop_and_leaves_nothing_behind() {
    let script = |r: &Rig| {
        r.fake
            .at(1.0, |f| f.set_stream(StreamState::Running, FMT_48K_24));
        // A congested second, so that the rate changes on the way.
        r.fake
            .at(3.0, |f| f.write_delay = Duration::from_millis(20));
        r.fake.at(4.0, |f| f.write_delay = FAST);
        r.fake.stop_at(6.0);
    };
    let mut r = rig();
    script(&r);
    let lookup = r.lookup();
    run_published(&mut r.s, &r.hub, ID, lookup);
    assert_eq!(r.s.phase(), Phase::Stopped);
    assert!(r.hub.snapshot().devices.is_empty());
    assert_eq!(r.log.find("link down").len(), 0);
    // The same session without a status feed sends the same packets at the
    // same times: publishing changes nothing about the audio.
    let mut plain = rig();
    script(&plain);
    plain.s.run();
    let sent = |r: &Rig| r.fake.with(|f| (f.packets.clone(), f.packet_times.clone()));
    let (packets, times) = sent(&r);
    assert!(packets.len() > 250, "{}", packets.len());
    assert!(packets.iter().any(|p| p.len() == 588) && packets.iter().any(|p| p.len() < 588));
    assert_eq!((packets, times), sent(&plain));
    assert_eq!(r.log.text(), plain.log.text());
}

#[test]
fn a_setup_that_fails_is_reconnecting_with_its_reason() {
    let Rig {
        fake, hub, mut s, ..
    } = rig();
    fake.with(|f| f.sink.reject_open = Some(0x31));
    let mut p = Publisher::start(&hub, ID, String::new, &s);
    until(&mut p, &mut s, &fake, 0.5);
    let d = dev(&hub);
    assert_eq!(d.phase, LinkPhase::Reconnecting);
    assert!(
        d.phase_reason.starts_with("cannot set up the stream: "),
        "{}",
        d.phase_reason
    );
    assert_eq!(d.counters.reconnects, 0, "there was no link to lose");
    assert_eq!(
        d.last_event.map(|e| e.level),
        Some(EventLevel::Error),
        "logged as an error"
    );
    // It works at the next attempt: nothing is left of the reason.
    fake.with(|f| f.sink.reject_open = None);
    until(&mut p, &mut s, &fake, 2.0);
    let d = dev(&hub);
    assert_eq!((d.phase, d.phase_reason.as_str()), (LinkPhase::Ready, ""));
    fake.stop_at(3.0);
    until(&mut p, &mut s, &fake, 4.0);
    p.finish();
}

/// A status by hand: the conversion alone.
fn hand_made() -> SessionStatus {
    let fmt = FMT_96K_24;
    let plan = select_plan(
        fmt,
        avdtp_flags(0x3E),
        &Settings {
            uhq_bitrate: Bitrate::K584,
            ..Settings::default()
        },
    )
    .unwrap();
    let top = Rate {
        bitrate: plan.bitrate,
        frames_per_packet: plan.frames_per_packet,
    };
    let ladder = Ladder::new(plan.encoder, top, 1005, true, true);
    let rate = ladder.rungs()[1];
    let audio = AudioState {
        seq: 4,
        flags: AUDIO_FLAG_ACX | AUDIO_FLAG_HW_VOLUME,
        stream: StreamState::Running,
        format: fmt,
        frames_rendered: 1_000_000,
        frames_dropped: 480,
        volume: VolumeState {
            avrcp: 127,
            ..VolumeState::default()
        },
    };
    SessionStatus {
        phase: Phase::Streaming,
        connected: Some(true),
        retrying: false,
        reason: "left over".into(),
        endpoint: Some(fmt),
        link: Some(LinkView {
            plan,
            caps: 0x3E,
            ladder,
            rate,
            rung: 1,
            mtu_limit: 1005,
            mtu_why: "because".into(),
            streaming_for: Some(Duration::from_millis(61_500)),
        }),
        sink_delay: Some(2500),
        volume: crossc_core::volume::decide(&audio),
        audio,
        driver: Some(((2 << 24) | (7 << 16) | 300, 3)),
        totals: Totals {
            packets: 1000,
            frames_in: 864_000,
            discontinuities: 2,
            write_errors: 1,
            late_packets: 5,
            reconnects: 3,
            dropped_frames: 77,
            write_total: Duration::from_micros(250_000),
            write_max: Duration::from_micros(9000),
            write_count: 500,
        },
    }
}

fn avdtp_flags(octet: u8) -> avdtp::codec::SscFlags {
    avdtp::codec::SscFlags(octet)
}

#[test]
fn every_field_of_a_status_is_converted() {
    let st = hand_made();
    let d = to_device_status(ID, "Some Buds", &st);
    assert_eq!(
        d,
        DeviceStatus {
            id: ID.into(),
            name: "Some Buds".into(),
            phase: LinkPhase::Streaming,
            phase_reason: String::new(), // none while there is a link
            endpoint: Some(PcmFormat {
                sample_rate: 96_000,
                bits: 24,
                channels: 2
            }),
            link: Some(LinkStatus {
                mode: CodecMode::Uhq,
                config: 0x0A,
                caps: 0x3E,
                stream_rate: 96_000,
                stream_bits: 24,
                bitrate_kbps: 291,
                top_kbps: 584,
                adaptive: true,
                ladder_kbps: vec![584, 291, 152],
                rung: 1,
                frames_per_packet: 2,
                packet_bytes: 12
                    + 2 * ssc_enc::frame_size(ssc_enc::Mode::Uhq96k, Bitrate::K291) as u32,
                mtu_limit: 1005,
                mtu_reason: "because".into(),
                sink_delay_tenth_ms: Some(2500),
                streaming_ms: Some(61_500),
            }),
            volume: Some(VolumeStatus {
                mode: VolumeMode::Software,
                level_percent: Some(100),
                muted: false
            }),
            counters: Counters {
                packets: 1000,
                pcm_frames: 864_000,
                // The session's count, not the driver's lifetime counter
                // (480 in the audio state).
                dropped_frames: 77,
                discontinuities: 2,
                write_errors: 1,
                write_avg_us: 500,
                write_max_us: 9000,
                late_packets: 5,
                reconnects: 3,
            },
            driver: Some(DriverInfo {
                version: "2.7.300".into(),
                protocol: 3
            }),
            last_event: None,
        }
    );
}

#[test]
fn phases_modes_and_volume_levels() {
    let base = hand_made();
    let phase = |phase, connected| {
        let st = SessionStatus {
            phase,
            connected,
            link: None,
            ..base.clone()
        };
        let d = to_device_status(ID, "", &st);
        (d.phase, d.phase_reason)
    };
    let why = || "left over".to_string();
    assert_eq!(
        phase(Phase::WaitDriver, None),
        (LinkPhase::WaitingDriver, why())
    );
    assert_eq!(
        phase(Phase::WaitLink, None),
        (LinkPhase::WaitingBuds, why())
    );
    assert_eq!(
        phase(Phase::WaitLink, Some(false)),
        (LinkPhase::WaitingBuds, why())
    );
    assert_eq!(
        phase(Phase::WaitLink, Some(true)),
        (LinkPhase::Reconnecting, why())
    );
    assert_eq!(
        phase(Phase::Ready, Some(true)),
        (LinkPhase::Ready, String::new())
    );
    assert_eq!(
        phase(Phase::Stopped, Some(true)),
        (LinkPhase::Stopped, why())
    );
    // The codec modes, and a ladder of one rate.
    for (octet, fmt, mode, bits, rate) in [
        (0x3Eu8, FMT_48K_24, CodecMode::Hifi, 24u8, 48_000u32),
        (0x38, FMT_48K_24, CodecMode::Ssc, 16, 48_000),
        (0x28, FMT_48K_24, CodecMode::Ssc, 16, 44_100),
    ] {
        let settings = Settings {
            adaptive: false,
            ..Settings::default()
        };
        let plan = select_plan(fmt, avdtp_flags(octet), &settings).unwrap();
        let top = Rate {
            bitrate: plan.bitrate,
            frames_per_packet: 1,
        };
        let ladder = Ladder::new(plan.encoder, top, 1005, false, true);
        let st = SessionStatus {
            link: Some(LinkView {
                plan,
                caps: octet,
                ladder,
                rate: top,
                rung: 0,
                mtu_limit: 1005,
                mtu_why: String::new(),
                streaming_for: None,
            }),
            ..base.clone()
        };
        let l = to_device_status(ID, "", &st).link.unwrap();
        assert_eq!((l.mode, l.stream_bits, l.stream_rate), (mode, bits, rate));
        assert_eq!((l.bitrate_kbps, l.top_kbps), (229, 229));
        assert_eq!((l.adaptive, l.ladder_kbps.as_slice()), (false, &[229][..]));
        assert_eq!(l.streaming_ms, None);
    }
    // The slider: the driver's AVRCP value as a percentage, when there is
    // a hardware volume and it is not muted.
    let volume = |flags: u32, avrcp: u8| {
        let audio = AudioState {
            flags,
            volume: VolumeState {
                avrcp,
                ..VolumeState::default()
            },
            ..base.audio
        };
        let st = SessionStatus {
            volume: crossc_core::volume::decide(&audio),
            audio,
            ..base.clone()
        };
        let v = to_device_status(ID, "", &st).volume.unwrap();
        (v.mode, v.level_percent, v.muted)
    };
    let buds = AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP;
    assert_eq!(volume(buds, 0), (VolumeMode::Buds, Some(0), false));
    assert_eq!(volume(buds, 1), (VolumeMode::Buds, Some(1), false));
    assert_eq!(volume(buds, 64), (VolumeMode::Buds, Some(50), false));
    assert_eq!(volume(buds, 127), (VolumeMode::Buds, Some(100), false));
    assert_eq!(volume(buds, 255), (VolumeMode::Buds, Some(100), false));
    assert_eq!(
        volume(buds | AUDIO_FLAG_MUTED, 0),
        (VolumeMode::Buds, None, true)
    );
    assert_eq!(
        volume(AUDIO_FLAG_HW_VOLUME, 32),
        (VolumeMode::Software, Some(25), false)
    );
    assert_eq!(volume(0, 90), (VolumeMode::Windows, None, false));
    // Not attached: nothing is known of the volume or the driver.
    let st = SessionStatus {
        driver: None,
        ..base.clone()
    };
    let d = to_device_status(ID, "", &st);
    assert_eq!((d.volume, d.driver), (None, None));
    // A session that has sent nothing divides by nothing.
    let st = SessionStatus {
        totals: Totals::default(),
        ..base
    };
    assert_eq!(to_device_status(ID, "", &st).counters.write_avg_us, 0);
}

#[test]
fn warnings_and_errors_become_the_last_event() {
    let hub = new_hub(String::new());
    let inner = Arc::new(MemoryLog::default());
    let log = StatusLog::new(inner.clone(), hub.clone());
    hub.set_device(DeviceStatus {
        id: ID.into(),
        ..DeviceStatus::default()
    });
    // Information is passed on and is no event.
    log.log(Level::Info, &format!("[{ID}] link up"));
    log.log(Level::Debug, "nothing");
    assert_eq!(dev(&hub).last_event, None);
    assert_eq!(hub.snapshot().service.last_event, None);
    // A device's line belongs to the device, without its prefix.
    log.log(Level::Warn, &format!("[{ID}] link down: sink sent CLOSE"));
    let e = dev(&hub).last_event.expect("an event");
    assert_eq!(
        (e.level, e.text.as_str()),
        (EventLevel::Warning, "link down: sink sent CLOSE")
    );
    assert_eq!(hub.snapshot().service.last_event, None);
    // A line of no device is the service's, whole.
    log.log(Level::Error, "cannot list the crossc device interfaces: x");
    let e = hub.snapshot().service.last_event.expect("an event");
    assert_eq!(
        (e.level, e.text.as_str()),
        (
            EventLevel::Error,
            "cannot list the crossc device interfaces: x"
        )
    );
    // A device the hub does not know (its thread could not be started, or
    // panicked and took the device with it): there is no entry to show the
    // line under, so it is the service's, and says which device it means.
    log.log(Level::Error, "[112233445566] cannot start a session thread");
    assert_eq!(hub.snapshot().devices.len(), 1);
    let e = hub.snapshot().service.last_event.expect("an event");
    assert_eq!(
        (e.level, e.text.as_str()),
        (
            EventLevel::Error,
            "[112233445566] cannot start a session thread"
        )
    );
    assert!(
        dev(&hub)
            .last_event
            .is_some_and(|e| e.text.starts_with("link down")),
        "the other device keeps its own"
    );
    // Information about such a device is still no event.
    log.log(Level::Info, "[112233445566] device interface found");
    assert!(
        hub.snapshot()
            .service
            .last_event
            .is_some_and(|e| e.text.ends_with("session thread"))
    );
    // The statuses a session sets keep the event.
    hub.set_device_keeping_event(DeviceStatus {
        id: ID.into(),
        phase: LinkPhase::Ready,
        ..DeviceStatus::default()
    });
    assert!(dev(&hub).last_event.is_some());
    // Every line reached the log itself, unchanged.
    assert_eq!(inner.lines.lock().unwrap().len(), 6);
    assert_eq!(inner.find(&format!("[{ID}] link down")).len(), 1);
    // What the log does not write is not asked for either.
    assert!(log.enabled(Level::Debug));
    let quiet = StatusLog::new(Arc::new(NullLog), hub);
    assert!(!quiet.enabled(Level::Error));
}

/// What a session thread that panics leaves in the feed: `host.rs` takes the
/// device out and then logs the error.
#[test]
fn a_session_that_ended_badly_is_the_service_s_last_event() {
    let Rig {
        fake, hub, mut s, ..
    } = rig();
    let log = StatusLog::new(Arc::new(NullLog), hub.clone());
    let mut p = Publisher::start(&hub, ID, String::new, &s);
    until(&mut p, &mut s, &fake, 0.5);
    assert_eq!(hub.snapshot().devices.len(), 1);
    hub.remove_device(ID);
    log.log(
        Level::Error,
        &format!("[{ID}] the session thread panicked; it will be restarted"),
    );
    let snap = hub.snapshot();
    assert!(snap.devices.is_empty());
    let e = snap.service.last_event.expect("the error is on show");
    assert_eq!(e.level, EventLevel::Error);
    assert_eq!(
        e.text,
        format!("[{ID}] the session thread panicked; it will be restarted")
    );
}

#[test]
fn the_dropped_frames_on_show_are_the_session_s_not_the_driver_s() {
    let Rig {
        fake, hub, mut s, ..
    } = rig();
    // What the driver dropped before this session, and while the sink had
    // the stream suspended and nobody read (1.5 to 3.5 s).
    fake.with(|f| f.drop_frames(5000));
    fake.at(1.0, |f| f.set_stream(StreamState::Running, FMT_48K_24));
    fake.at(1.5, |f| f.sink_suspends());
    fake.at(4.105, |f| f.drop_frames(1000));
    fake.at(4.5, |f| f.set_stream(StreamState::Paused, FMT_48K_24));
    let mut p = Publisher::start(&hub, ID, String::new, &s);
    until(&mut p, &mut s, &fake, 4.0);
    assert!(fake.with(|f| f.state.frames_dropped) > 50_000);
    let d = dev(&hub);
    assert_eq!(d.phase, LinkPhase::Streaming);
    assert_eq!(d.counters.dropped_frames, 0);
    // Lost from the started stream: that is what the number is for.
    until(&mut p, &mut s, &fake, 4.6);
    assert_eq!(dev(&hub).counters.dropped_frames, 1000);
    fake.stop_at(5.0);
    until(&mut p, &mut s, &fake, 6.0);
    p.finish();
}

#[cfg(windows)]
mod pipe {
    use super::*;
    use crate::status::win::{check_pipe, start_server_on, verdict};
    use crossc_status::pipe::{Held, Problem, Security, Server, ServerOptions, Trust, read_once};

    fn pipe_name(tag: &str) -> String {
        format!(
            r"\\.\pipe\crossc-status-test-{}-service-{tag}",
            std::process::id()
        )
    }

    fn serve(hub: &Hub, name: &str) -> Server {
        Server::start(
            hub.clone(),
            ServerOptions {
                pipe_name: name.to_string(),
                security: Security::CurrentUser,
                ..ServerOptions::default()
            },
        )
        .expect("start the status server")
    }

    #[test]
    fn a_session_reaches_a_client_of_the_pipe() {
        let name = pipe_name("feed");
        let Rig {
            fake, hub, mut s, ..
        } = rig();
        let _server = serve(&hub, &name);
        // The server makes the pipe on its own thread: wait for it.
        assert_eq!(
            check_pipe(&name, Trust::ServiceOrSelf, 250, Duration::from_millis(20)),
            Ok(())
        );
        fake.at(1.0, |f| f.set_stream(StreamState::Running, FMT_48K_24));
        let mut p = Publisher::start(&hub, ID, || "Test Buds".to_string(), &s);
        until(&mut p, &mut s, &fake, 2.0);
        let snap = read_once(&name, Duration::from_secs(10), Trust::ServiceOrSelf)
            .expect("a snapshot from the pipe");
        assert_eq!(snap.service.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(snap.devices.len(), 1);
        let d = &snap.devices[0];
        assert_eq!((d.id.as_str(), d.name.as_str()), (ID, "Test Buds"));
        assert_eq!(d.phase, LinkPhase::Streaming);
        let l = d.link.as_ref().expect("a link");
        assert_eq!((l.mode, l.bitrate_kbps), (CodecMode::Hifi, 229));
        assert_eq!(l.ladder_kbps, [229, 192, 128, 88]);
        assert!(d.counters.packets > 40);
        // The session ends: the next snapshot has no device.
        fake.stop_at(2.5);
        until(&mut p, &mut s, &fake, 3.0);
        p.finish();
        let snap = read_once(&name, Duration::from_secs(10), Trust::ServiceOrSelf)
            .expect("a snapshot from the pipe");
        assert!(snap.devices.is_empty());
    }

    #[test]
    fn the_pipe_check_is_the_tray_s_own_test() {
        // No such pipe: given up after the attempts.
        let t0 = std::time::Instant::now();
        assert_eq!(
            check_pipe(
                &pipe_name("none"),
                Trust::Service,
                3,
                Duration::from_millis(20)
            ),
            Err(Problem::NotRunning)
        );
        assert!(t0.elapsed() >= Duration::from_millis(40));
        // A pipe of this user: fine for a client that trusts itself, and
        // found although the server makes it on its own thread.
        let name = pipe_name("check");
        let hub = new_hub(String::new());
        let _server = serve(&hub, &name);
        assert_eq!(
            check_pipe(&name, Trust::ServiceOrSelf, 250, Duration::from_millis(20)),
            Ok(())
        );
        // The tray trusts only SYSTEM's or Administrators' pipe, which a
        // plain user's is not: that is what the service's log would say.
        match check_pipe(&name, Trust::Service, 50, Duration::from_millis(20)) {
            Err(Problem::Untrusted) => {}
            Ok(()) => eprintln!("elevated: this pipe is Administrators', the check passes"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn starting_the_server_says_once_whether_the_tray_will_trust_it() {
        let name = pipe_name("start");
        let hub = new_hub(String::new());
        let log = Arc::new(MemoryLog::default());
        // The start itself does not wait for the pipe.
        let t0 = std::time::Instant::now();
        let server = start_server_on(&hub, &name, Security::CurrentUser, log.clone());
        assert!(t0.elapsed() < Duration::from_secs(2));
        assert!(server.is_some());
        // The verdict comes from its own thread, once.
        let lines = || log.find("status pipe");
        let t0 = std::time::Instant::now();
        while lines().is_empty() && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));
        let said = lines();
        assert_eq!(said.len(), 1, "{said:?}");
        let line = &said[0].1;
        assert!(line.contains(&name), "{line}");
        // A plain user's pipe is not one the tray believes; an elevated
        // run's is.
        assert!(
            line.contains("is not owned by SYSTEM or Administrators") || line.ends_with("is up"),
            "{line}"
        );
        drop(server);
        // The feed does not report on itself: nothing of this is an event.
        assert_eq!(hub.snapshot().service.last_event, None);
    }

    #[test]
    fn a_name_another_program_took_is_said_as_that_not_as_an_owner_problem() {
        // Another program has the name before the service starts (here:
        // a server of this test's own).
        let name = pipe_name("taken");
        let squatter = serve(&new_hub(String::new()), &name);
        assert_eq!(
            check_pipe(&name, Trust::ServiceOrSelf, 250, Duration::from_millis(20)),
            Ok(())
        );
        let hub = new_hub(String::new());
        let log = Arc::new(MemoryLog::default());
        let server = start_server_on(&hub, &name, Security::CurrentUser, log.clone());
        assert!(server.is_some());
        let said = || log.find(&name);
        let t0 = std::time::Instant::now();
        while said().len() < 2 && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));
        let said = said();
        assert_eq!(said.len(), 2, "{said:?}");
        // The server: it cannot create the pipe, and why.
        assert!(
            said.iter().any(|(_, l)| l.contains("cannot be created")
                && l.contains("another process holds that name")),
            "{said:?}"
        );
        // The check: what that means. The other program's pipe is neither
        // called the service's with a wrong owner, nor "up".
        assert!(
            said.iter()
                .any(|(_, l)| l.contains("exists that is not this service's")
                    && l.contains("closed")),
            "{said:?}"
        );
        assert!(
            !said
                .iter()
                .any(|(_, l)| l.contains("is not owned by") || l.ends_with("is up")),
            "{said:?}"
        );
        drop(server);
        drop(squatter);
    }

    #[test]
    fn the_verdict_goes_by_whose_pipe_it_is() {
        let name = r"\\.\pipe\x";
        let never = || -> Result<(), Problem> { panic!("another program's pipe is not opened") };
        let text = |whose, open: Result<(), Problem>| verdict(name, whose, || open).map(|v| v.1);
        // The service's own pipe: the tray's test decides.
        assert_eq!(
            verdict(name, Held::Ours, || Ok(())),
            Some((Level::Info, format!("status pipe {name} is up")))
        );
        let line = text(Held::Ours, Err(Problem::Untrusted)).unwrap();
        assert!(
            line.contains("is not owned by SYSTEM or Administrators"),
            "{line}"
        );
        let line = text(Held::Ours, Err(Problem::Denied)).unwrap();
        assert!(line.contains("could not be checked"), "{line}");
        assert_eq!(text(Held::Ours, Err(Problem::NotRunning)), None);
        // Another program's: said as that, whatever opening it would give
        // (an elevated squatter's pipe would pass the owner test).
        let (level, line) = verdict(name, Held::Taken, never).unwrap();
        assert_eq!(level, Level::Warn);
        assert!(
            line.contains("another program created it") && line.contains("Status not trusted"),
            "{line}"
        );
        // The server's own report covers the rest.
        assert_eq!(verdict(name, Held::Failed, never), None);
        assert_eq!(verdict(name, Held::NotYet, never), None);
    }
}
