//! `avrcp_vol`: the taper tables, the echo rule, connect sync, mute, the
//! subscriber state, and a closed-loop simulation of Windows + BthAvrcp +
//! buds that must always settle.

use std::collections::VecDeque;
use std::format;
use std::vec::Vec;

use crossc_ioctl::audio::{
    AUDIO_FLAG_AVRCP, AUDIO_FLAG_AVRCP_SUBSCRIBED, AUDIO_FLAG_HW_VOLUME, AUDIO_FLAG_MUTED, VolumeState,
};

use crate::avrcp_vol::{
    AVRCP_BOUNDARY, BadChannel, CHANNEL_MASTER, Effects, INITIAL_LEVEL, LEVEL_MAX, LEVEL_MIN, LEVEL_OF_AVRCP,
    LEVEL_STEP, STALE_MS, Volume, avrcp_to_level, clamp_level, level_to_avrcp,
};

/// `lo` of the taper: 10^(-63.5/35) (= (10^(-63.5/20))^(1/1.75)).
fn lo() -> f64 {
    10f64.powf(-63.5 / 35.0)
}

fn taper_db(wiper: f64) -> f64 {
    35.0 * (lo() + wiper * (1.0 - lo())).log10()
}

fn db(level: i32) -> f64 {
    f64::from(level) / 65536.0
}

fn lvl(db: f64) -> i32 {
    (db * 65536.0).round() as i32
}

/// BthAvrcp's `DecibelsToAvrcpVolume` in floating point: wiper * 127,
/// rounded half away from zero, masked.
fn bthavrcp_to_avrcp(level: i32) -> u8 {
    let w = ((10f64.powf(db(level) / 35.0)) - lo()) / (1.0 - lo());
    ((w * 127.0).round() as i64 & 0x7F) as u8
}

const NONE: Effects = Effects { notify_volume: false, notify_mute: false, avrcp_event: false };
const EVENT: Effects = Effects { notify_volume: false, notify_mute: false, avrcp_event: true };
const SLIDER: Effects = Effects { notify_volume: true, notify_mute: false, avrcp_event: false };

/// A device with the element and a subscriber that has done its connect SET.
fn connected(buds: u8) -> Volume {
    let mut v = Volume::new();
    v.set_element(true);
    v.subscribe(true);
    assert_eq!(v.remote_set(avrcp_to_level(buds), 0), SLIDER);
    v
}

// ---------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------

#[test]
fn tables_are_the_inbox_taper() {
    assert!((lo() - 10f64.powf(-63.5 / 20.0).powf(1.0 / 1.75)).abs() < 1e-12);
    assert_eq!(LEVEL_MIN, -4_161_536);
    assert_eq!(LEVEL_MIN as u32, 0xFFC0_8000);
    assert_eq!((LEVEL_MAX, LEVEL_STEP), (0, 0x8000));
    for v in 0..128usize {
        let exact = taper_db(v as f64 / 127.0) * 65536.0;
        let t = LEVEL_OF_AVRCP[v];
        assert!((f64::from(t) - exact).abs() <= 1.0, "v={v}: {t} vs {exact}");
        if v > 0 {
            assert!(t > LEVEL_OF_AVRCP[v - 1], "monotonic at {v}");
        }
    }
    assert_eq!((LEVEL_OF_AVRCP[0], LEVEL_OF_AVRCP[127]), (LEVEL_MIN, LEVEL_MAX));
    for v in 0..127usize {
        let exact = taper_db((v as f64 + 0.5) / 127.0) * 65536.0;
        let b = AVRCP_BOUNDARY[v];
        assert!(f64::from(b) >= exact && f64::from(b) - exact < 1.0, "boundary {v}");
        assert!(LEVEL_OF_AVRCP[v] < b && b < LEVEL_OF_AVRCP[v + 1]);
    }
    // The research table (own-avrcp-design.md 5.4): slider -> AVRCP -> dB.
    for (avrcp, want) in
        [(0u8, -63.50), (13, -32.73), (38, -17.80), (64, -10.19), (89, -5.31), (114, -1.61), (127, 0.0)]
    {
        assert!((db(avrcp_to_level(avrcp)) - want).abs() < 0.006, "{avrcp}");
    }
}

