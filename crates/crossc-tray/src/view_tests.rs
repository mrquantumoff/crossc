use super::*;
use crate::demo;
use crossc_status::{Counters, DriverInfo, ServiceInfo};

fn live(name: &str) -> Feed {
    demo::state(name, 0).unwrap()
}

fn device(name: &str) -> DeviceStatus {
    match live(name) {
        Feed::Live(s) => s.devices[0].clone(),
        other => panic!("{other:?}"),
    }
}

fn value<'a>(c: &'a Card, label: &str) -> &'a Row {
    c.sections
        .iter()
        .flat_map(|s| &s.rows)
        .find(|r| r.label == label)
        .unwrap_or_else(|| panic!("no row {label}"))
}

#[test]
fn small_formatters() {
    assert_eq!(rate(96_000), "96 kHz");
    assert_eq!(rate(48_000), "48 kHz");
    assert_eq!(rate(44_100), "44.1 kHz");
    assert_eq!(rate(0), "0 kHz");
    assert_eq!(grouped(0), "0");
    assert_eq!(grouped(999), "999");
    assert_eq!(grouped(1_000), "1,000");
    assert_eq!(grouped(5_788_800), "5,788,800");
    assert_eq!(grouped(u64::MAX), "18,446,744,073,709,551,615");
    assert_eq!(span(0), "0 s");
    assert_eq!(span(59), "59 s");
    assert_eq!(span(185), "3 min 05 s");
    assert_eq!(span(7_620), "2 h 07 min");
    assert_eq!(span(356_400), "4 d 3 h");
    assert_eq!(millis(310), "0.31 ms");
    assert_eq!(millis(4_200), "4.2 ms");
    assert_eq!(millis(120_000), "120 ms");
}

#[test]
fn the_icon_follows_the_state() {
    assert_eq!(glyph(&Feed::Connecting), Glyph::Off);
    assert_eq!(glyph(&Feed::Down(Outage::NotRunning)), Glyph::Off);
    assert_eq!(glyph(&Feed::Down(Outage::Lost)), Glyph::Waiting);
    assert_eq!(glyph(&Feed::Down(Outage::Busy)), Glyph::Waiting);
    for o in [
        Outage::Denied,
        Outage::Unreadable,
        Outage::Version { theirs: 2 },
    ] {
        assert_eq!(glyph(&Feed::Down(o)), Glyph::Problem);
    }
    assert_eq!(glyph(&live("nobuds")), Glyph::Waiting);
    assert_eq!(glyph(&live("waiting")), Glyph::Waiting);
    assert_eq!(glyph(&live("reconnect")), Glyph::Waiting);
    assert_eq!(glyph(&live("hifi")), Glyph::Streaming(Quality::Hifi));
    assert_eq!(glyph(&live("uhq")), Glyph::Streaming(Quality::Uhq));
}

#[test]
fn with_two_devices_the_one_that_plays_gives_the_icon() {
    let Feed::Live(mut s) = live("two") else {
        panic!()
    };
    assert_eq!(
        glyph(&Feed::Live(s.clone())),
        Glyph::Streaming(Quality::Uhq)
    );
    // Whatever their order.
    s.devices.reverse();
    assert_eq!(
        glyph(&Feed::Live(s.clone())),
        Glyph::Streaming(Quality::Uhq)
    );
    // The UHQ one stops playing: both are ready, the better mode shows.
    s.devices[1].phase = LinkPhase::Ready;
    assert_eq!(glyph(&Feed::Live(s.clone())), Glyph::Ready(Quality::Uhq));
    // The HiFi one starts: streaming beats ready, whatever the mode.
    s.devices[0].phase = LinkPhase::Streaming;
    assert_eq!(
        glyph(&Feed::Live(s.clone())),
        Glyph::Streaming(Quality::Hifi)
    );
    // A phase that says "streaming" without a stream is not believed.
    s.devices[0].link = None;
    assert_eq!(glyph(&Feed::Live(s.clone())), Glyph::Ready(Quality::Uhq));
    // A mode this version does not know still shows as a stream.
    let mut d = device("uhq");
    d.link.as_mut().unwrap().mode = CodecMode::Unknown;
    assert_eq!(device_glyph(&d), Glyph::Streaming(Quality::Ssc));
}