#[test]
fn conversions_round_trip_and_match_bthavrcp() {
    for v in 0..=127u8 {
        assert_eq!(level_to_avrcp(avrcp_to_level(v)), v);
        assert_eq!(bthavrcp_to_avrcp(avrcp_to_level(v)), v);
        assert_eq!(avrcp_to_level(v | 0x80), avrcp_to_level(v), "bit 7 is ignored");
    }
    for (v, &b) in AVRCP_BOUNDARY.iter().enumerate() {
        assert_eq!(level_to_avrcp(b) as usize, v + 1);
        assert_eq!(level_to_avrcp(b - 1) as usize, v);
    }
    // Every level in range converts like BthAvrcp's floating point does
    // (sampled; the float version is off only within 1/65536 dB of a
    // boundary, and none of the samples sits there).
    let mut l = LEVEL_MIN;
    while l <= LEVEL_MAX {
        let near = AVRCP_BOUNDARY.iter().any(|&b| (b - l).abs() <= 1);
        if !near {
            assert_eq!(level_to_avrcp(l), bthavrcp_to_avrcp(l), "level {l}");
        }
        l += 977;
    }
    // Out of range clamps instead of wrapping like BthAvrcp's `& 0x7F`.
    assert_eq!(level_to_avrcp(1), 127);
    assert_eq!(level_to_avrcp(i32::MAX), 127);
    assert_eq!(level_to_avrcp(LEVEL_MIN - 1), 0);
    assert_eq!(level_to_avrcp(i32::MIN), 0);
    assert_eq!(bthavrcp_to_avrcp(65536), 8, "an unclamped +1 dB wraps to 8 in BthAvrcp");
    assert_eq!(bthavrcp_to_avrcp(LEVEL_MIN - (20 << 16)), 127, "and 20 dB below the minimum to full volume");
    assert_eq!((clamp_level(5), clamp_level(i32::MIN)), (0, LEVEL_MIN));
}

#[test]
fn slider_position_reaches_the_buds_as_127p() {
    // audiosrv maps slider p onto the hardware range with the same taper
    // (exponent 1.75 over -63.5..0 dB); a level written exactly then
    // arrives as round(127 p).
    for pct in 0..=100u32 {
        let p = f64::from(pct) / 100.0;
        let level = lvl(taper_db(p));
        let want = (127.0 * p).round() as u8;
        assert_eq!(level_to_avrcp(level), want, "{pct} %");
    }
}

#[test]
fn slider_position_after_half_db_quantisation_is_within_two_steps() {
    // If audiosrv writes on the element's 0.5 dB grid (nearest), about half
    // the positions land next to round(127 p), never more than two steps
    // away. Cosmetic: the closed-loop tests show no oscillation from it.
    let mut off = 0;
    let mut worst = 0;
    for pct in 0..=100u32 {
        let p = f64::from(pct) / 100.0;
        let l = lvl(taper_db(p));
        let q = (l + LEVEL_STEP / 2).div_euclid(LEVEL_STEP) * LEVEL_STEP;
        let d = (i32::from(level_to_avrcp(q)) - (127.0 * p).round() as i32).abs();
        off += i32::from(d != 0);
        worst = worst.max(d);
    }
    assert!(worst <= 2, "worst {worst}");
    assert!((40..=60).contains(&off), "{off} of 101 positions off");
    // The reviewer's examples: 30 % -> -18.0 dB -> 37, 50 % -> -10.5 -> 63,
    // 80 % -> -3.5 -> 100; and -10.0 dB is AVRCP 65, not 64.
    assert_eq!([lvl(-18.0), lvl(-10.5), lvl(-3.5), lvl(-10.0), lvl(-17.5)].map(level_to_avrcp), [37, 63, 100, 65, 39]);
}

// ---------------------------------------------------------------------------
// Windows side
// ---------------------------------------------------------------------------

#[test]
fn channels_master_and_balance() {
    let mut v = connected(64);
    // Only the quieter channel moves: the louder one still is the buds' level.
    assert_eq!(v.windows_level(0, lvl(-20.0), 0), Ok(NONE));
    assert_eq!(v.level(), avrcp_to_level(64), "the louder channel");
    assert_eq!(v.windows_level(1, lvl(-20.0), 0), Ok(EVENT));
    assert_eq!(v.level(), lvl(-20.0));
    assert_eq!(v.windows_level(1, lvl(-6.0), 0), Ok(EVENT));
    assert_eq!((v.retrieve_level(0), v.retrieve_level(1)), (Ok(lvl(-20.0)), Ok(lvl(-6.0))));
    assert_eq!(v.retrieve_level(CHANNEL_MASTER), Ok(lvl(-6.0)));
    assert_eq!(v.level(), lvl(-6.0));
    assert_eq!(v.windows_level(CHANNEL_MASTER, lvl(-3.0), 0), Ok(EVENT));
    assert_eq!((v.retrieve_level(0), v.retrieve_level(1)), (Ok(lvl(-3.0)), Ok(lvl(-3.0))));
    // The same level again: nothing to tell AVRCP.
    assert_eq!(v.windows_level(0, lvl(-3.0), 0), Ok(NONE));
    assert_eq!(v.windows_level(2, 0, 0), Err(BadChannel));
    assert_eq!(v.retrieve_level(7), Err(BadChannel));
    assert_eq!(v.windows_mute(2, true, 0), Err(BadChannel));
    // Out of range is clamped.
    assert_eq!(v.windows_level(CHANNEL_MASTER, 100 << 16, 0), Ok(EVENT));
    assert_eq!(v.level(), LEVEL_MAX);
    v.windows_level(CHANNEL_MASTER, i32::MIN, 0).unwrap();
    assert_eq!(v.level(), LEVEL_MIN);
    assert_eq!(v.remote_get(), LEVEL_MIN);
}

#[test]
fn no_events_without_a_subscriber() {
    let mut v = Volume::new();
    assert_eq!(v.level(), INITIAL_LEVEL);
    assert_eq!(level_to_avrcp(INITIAL_LEVEL), 64);
    v.set_element(true);
    assert_eq!(v.windows_level(CHANNEL_MASTER, lvl(-12.0), 0), Ok(NONE));
    assert_eq!(v.windows_mute(CHANNEL_MASTER, true, 0), Ok(NONE));
    assert!(!v.active());
    assert_eq!(v.wire().0, AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_MUTED);
}

// ---------------------------------------------------------------------------
// Echo rule and confirmations
// ---------------------------------------------------------------------------

#[test]
fn windows_write_back_of_the_buds_level_is_not_sent_back() {
    // Buds at 126 = -0.118 dB. audiosrv writes back 0.0 or -0.5 dB (its
    // grid); both would convert to another AVRCP value (127 / 122).
    for back in [0, -LEVEL_STEP] {
        let mut v = connected(100);
        assert_eq!(v.remote_set(avrcp_to_level(126), 0), SLIDER);
        assert_ne!(level_to_avrcp(back), 126);
        assert_eq!(v.windows_level(0, back, 0), Ok(NONE));
        assert_eq!(v.windows_level(1, back, 0), Ok(NONE));
        assert_eq!(v.retrieve_level(0), Ok(back), "Windows reads what it wrote");
        assert_eq!(level_to_avrcp(v.remote_get()), 126, "AVRCP still reads the buds' level");
        // A real slider move afterwards goes out.
        assert_eq!(v.windows_level(CHANNEL_MASTER, lvl(-1.5), 0), Ok(EVENT));
        assert_eq!(level_to_avrcp(v.remote_get()), level_to_avrcp(lvl(-1.5)));
        // And the rule is over: a move back near the old level is real too.
        assert_eq!(v.windows_level(CHANNEL_MASTER, back, 0), Ok(EVENT));
    }
}

#[test]
fn echo_window_ends_after_echo_ms() {
    use crate::avrcp_vol::ECHO_MS;
    // Buds at 126 (-0.12 dB); audiosrv's write-back of 0 dB is swallowed.
    let mut v = connected(64);
    let t = 5000;
    assert_eq!(v.remote_set(avrcp_to_level(126), t), SLIDER);
    assert_eq!(v.windows_level(CHANNEL_MASTER, 0, t + 10), Ok(NONE));
    assert_eq!(level_to_avrcp(v.remote_get()), 126);
    // Later, 0 dB from Windows is a real move: the buds reach 127.
    assert_eq!(v.windows_level(CHANNEL_MASTER, 0, t + ECHO_MS), Ok(EVENT));
    assert_eq!(level_to_avrcp(v.remote_get()), 127);
    // A small move (a volume key near the top, under 0.5 dB) after the
    // window is not swallowed either.
    let r = avrcp_to_level(110);
    assert_eq!(v.remote_set(r, 20_000), SLIDER);
    assert_eq!(v.windows_level(CHANNEL_MASTER, r + 0x3000, 20_000 + ECHO_MS - 1), Ok(NONE));
    assert_eq!(v.windows_level(CHANNEL_MASTER, r + 0x3001, 20_000 + ECHO_MS), Ok(EVENT));
    assert_eq!(v.remote_get(), r + 0x3001);
}