#[test]
fn tooltips() {
    assert_eq!(
        tooltip(&live("uhq")),
        "Buds4 Pro — UHQ 96 kHz/24 · 584 kbps · MTU 1005"
    );
    assert_eq!(
        tooltip(&live("hifi")),
        "Buds4 Pro — HiFi 48 kHz/24 · 192 kbps · MTU 1005"
    );
    assert_eq!(
        tooltip(&live("adaptive")),
        "Buds4 Pro — UHQ 96 kHz/24 · 291 kbps · MTU 1005"
    );
    // Buds that are not connected are not listed.
    assert_eq!(tooltip(&live("waiting")), "crossc — buds not connected");
    assert!(cards(&live("waiting")).is_empty());
    assert_eq!(glyph(&live("waiting")), Glyph::Waiting);
    assert_eq!(tooltip(&live("reconnect")), "Buds4 Pro — reconnecting");
    assert_eq!(
        tooltip(&live("two")),
        "Buds4 Pro — UHQ 96 kHz/24 · 584 kbps · MTU 1005\n\
         Buds3 Pro — HiFi 48 kHz/24 · 192 kbps · MTU 1005, idle"
    );
    // Two pairs of buds fit what the shell takes, uncut.
    assert!(tooltip(&live("two")).encode_utf16().count() < TOOLTIP_MAX);
    assert_eq!(
        tooltip(&Feed::Down(Outage::Untrusted)),
        "crossc — status not trusted"
    );
    assert_eq!(tooltip(&live("nobuds")), "crossc — no buds");
    assert_eq!(
        tooltip(&Feed::Down(Outage::NotRunning)),
        "crossc — service not running"
    );
    assert_eq!(
        tooltip(&Feed::Down(Outage::Version { theirs: 2 })),
        "crossc — version mismatch"
    );
    assert_eq!(tooltip(&Feed::Connecting), "crossc — connecting");
    // 44.1 kHz plain SSC, a device without a name.
    let mut d = device("hifi");
    d.name.clear();
    let l = d.link.as_mut().unwrap();
    l.mode = CodecMode::Ssc;
    l.stream_rate = 44_100;
    l.stream_bits = 16;
    l.bitrate_kbps = 229;
    assert_eq!(
        device_line(&d),
        "AABBCCDDEEFF — SSC 44.1 kHz/16 · 229 kbps · MTU 1005"
    );
}

#[test]
fn a_tooltip_never_exceeds_what_the_shell_takes() {
    let Feed::Live(mut s) = live("two") else {
        panic!()
    };
    let d = s.devices[0].clone();
    s.devices = (0..8)
        .map(|i| DeviceStatus {
            name: format!("Somebody's very own Galaxy Buds4 Pro number {i} 🎧"),
            ..d.clone()
        })
        .collect();
    let t = tooltip(&Feed::Live(s));
    assert_eq!(t.encode_utf16().count(), TOOLTIP_MAX);
    assert!(t.ends_with('…'));
    assert!(t.starts_with("Somebody's very own Galaxy Buds4 Pro number 0 🎧 — UHQ"));

    assert_eq!(fit_utf16("short", 127), "short");
    assert_eq!(fit_utf16("abcdef", 6), "abcdef");
    assert_eq!(fit_utf16("abcdefg", 6), "abcde…");
    // A surrogate pair is never cut in half: 🎧 is two units.
    assert_eq!(fit_utf16("ab🎧🎧🎧", 5), "ab🎧…");
    assert_eq!(fit_utf16("ab🎧🎧🎧", 4), "ab…");
    assert_eq!(fit_utf16("abc", 0), "");
    assert_eq!(fit_utf16("abc", 1), "…");
    for max in 0..40 {
        assert!(fit_utf16("a🎧é — b🎧c · d🎧e", max).encode_utf16().count() <= max);
    }
}