#[test]
fn echo_window_is_less_than_one_step() {
    let mut v = connected(64);
    let r = avrcp_to_level(80);
    v.remote_set(r, 0);
    assert_eq!(v.windows_level(CHANNEL_MASTER, r + LEVEL_STEP - 1, 0), Ok(NONE));
    assert_eq!(v.windows_level(CHANNEL_MASTER, r - LEVEL_STEP + 1, 0), Ok(NONE));
    assert_eq!(v.windows_level(CHANNEL_MASTER, r + LEVEL_STEP, 0), Ok(EVENT));
}

#[test]
fn buds_confirming_a_windows_level_does_not_move_the_slider() {
    let mut v = connected(64);
    let w = lvl(-7.3);
    assert_eq!(v.windows_level(CHANNEL_MASTER, w, 0), Ok(EVENT));
    let sent = level_to_avrcp(v.remote_get());
    // Buds that report CHANGED for a SetAbsoluteVolume (AVRCP says they
    // should not, some do): BthAvrcp writes the coarser level back.
    assert_eq!(v.remote_set(avrcp_to_level(sent), 0), NONE);
    assert_eq!((v.level(), v.retrieve_level(0)), (w, Ok(w)));
    // A different value is a gesture.
    assert_eq!(v.remote_set(avrcp_to_level(sent + 3), 0), SLIDER);
    assert_eq!(v.retrieve_level(1), Ok(avrcp_to_level(sent + 3)));
}

// ---------------------------------------------------------------------------
// Connect, subscriber, staleness
// ---------------------------------------------------------------------------

#[test]
fn the_buds_level_wins_at_connect() {
    let mut v = Volume::new();
    v.set_element(true);
    v.windows_level(CHANNEL_MASTER, lvl(-20.0), 0).unwrap();
    v.subscribe(true);
    assert!(v.active());
    // INTERIM with the buds' level, even one in the same AVRCP step.
    let same = avrcp_to_level(level_to_avrcp(lvl(-20.0)));
    assert_eq!(v.remote_set(same, 0), SLIDER);
    assert_eq!(v.retrieve_level(0), Ok(same));
    // Reconnect (handle closed, reopened) does the same again.
    v.subscribe(false);
    assert!(!v.active());
    v.subscribe(true);
    assert_eq!(v.remote_set(avrcp_to_level(30), 0), SLIDER);
    assert_eq!(v.retrieve_level(CHANNEL_MASTER), Ok(avrcp_to_level(30)));
}

#[test]
fn bthavrcp_default_then_interim_is_the_connect_level_even_while_muted() {
    use crate::avrcp_vol::{BTHAVRCP_DEFAULT_AVRCP, CONNECT_MS};
    // Response processing enabled before the INTERIM: BthAvrcp first SETs
    // its constructor default (64), then the buds' level R.
    let mut v = Volume::new();
    v.set_element(true);
    v.windows_level(CHANNEL_MASTER, lvl(-25.0), 0).unwrap();
    v.windows_mute(CHANNEL_MASTER, true, 0).unwrap();
    v.subscribe(true);
    let t = 7000;
    let connect = Effects { notify_volume: true, notify_mute: false, avrcp_event: true };
    assert_eq!(v.remote_set(avrcp_to_level(BTHAVRCP_DEFAULT_AVRCP), t), connect);
    assert_eq!(v.remote_get(), LEVEL_MIN, "the mute is re-sent");
    // Muting during the window does not close it (already muted here; a
    // mute change only generates the event).
    assert_eq!(v.windows_mute(CHANNEL_MASTER, true, t + 1), Ok(NONE));
    // The buds following the mute: ignored. A repeat of 64: ignored, but
    // BthAvrcp now caches 64, so the mute is re-sent.
    assert_eq!(v.remote_set(avrcp_to_level(0), t + 10), NONE);
    assert_eq!(v.remote_set(avrcp_to_level(64), t + 20), EVENT);
    assert_eq!(v.remote_get(), LEVEL_MIN);
    // The INTERIM: the connect level, the mute stays.
    assert_eq!(v.remote_set(avrcp_to_level(7), t + CONNECT_MS - 1), connect);
    assert!(v.muted());
    assert_eq!(v.level(), avrcp_to_level(7));
    assert_eq!(v.remote_get(), LEVEL_MIN);
    // The window is closed now: a later gesture unmutes as usual.
    let fx = v.remote_set(avrcp_to_level(20), t + CONNECT_MS - 1);
    assert_eq!(fx, Effects { notify_volume: true, notify_mute: true, avrcp_event: false });
    assert!(!v.muted());
}

#[test]
fn connect_window_rules() {
    use crate::avrcp_vol::CONNECT_MS;
    let gesture_unmutes = Effects { notify_volume: true, notify_mute: true, avrcp_event: false };
    let muted_at = |first: u8| {
        let mut v = Volume::new();
        v.set_element(true);
        v.windows_mute(CHANNEL_MASTER, true, 0).unwrap();
        v.subscribe(true);
        v.remote_set(avrcp_to_level(first), 1000);
        v
    };
    // A first SET other than 64 is the buds' level already: no window.
    let mut v = muted_at(65);
    assert_eq!(v.remote_set(avrcp_to_level(30), 1001), gesture_unmutes);
    // The window ends after CONNECT_MS.
    let mut v = muted_at(64);
    assert_eq!(v.remote_set(avrcp_to_level(30), 1000 + CONNECT_MS), gesture_unmutes);
    // An ignored SET past the end does not reopen it.
    let mut v = muted_at(64);
    assert_eq!(v.remote_set(avrcp_to_level(0), 1000 + CONNECT_MS), NONE);
    assert_eq!(v.remote_set(avrcp_to_level(30), 1001 + CONNECT_MS), gesture_unmutes);
    // A real slider move (unmuted, sent to AVRCP) closes it early.
    let mut v = Volume::new();
    v.set_element(true);
    v.subscribe(true);
    v.remote_set(avrcp_to_level(64), 1000);
    assert_eq!(v.windows_level(CHANNEL_MASTER, lvl(-30.0), 1100), Ok(EVENT));
    let sent = level_to_avrcp(v.remote_get());
    // Buds confirming that level: not a connect level (no slider snap).
    assert_eq!(v.remote_set(avrcp_to_level(sent), 1200), NONE);
    assert_eq!(v.retrieve_level(0), Ok(lvl(-30.0)));
    // AVRCP 0 while muted before AVRCP has read the minimum is the buds'
    // level (the INTERIM), not the buds following the mute.
    let mut v = muted_at(64);
    assert_eq!(
        v.remote_set(avrcp_to_level(0), 1100),
        Effects { notify_volume: true, notify_mute: false, avrcp_event: true }
    );
    assert_eq!((v.level(), v.muted()), (LEVEL_MIN, true));
    // A resubscribe starts over.
    let mut v = muted_at(64);
    v.subscribe(false);
    v.subscribe(true);
    assert!(v.remote_set(avrcp_to_level(40), 1100).avrcp_event);
    assert_eq!(v.remote_set(avrcp_to_level(41), 1200), gesture_unmutes, "40 opened no window");
}

#[test]
fn a_silent_subscriber_turns_stale_and_recovers() {
    let mut v = connected(64);
    assert!(v.active());
    assert_eq!(v.windows_level(CHANNEL_MASTER, lvl(-12.0), 1000), Ok(EVENT));
    // Within STALE_MS: a fast drag whose events BthAvrcp coalesces.
    assert_eq!(v.windows_level(CHANNEL_MASTER, lvl(-13.0), 1000 + STALE_MS - 1), Ok(EVENT));
    assert!(v.active());
    // The first event is still unanswered at the next change: stale.
    assert_eq!(v.windows_level(CHANNEL_MASTER, lvl(-14.0), 1000 + STALE_MS), Ok(EVENT));
    assert!(!v.active() && v.subscribed());
    assert_eq!(v.wire().0 & (AUDIO_FLAG_AVRCP | AUDIO_FLAG_AVRCP_SUBSCRIBED), AUDIO_FLAG_AVRCP_SUBSCRIBED);
    // It reads: active again.
    v.remote_get();
    assert!(v.active());
    // Answered events never go stale, however far apart.
    for i in 0..5u64 {
        assert_eq!(v.windows_level(CHANNEL_MASTER, lvl(-20.0 - i as f64), 10_000 * i), Ok(EVENT));
        v.remote_get();
    }
    assert!(v.active());
    // A SET also counts as an answer.
    v.windows_level(CHANNEL_MASTER, lvl(-3.0), 100_000).unwrap();
    v.windows_level(CHANNEL_MASTER, lvl(-4.0), 100_000 + STALE_MS).unwrap();
    assert!(!v.active());
    v.remote_set(avrcp_to_level(10), 0);
    assert!(v.active());
}

#[test]
fn removing_the_element_drops_the_subscriber_but_keeps_the_level() {
    let mut v = connected(90);
    v.windows_mute(CHANNEL_MASTER, true, 0).unwrap();
    v.set_element(false);
    assert!(!v.active() && !v.subscribed());
    assert_eq!(v.wire(), (0, VolumeState::default()));
    v.set_element(true);
    assert!(v.muted());
    assert_eq!(v.level(), avrcp_to_level(90));
    assert!(!v.active(), "a rebuilt circuit waits for a new subscriber");
}