#[test]
fn the_name_is_shortened_only_for_the_tooltip() {
    let mut d = device("uhq");
    assert_eq!(short_name(&d), "Buds4 Pro");
    assert_eq!(card(&d, 0).title, "Galaxy Buds4 Pro");
    d.name = "Galaxy ".into();
    assert_eq!(short_name(&d), "Galaxy");
    d.name = "Sam's Buds".into();
    assert_eq!(short_name(&d), "Sam's Buds");
}

fn labels(c: &Card) -> Vec<&'static str> {
    c.sections
        .iter()
        .flat_map(|s| &s.rows)
        .map(|r| r.label)
        .collect()
}

fn stat_values(c: &Card) -> Vec<(&'static str, &str, Tone)> {
    c.stats
        .iter()
        .map(|s| (s.label, s.value.as_str(), s.tone))
        .collect()
}

#[test]
fn the_card_of_a_uhq_stream() {
    let c = card(&device("uhq"), 0);
    assert_eq!(c.title, "Galaxy Buds4 Pro");
    assert_eq!((c.badge.as_str(), c.badge_tone), ("Streaming", Tone::Good));
    assert_eq!(c.summary, "UHQ (Ultra High Quality) · 24-bit");
    // The four numbers that matter most are at the top, the media MTU
    // among them.
    assert_eq!(
        stat_values(&c),
        [
            ("Mode", "UHQ", Tone::Normal),
            ("Sample rate", "96 kHz", Tone::Normal),
            ("Bitrate", "584 kbps", Tone::Normal),
            ("Media MTU", "1005 B", Tone::Normal),
        ]
    );
    assert_eq!(
        c.sections.iter().map(|s| s.title).collect::<Vec<_>>(),
        ["Audio", "Link", "Activity"]
    );
    assert_eq!(value(&c, "Windows format").value, "96 kHz · 24-bit · 2 ch");
    let sent = value(&c, "Sent to the buds");
    assert_eq!(
        (sent.value.as_str(), sent.detail.as_deref()),
        ("96 kHz · 24-bit", None)
    );
    let rate = value(&c, "Bitrate");
    assert_eq!(rate.value, "584 kbps");
    assert_eq!(
        rate.detail.as_deref(),
        Some("adaptive · ladder 584 / 291 / 152")
    );
    assert_eq!(rate.tone, Tone::Normal);
    let delay = value(&c, "Delay");
    assert_eq!(delay.value, "150.0 ms");
    assert_eq!(delay.detail.as_deref(), Some("reported by the buds"));
    let vol = value(&c, "Volume");
    assert_eq!(vol.value, "62 % · set on the buds");
    assert_eq!(value(&c, "Packet").value, "672 bytes · 1 frame");
    let mtu = value(&c, "Media MTU");
    assert_eq!(mtu.value, "1005 bytes");
    assert_eq!(
        mtu.detail.as_deref(),
        Some("the largest packet this link takes")
    );
    let options = value(&c, "Codec options");
    assert_eq!(options.value, "0x0a of 0x3e");
    assert_eq!(
        options.detail.as_deref(),
        Some("in use, of what the buds offer")
    );
    let driver = value(&c, "Driver");
    assert_eq!(driver.value, "0.2.0");
    assert_eq!(driver.detail.as_deref(), Some("interface version 3"));
    // A stream without a reason has no State row: the badge says it.
    assert!(!labels(&c).contains(&"State"));
    assert_eq!(value(&c, "Streaming for").value, "1 min 01 s");
    assert_eq!(value(&c, "Write latency").value, "0.31 ms avg · 3.9 ms max");
    assert_eq!(value(&c, "Reconnects").value, "0");
    assert_eq!(c.note, None);
}