// ---------------------------------------------------------------------------
// Mute
// ---------------------------------------------------------------------------

#[test]
fn mute_sends_the_minimum_and_restores() {
    let mut v = connected(70);
    assert_eq!(v.windows_mute(0, true, 0), Ok(EVENT));
    assert_eq!(v.windows_mute(1, true, 0), Ok(NONE), "already muted");
    assert!(v.muted());
    assert_eq!(v.remote_get(), LEVEL_MIN);
    // Slider moves while muted: stored, nothing sent.
    assert_eq!(v.windows_level(CHANNEL_MASTER, lvl(-15.0), 0), Ok(NONE));
    assert_eq!(v.level(), lvl(-15.0));
    // The buds follow the mute (CHANGED 0): ignored, still muted.
    assert_eq!(v.remote_set(avrcp_to_level(0), 0), NONE);
    assert!(v.muted());
    assert_eq!(v.retrieve_mute(CHANNEL_MASTER), Ok(true));
    // Unmute: AVRCP reads the stored level.
    assert_eq!(v.windows_mute(CHANNEL_MASTER, false, 0), Ok(EVENT));
    assert_eq!(v.remote_get(), lvl(-15.0));
    let (flags, st) = v.wire();
    assert_eq!(flags & AUDIO_FLAG_MUTED, 0);
    assert_eq!(st.avrcp, level_to_avrcp(lvl(-15.0)));
}

#[test]
fn a_volume_gesture_on_the_buds_unmutes() {
    let mut v = connected(70);
    v.windows_mute(CHANNEL_MASTER, true, 0).unwrap();
    let fx = v.remote_set(avrcp_to_level(40), 0);
    assert_eq!(fx, Effects { notify_volume: true, notify_mute: true, avrcp_event: false });
    assert!(!v.muted());
    assert_eq!(v.retrieve_level(0), Ok(avrcp_to_level(40)));
}

#[test]
fn connect_while_muted_keeps_the_mute() {
    let mut v = Volume::new();
    v.set_element(true);
    v.windows_mute(CHANNEL_MASTER, true, 0).unwrap();
    v.subscribe(true);
    let fx = v.remote_set(avrcp_to_level(100), 0);
    assert_eq!(fx, Effects { notify_volume: true, notify_mute: false, avrcp_event: true });
    assert!(v.muted());
    assert_eq!(v.remote_get(), LEVEL_MIN, "the buds are sent to 0");
    v.windows_mute(CHANNEL_MASTER, false, 0).unwrap();
    assert_eq!(v.remote_get(), avrcp_to_level(100));
}

#[test]
fn wire_report() {
    let mut v = connected(64);
    v.windows_level(0, lvl(-30.0), 0).unwrap();
    v.remote_get();
    v.event_generated();
    let (flags, st) = v.wire();
    assert_eq!(flags, AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP | AUDIO_FLAG_AVRCP_SUBSCRIBED);
    assert_eq!(
        st,
        VolumeState {
            level: avrcp_to_level(64),
            left: lvl(-30.0),
            right: avrcp_to_level(64),
            avrcp: 64,
            gets: 1,
            sets: 1,
            events: 1,
        }
    );
}

// ---------------------------------------------------------------------------
// Closed loop: Windows (audiosrv) + BthAvrcp + buds
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Msg {
    /// AcxVolumeChangeLevelNotification reached audiosrv.
    WindowsNotified,
    /// The AVRCP event fired.
    Event,
    /// SetAbsoluteVolume reached the buds.
    BudsSet(u8),
    /// CHANGED / INTERIM from the buds reached BthAvrcp.
    BudsReport(u8),
    /// BthAvrcp's `EnableResponseProcessingEntry`: processing on, SET of the
    /// cached byte (inbox model only).
    Enable,
}

/// How audiosrv writes a level back after reading it.
#[derive(Clone, Copy)]
enum Grid {
    Nearest,
    Floor,
    Exact,
}

struct Loop {
    v: Volume,
    q: VecDeque<Msg>,
    /// BthAvrcp's cached AVRCP byte (`this+0xE9`).
    cached: Option<u8>,
    buds: u8,
    /// The buds answer every SetAbsoluteVolume with a CHANGED too.
    chatty: bool,
    grid: Grid,
    sent: Vec<u8>,
    steps: u32,
    /// Model BthAvrcp as disassembled: reports are dropped when their
    /// integer percent equals the cached byte's, and SET only once response
    /// processing is on (`this+0xE8`).
    inbox: bool,
    processing: bool,
}

/// BthAvrcp's percent compare (`v * 100 / 127`).
fn pct(v: u8) -> u32 {
    u32::from(v & 0x7F) * 100 / 127
}

impl Loop {
    fn new(buds: u8, chatty: bool, grid: Grid) -> Self {
        let mut v = Volume::new();
        v.set_element(true);
        Loop {
            v,
            q: VecDeque::new(),
            cached: None,
            buds,
            chatty,
            grid,
            sent: Vec::new(),
            steps: 0,
            inbox: false,
            processing: true,
        }
    }
    /// A fresh BthAvrcp `VolumeController`: cached 64, processing off.
    fn new_inbox(buds: u8, chatty: bool, grid: Grid) -> Self {
        Loop { cached: Some(64), inbox: true, processing: false, ..Self::new(buds, chatty, grid) }
    }
    fn apply(&mut self, fx: Effects) {
        if fx.notify_volume || fx.notify_mute {
            self.q.push_back(Msg::WindowsNotified);
        }
        if fx.avrcp_event {
            self.v.event_generated();
            self.q.push_back(Msg::Event);
        }
    }
    fn quantise(&self, l: i32) -> i32 {
        match self.grid {
            Grid::Nearest => (l + LEVEL_STEP / 2).div_euclid(LEVEL_STEP) * LEVEL_STEP,
            Grid::Floor => l.div_euclid(LEVEL_STEP) * LEVEL_STEP,
            Grid::Exact => l,
        }
    }
    fn run(&mut self) {
        self.steps = 0;
        while let Some(m) = self.q.pop_front() {
            self.steps += 1;
            assert!(self.steps < 100, "no settling: {:?}", self.sent);
            match m {
                Msg::WindowsNotified => {
                    // audiosrv reads both channels and writes them back on its grid.
                    for ch in 0..2 {
                        let l = self.v.retrieve_level(ch).unwrap();
                        let fx = self.v.windows_level(ch, self.quantise(l), 0).unwrap();
                        self.apply(fx);
                    }
                }
                Msg::Event => {
                    let a = level_to_avrcp(self.v.remote_get());
                    if self.cached != Some(a) {
                        self.cached = Some(a);
                        self.q.push_back(Msg::BudsSet(a));
                    }
                }
                Msg::BudsSet(a) => {
                    self.sent.push(a);
                    self.buds = a;
                    if self.chatty {
                        self.q.push_back(Msg::BudsReport(a));
                    }
                }
                Msg::BudsReport(a) => {
                    if self.inbox && self.cached.is_some_and(|c| pct(c) == pct(a)) {
                        continue;
                    }
                    self.cached = Some(a);
                    if self.processing {
                        let fx = self.v.remote_set(avrcp_to_level(a), 0);
                        self.apply(fx);
                    }
                }
                Msg::Enable => {
                    self.processing = true;
                    let c = self.cached.expect("inbox model");
                    let fx = self.v.remote_set(avrcp_to_level(c), 0);
                    self.apply(fx);
                }
            }
        }
    }
    fn connect(&mut self) {
        self.v.subscribe(true);
        self.q.push_back(Msg::BudsReport(self.buds));
        self.run();
    }
}

#[test]
fn closed_loop_settles_for_every_level_and_buds_behaviour() {
    for grid in [Grid::Nearest, Grid::Floor, Grid::Exact] {
        for chatty in [false, true] {
            for start in 0..=127u8 {
                let mut l = Loop::new(start, chatty, grid);
                l.connect();
                // Connect: the buds keep their level, nothing is sent to them.
                assert!(l.sent.is_empty(), "connect at {start} sent {:?}", l.sent);
                assert_eq!(level_to_avrcp(l.v.remote_get()), start);
                // A gesture on the buds to every other level: the buds are
                // never sent anything back.
                for target in [0u8, 1, 13, 64, 100, 126, 127, start.wrapping_add(1) & 0x7F] {
                    l.q.push_back(Msg::BudsReport(target));
                    l.buds = target;
                    l.run();
                    assert!(l.sent.is_empty(), "gesture {start}->{target} echoed {:?}", l.sent);
                    assert_eq!(level_to_avrcp(l.v.remote_get()), target);
                }
            }
        }
    }
}