#[test]
fn the_numbers_at_the_top() {
    let stats = |d: &DeviceStatus| {
        card(d, 0)
            .stats
            .iter()
            .map(|s| (s.value.clone(), s.tone))
            .collect::<Vec<_>>()
    };
    let n = |v: &str| (v.to_string(), Tone::Normal);
    assert_eq!(
        stats(&device("hifi")),
        [n("HiFi"), n("48 kHz"), n("192 kbps"), n("1005 B")]
    );
    // Stepped down: the bitrate is amber here as in its row.
    assert_eq!(
        stats(&device("adaptive")),
        [
            n("UHQ"),
            n("96 kHz"),
            ("291 kbps".to_string(), Tone::Warn),
            n("1005 B")
        ]
    );
    // Plain SSC at 44.1 kHz, a mode and an MTU the service did not name.
    let mut d = device("hifi");
    let l = d.link.as_mut().unwrap();
    l.mode = CodecMode::Ssc;
    l.stream_rate = 44_100;
    l.mtu_limit = 672;
    assert_eq!(stats(&d)[..2], [n("SSC"), n("44.1 kHz")]);
    assert_eq!(stats(&d)[3], n("672 B"));
    let l = d.link.as_mut().unwrap();
    l.mode = CodecMode::Unknown;
    l.mtu_limit = 0;
    l.top_kbps = 0;
    assert_eq!(stats(&d)[0], n("Unknown"));
    assert_eq!(stats(&d)[2], n("192 kbps"));
    assert_eq!(stats(&d)[3], ("unknown".to_string(), Tone::Muted));
    assert_eq!(
        device_line(&d),
        "Buds4 Pro — unknown mode 44.1 kHz/24 · 192 kbps"
    );
    // No stream, no numbers.
    assert!(card(&device("waiting"), 0).stats.is_empty());
    assert!(card(&device("reconnect"), 0).stats.is_empty());
    // Every value fits its tile (a quarter of the card).
    for name in ["hifi", "uhq", "adaptive", "two"] {
        for c in cards(&live(name)) {
            assert_eq!(c.stats.len(), 4);
            assert!(c.stats.iter().all(|s| s.value.chars().count() <= 8));
        }
    }
}

#[test]
fn the_bitrate_row_says_where_the_ladder_stands() {
    let c = card(&device("adaptive"), 0);
    let rate = value(&c, "Bitrate");
    assert_eq!(rate.value, "291 kbps");
    assert_eq!(rate.tone, Tone::Warn);
    assert_eq!(
        rate.detail.as_deref(),
        Some("stepped down · step 2 of 3 · top 584")
    );
    assert_eq!(value(&c, "Late packets").tone, Tone::Warn);

    let mut d = device("uhq");
    let l = d.link.as_mut().unwrap();
    l.adaptive = false;
    assert_eq!(
        value(&card(&d, 0), "Bitrate").detail.as_deref(),
        Some("fixed")
    );
    // Adaptive with a single rate, or without a ladder at all.
    let l = d.link.as_mut().unwrap();
    l.adaptive = true;
    l.ladder_kbps = vec![584];
    assert_eq!(
        value(&card(&d, 0), "Bitrate").detail.as_deref(),
        Some("adaptive · at the top")
    );
    let l = d.link.as_mut().unwrap();
    l.ladder_kbps.clear();
    l.top_kbps = 0;
    assert_eq!(
        value(&card(&d, 0), "Bitrate").detail.as_deref(),
        Some("adaptive · at the top")
    );
    // Below the top with a rung that makes no sense: no step is claimed.
    let l = d.link.as_mut().unwrap();
    l.top_kbps = 584;
    l.bitrate_kbps = 152;
    l.rung = u32::MAX;
    assert_eq!(
        value(&card(&d, 0), "Bitrate").detail.as_deref(),
        Some("stepped down · top 584")
    );
}