#[test]
fn closed_loop_slider_moves_reach_the_buds_once() {
    for grid in [Grid::Nearest, Grid::Floor, Grid::Exact] {
        for chatty in [false, true] {
            let mut l = Loop::new(64, chatty, grid);
            l.connect();
            for pct in (0..=100u32).step_by(3) {
                let p = f64::from(pct) / 100.0;
                let level = l.quantise(lvl(taper_db(p)));
                l.sent.clear();
                for ch in 0..2 {
                    let fx = l.v.windows_level(ch, level, 0).unwrap();
                    l.apply(fx);
                }
                l.run();
                let want = level_to_avrcp(level);
                assert_eq!(l.buds, want, "{pct} %");
                assert!(l.sent.len() <= 1, "{pct} %: sent {:?}", l.sent);
                // The slider stays where the user put it.
                assert_eq!(l.v.retrieve_level(0), Ok(level), "{pct} %");
            }
        }
    }
}

#[test]
fn closed_loop_mute_round_trip() {
    let mut l = Loop::new(90, true, Grid::Nearest);
    l.connect();
    let fx = l.v.windows_mute(CHANNEL_MASTER, true, 0).unwrap();
    l.apply(fx);
    l.run();
    assert_eq!(l.buds, 0);
    assert!(l.v.muted(), "the buds' CHANGED 0 does not unmute");
    let fx = l.v.windows_mute(CHANNEL_MASTER, false, 0).unwrap();
    l.apply(fx);
    l.run();
    assert_eq!(l.buds, 90);
    assert!(!l.v.muted());
}

/// When BthAvrcp's INTERIM arrives relative to `EnableResponseProcessingEntry`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Order {
    /// INTERIM first: one SET with the buds' level.
    InterimFirst,
    /// Enable first, INTERIM queued right behind it.
    EnableFirst,
    /// Enable first and everything it caused settled before the INTERIM,
    /// which still carries the level the buds had at connect.
    InterimLate,
    /// As `InterimLate`, but the INTERIM carries the buds' level after the
    /// mute reached them (0 when muted).
    InterimAfterMute,
}

#[test]
fn closed_loop_connect_in_either_order() {
    for order in [Order::InterimFirst, Order::EnableFirst, Order::InterimLate, Order::InterimAfterMute] {
        for muted in [false, true] {
            for grid in [Grid::Nearest, Grid::Floor, Grid::Exact] {
                for chatty in [false, true] {
                    for start in 0..=127u8 {
                        let mut l = Loop::new_inbox(start, chatty, grid);
                        let windows = lvl(-25.0);
                        l.v.windows_level(CHANNEL_MASTER, windows, 0).unwrap();
                        if muted {
                            l.v.windows_mute(CHANNEL_MASTER, true, 0).unwrap();
                        }
                        l.v.subscribe(true);
                        match order {
                            Order::InterimFirst => l.q.extend([Msg::BudsReport(start), Msg::Enable]),
                            Order::EnableFirst => l.q.extend([Msg::Enable, Msg::BudsReport(start)]),
                            Order::InterimLate | Order::InterimAfterMute => {
                                l.q.push_back(Msg::Enable);
                                l.run();
                                let r = if order == Order::InterimLate { start } else { l.buds };
                                l.q.push_back(Msg::BudsReport(r));
                            }
                        }
                        l.run();
                        let at = format!("{order:?} muted={muted} chatty={chatty} start={start}");
                        assert_eq!(l.v.muted(), muted, "{at}");
                        // An INTERIM in the same percent as the 0 BthAvrcp has
                        // just sent for the mute is dropped by its percent
                        // compare: Windows keeps 64 (BthAvrcp's default), and
                        // unmuting sends the buds there. Always the case when
                        // the INTERIM reflects the muted buds.
                        let lost = muted
                            && (order == Order::InterimAfterMute
                                || (order == Order::InterimLate && pct(start) == pct(0)));
                        let want = if lost { 64 } else { start };
                        assert_eq!(level_to_avrcp(l.v.level()), want, "{at}");
                        if muted {
                            assert_eq!(l.buds, 0, "{at}: sent {:?}", l.sent);
                        } else {
                            assert_eq!(l.buds, start, "{at}");
                            assert!(l.sent.is_empty(), "{at}: sent {:?}", l.sent);
                        }
                        // Unmuting sends the connect level.
                        let fx = l.v.windows_mute(CHANNEL_MASTER, false, 10).unwrap();
                        l.apply(fx);
                        l.run();
                        assert_eq!(l.buds, want, "{at}: after unmute");
                    }
                }
            }
        }
    }
}