#[test]
fn what_is_sent_names_the_conversion() {
    let mut d = device("hifi");
    let sent = |d: &DeviceStatus| value(&card(d, 0), "Sent to the buds").detail.clone();
    assert_eq!(sent(&d), None);
    d.endpoint = Some(PcmFormat {
        sample_rate: 96_000,
        bits: 24,
        channels: 2,
    });
    assert_eq!(sent(&d).as_deref(), Some("resampled from 96 kHz"));
    d.endpoint = Some(PcmFormat {
        sample_rate: 48_000,
        bits: 16,
        channels: 2,
    });
    assert_eq!(sent(&d).as_deref(), Some("16-bit input widened"));
    let l = d.link.as_mut().unwrap();
    l.mode = CodecMode::Ssc;
    l.stream_bits = 16;
    d.endpoint = Some(PcmFormat {
        sample_rate: 48_000,
        bits: 24,
        channels: 2,
    });
    assert_eq!(sent(&d).as_deref(), Some("24-bit input dithered down"));
    d.endpoint = None;
    assert_eq!(sent(&d), None);
    assert_eq!(
        value(&card(&d, 0), "Windows format").value,
        "nothing played yet"
    );
}

#[test]
fn volume_rows() {
    let row = |mode, level, muted| {
        let r = volume_row(&VolumeStatus {
            mode,
            level_percent: level,
            muted,
        });
        (r.value, r.tone)
    };
    assert_eq!(
        row(VolumeMode::Buds, Some(62), false),
        ("62 % · set on the buds".to_string(), Tone::Normal)
    );
    assert_eq!(
        row(VolumeMode::Software, Some(40), true),
        (
            "40 % · applied by the service · muted".to_string(),
            Tone::Warn
        )
    );
    assert_eq!(
        row(VolumeMode::Windows, None, false),
        ("applied by Windows".to_string(), Tone::Normal)
    );
    assert_eq!(
        row(VolumeMode::Unknown, Some(0), false),
        ("0 % · unknown".to_string(), Tone::Normal)
    );
}

#[test]
fn a_device_without_a_link_shows_why() {
    let c = card(&device("reconnect"), 1_800_007_261);
    assert_eq!(
        (c.badge.as_str(), c.badge_tone),
        ("Reconnecting", Tone::Warn)
    );
    // The reason is on the card once: under "State". Not as the line
    // under the title, and not again as the warning that caused it.
    let reason = "link down: media write failed; next attempt in 2 s";
    assert_eq!(c.summary, "Setting the stream up again");
    let state = value(&c, "State");
    assert_eq!(state.value, "Reconnecting");
    assert_eq!(state.tone, Tone::Warn);
    assert_eq!(state.detail.as_deref(), Some(reason));
    assert_eq!(c.note, None);
    let everything = format!("{c:?}");
    assert_eq!(everything.matches("media write failed").count(), 1);
    assert!(!labels(&c).contains(&"Packet"));
    assert_eq!(value(&c, "Write errors").tone, Tone::Warn);
    assert_eq!(value(&c, "Reconnects").tone, Tone::Warn);
    // The stream is gone: no latency of it.
    assert!(!labels(&c).contains(&"Write latency"));
    // A warning about something else is shown.
    let mut d = device("reconnect");
    d.last_event.as_mut().unwrap().text = "the buds refused the configuration".into();
    assert_eq!(
        card(&d, 1_800_007_261).note,
        Some((
            Tone::Warn,
            "just now: the buds refused the configuration".to_string()
        ))
    );
    // And so is the last warning once the reason is gone.
    let mut d = device("reconnect");
    d.phase_reason.clear();
    assert_eq!(
        card(&d, 1_800_007_261).note,
        Some((
            Tone::Warn,
            "just now: link down: media write failed".to_string()
        ))
    );
    assert!(!labels(&card(&d, 0)).contains(&"State"));

    // Buds that never connected: what is waited for, and no block of
    // counters that are all zero.
    let c = card(&device("waiting"), 0);
    assert_eq!(
        (c.badge.as_str(), c.badge_tone),
        ("Buds not connected", Tone::Muted)
    );
    assert_eq!(c.summary, "Waiting for the buds to connect to this PC");
    assert_eq!(
        c.sections.iter().map(|s| s.title).collect::<Vec<_>>(),
        ["Audio", "Link"]
    );
    assert_eq!(labels(&c), ["Windows format", "Volume", "Driver"]);
    assert!(!format!("{c:?}").contains("0.00 ms"));
    // One reconnect counted: the counters are there, the latency is not.
    let mut d = device("waiting");
    d.counters.reconnects = 1;
    let c = card(&d, 0);
    assert_eq!(c.sections.last().unwrap().title, "Activity");
    assert!(!labels(&c).contains(&"Write latency"));
    // A stream that is open and idle has counters but no latency either.
    let Feed::Live(two) = live("two") else {
        panic!()
    };
    let idle = card(&two.devices[1], 0);
    assert_eq!(idle.badge, "Ready");
    assert!(labels(&idle).contains(&"Packets sent"));
    assert!(!labels(&idle).contains(&"Write latency"));
    assert!(!labels(&idle).contains(&"Streaming for"));
    for phase in [
        LinkPhase::WaitingDriver,
        LinkPhase::WaitingBuds,
        LinkPhase::Reconnecting,
        LinkPhase::Stopped,
    ] {
        let mut d = device("waiting");
        d.phase = phase;
        let c = card(&d, 0);
        assert!(!c.summary.is_empty(), "{phase:?}");
        assert_ne!(c.summary, c.badge, "{phase:?}: said twice");
    }

    // The barest device a service could send.
    let bare = DeviceStatus {
        id: "X".into(),
        ..DeviceStatus::default()
    };
    let c = card(&bare, 0);
    assert_eq!(c.title, "X");
    assert_eq!(c.badge, "State unknown");
    assert_eq!(c.summary, "");
    assert_eq!(value(&c, "Driver").value, "no answer");
    assert_eq!(labels(&c), ["Windows format", "Driver"]);
}

#[test]
fn events_are_dated_against_the_senders_clock() {
    let mut d = device("uhq");
    d.last_event = Some(Event {
        level: EventLevel::Error,
        time_unix: 1_000,
        text: "the driver detached the session".into(),
    });
    let note = |d: &DeviceStatus, now| card(d, now).note.unwrap();
    assert_eq!(
        note(&d, 1_002),
        (
            Tone::Bad,
            "just now: the driver detached the session".to_string()
        )
    );
    assert_eq!(
        note(&d, 1_190).1,
        "3 min 10 s ago: the driver detached the session"
    );
    // A clock that went back, or a time that was never set.
    assert_eq!(note(&d, 900).1, "earlier: the driver detached the session");
    d.last_event.as_mut().unwrap().time_unix = 0;
    assert_eq!(
        note(&d, 1_190).1,
        "earlier: the driver detached the session"
    );
}

#[test]
fn headlines_for_everything_that_is_not_a_card() {
    assert_eq!(headline(&live("uhq")), None);
    for (feed, title) in [
        (Feed::Connecting, "Connecting"),
        (Feed::Down(Outage::NotRunning), "Service not running"),
        (Feed::Down(Outage::Busy), "Service busy"),
        (Feed::Down(Outage::Denied), "Access denied"),
        (
            Feed::Down(Outage::Version { theirs: 7 }),
            "Version mismatch",
        ),
        (Feed::Down(Outage::Unreadable), "Status unreadable"),
        (Feed::Down(Outage::Untrusted), "Status not trusted"),
        (Feed::Down(Outage::Lost), "Reconnecting"),
        (live("nobuds"), "No buds"),
    ] {
        let (_, t, text) = headline(&feed).unwrap();
        assert_eq!(t, title);
        assert!(text.len() > 20 && text.ends_with('.'), "{text}");
        // Nothing a user would have to look up or type into a console.
        for jargon in ["sc start", "pipe", "protocol", "AVRCP", "IOCTL"] {
            assert!(!text.contains(jargon), "{jargon} in: {text}");
        }
        assert!(cards(&feed).is_empty());
    }
    let (_, _, text) = headline(&Feed::Down(Outage::Version { theirs: 7 })).unwrap();
    assert!(
        text.contains("version 7") && text.contains("version 1"),
        "{text}"
    );
}

#[test]
fn the_footer_names_the_service() {
    assert_eq!(
        footer(&live("uhq")),
        format!(
            "crossc service {} · running for 2 h 01 min",
            env!("CARGO_PKG_VERSION")
        )
    );
    let bare = Feed::Live(Box::new(Snapshot {
        version: 1,
        service: ServiceInfo::default(),
        ..Snapshot::default()
    }));
    assert_eq!(footer(&bare), "crossc service (version unknown)");
    assert!(footer(&Feed::Connecting).starts_with("crossc tray "));
}

#[test]
fn the_text_report_has_everything() {
    let text = report(&live("two"));
    for needle in [
        "crossc status",
        "crossc service",
        r"log: C:\ProgramData\crossc",
        "Galaxy Buds4 Pro [AABBCCDDEEFF] — Streaming",
        "Galaxy Buds3 Pro [AABBCCDDEE00] — Ready",
        "· running for 2 h 01 min · status version 1",
        "  UHQ (Ultra High Quality) · 24-bit",
        "  Seamless HiFi · 24-bit",
        "  Media MTU         1005 bytes (the largest packet this link takes)",
        "  Codec options     0x0a of 0x3e (in use, of what the buds offer)",
        "  SSC octets        configuration 0x0a, capabilities 0x3e",
        "  Driver            0.2.0 (interface version 3)",
        "  Windows format    96 kHz · 24-bit · 2 ch",
        "  Sent to the buds  48 kHz · 24-bit (16-bit input widened)",
        "  Bitrate           584 kbps (adaptive · ladder 584 / 291 / 152)",
        "  Bitrate ladder    584 / 291 / 152 kbps",
        "  MTU rule          min of plausible reported MTUs",
        "  Volume            40 % · applied by the service · muted",
        "  Reconnects        1",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
    assert!(!text.contains('\r'));

    let text = report(&Feed::Down(Outage::NotRunning));
    assert!(
        text.starts_with("crossc status\nService not running: "),
        "{text}"
    );
    let text = report(&live("adaptive"));
    assert!(text.contains("  Last warning      "), "{text}");
    assert!(text.contains("step 2 of 3"), "{text}");
    // The report keeps the warning the card leaves out, for its time.
    let text = report(&live("reconnect"));
    assert!(
        text.contains("  Last warning      just now: link down: media write failed"),
        "{text}"
    );
    assert!(text.contains("  Setting the stream up again"), "{text}");
}

#[test]
fn the_flyout_is_one_column_per_device_up_to_two() {
    let (w0, h0) = flyout_size(&Feed::Down(Outage::NotRunning));
    assert_eq!(w0, COLUMN_WIDTH + 2 * PADDING);
    assert_eq!(flyout_size(&live("nobuds")), (w0, h0));
    let (w1, h1) = flyout_size(&live("uhq"));
    assert_eq!(w1, w0);
    assert!(h1 > h0 && h1 < 820, "{h1}");
    // Buds that never streamed: a short card, not half of it zeros.
    let (_, hw) = flyout_size(&live("waiting"));
    assert!(hw < h1 / 2, "{hw} vs {h1}");
    let (w2, h2) = flyout_size(&live("two"));
    assert_eq!(w2, 2 * COLUMN_WIDTH + GAP + 2 * PADDING);
    assert!(h2 < h1 + 60, "side by side, not stacked: {h2} vs {h1}");
    // A third device starts a second row.
    let Feed::Live(mut s) = live("two") else {
        panic!()
    };
    s.devices.push(DeviceStatus {
        id: "third".into(),
        driver: Some(DriverInfo::default()),
        counters: Counters::default(),
        ..DeviceStatus::default()
    });
    let (w3, h3) = flyout_size(&Feed::Live(s));
    assert_eq!(w3, w2);
    assert!(h3 > h2);
    // A warning adds its line.
    assert!(flyout_size(&live("adaptive")).1 > h1);
}

fn r(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
    Rect {
        left,
        top,
        right,
        bottom,
    }
}

#[test]
fn the_flyout_sits_against_the_taskbar_next_to_the_icon() {
    let size = (400, 500);
    // Taskbar at the bottom of a 1920x1080 screen, icon near the right.
    let work = r(0, 0, 1920, 1032);
    let p = place(r(1700, 1040, 1724, 1072), work, size, 12);
    assert_eq!(
        p,
        r(1508, 520, 1908, 1020),
        "right edge clamped, above the taskbar"
    );
    // Icon further left: centred on it.
    let p = place(r(1000, 1040, 1024, 1072), work, size, 12);
    assert_eq!((p.left, p.bottom), (812, 1020));
    // Taskbar on top.
    let work = r(0, 48, 1920, 1080);
    let p = place(r(1700, 8, 1724, 40), work, size, 12);
    assert_eq!((p.right, p.top), (1908, 60));
    // Taskbar on the left and on the right.
    let work = r(60, 0, 1920, 1080);
    let p = place(r(10, 1000, 50, 1030), work, size, 12);
    assert_eq!((p.left, p.bottom), (72, 1068));
    let work = r(0, 0, 1860, 1080);
    let p = place(r(1870, 500, 1910, 530), work, size, 12);
    assert_eq!((p.right, p.top), (1848, 265));
    // A second monitor to the left of the primary one (negative x).
    let work = r(-2560, 0, 0, 1392);
    let p = place(r(-200, 1400, -176, 1432), work, size, 12);
    assert_eq!(p, r(-412, 880, -12, 1380));
    // The icon inside the work area (overflow flyout, hidden taskbar).
    let work = r(0, 0, 1920, 1080);
    let p = place(r(1700, 900, 1724, 924), work, size, 12);
    assert_eq!((p.right, p.bottom), (1908, 888), "above the icon");
    let p = place(r(1700, 100, 1724, 124), work, size, 12);
    assert_eq!(p.top, 136, "no room above: below it");
}

#[test]
fn a_flyout_larger_than_the_work_area_is_cut_to_it() {
    let work = r(0, 0, 800, 600);
    let p = place(r(700, 610, 724, 640), work, (2000, 3000), 12);
    assert_eq!(p, r(12, 12, 788, 588));
    // Nonsense in, something on the screen out.
    let p = place(r(0, 0, 0, 0), r(0, 0, 10, 10), (500, 500), 12);
    assert!(p.width() >= 1 && p.height() >= 1);
    let p = place(r(5, 5, 5, 5), r(100, 100, 100, 100), (0, 0), 0);
    assert!(p.width() >= 1 && p.height() >= 1);
}

#[test]
fn a_detail_gets_the_lines_its_words_need() {
    let lines = |text: &str| {
        detail_lines(&Row {
            label: "x",
            value: String::new(),
            tone: Tone::Normal,
            detail: (!text.is_empty()).then(|| text.to_string()),
        })
    };
    assert_eq!(lines(""), 0);
    assert_eq!(lines("reported by the buds"), 1);
    assert_eq!(lines("adaptive · ladder 584 / 291 / 152"), 1);
    assert_eq!(
        lines("link down: media write failed; next attempt in 2 s"),
        2
    );
    assert_eq!(lines(&"word ".repeat(60)), 3, "never more than three");
    assert_eq!(lines(&"x".repeat(100)), 3, "a word without a break");
    assert_eq!(lines(&"x".repeat(41)), 2);
    // Every detail the demo's cards have fits its lines.
    for name in ["waiting", "hifi", "uhq", "adaptive", "reconnect", "two"] {
        for c in cards(&demo::state(name, 0).unwrap()) {
            for r in c.sections.iter().flat_map(|s| &s.rows) {
                let chars = r.detail.as_ref().map_or(0, |d| d.chars().count());
                assert!(
                    chars <= 40 * detail_lines(r) as usize,
                    "{name}: {:?}",
                    r.detail
                );
            }
        }
    }
}

#[test]
fn unconnected_buds_are_hidden_next_to_connected_ones() {
    let Feed::Live(mut s) = live("two") else {
        panic!()
    };
    s.devices[1].phase = LinkPhase::WaitingBuds;
    let feed = Feed::Live(s.clone());
    assert_eq!(cards(&feed).len(), 1);
    assert!(!tooltip(&feed).contains("Buds3 Pro"));
    assert!(headline(&feed).is_none());
    // "Copy details" still lists both.
    assert!(report(&feed).contains("Buds3 Pro"));
}
