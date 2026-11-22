//! Samsung SSC over A2DP: the capability octet, mode negotiation, the bitrate
//! ladders and adaptive bitrate, and the media packet header. No FFI here.
//!
//! Wire facts follow crossc (`crates/avdtp/src/codec.rs`, `docs/DESIGN.md`).

use ssc_enc::{Bitrate, Mode};

pub const VENDOR_ID: u32 = 0x0000_0075;
pub const CODEC_ID: u16 = 0x0103;

/// Bits of the single SSC capability / configuration octet.
pub mod flags {
    pub const LEGACY_96K: u8 = 0x80;
    pub const SR_44K1: u8 = 0x20;
    pub const SR_48K: u8 = 0x10;
    pub const NO_LIMIT_24BIT: u8 = 0x08;
    pub const HIFI_48K_24: u8 = 0x04;
    pub const UHQ_V2_96K_24: u8 = 0x02;
    /// Sink-side only: the sink has UHQ v2 turned off.
    pub const SINK_DISABLES_UHQ: u8 = 0x01;
}

/// What our source endpoint offers: every mode we encode.
pub const LOCAL_CAPS: u8 = flags::SR_44K1
    | flags::SR_48K
    | flags::NO_LIMIT_24BIT
    | flags::HIFI_48K_24
    | flags::UHQ_V2_96K_24;

/// Codec information element: vendor id (LE), codec id (LE), flags.
pub const ELEMENT_LEN: usize = 7;

pub fn parse_element(data: &[u8]) -> Option<u8> {
    // Some sinks append octets after the flags; only the first one is defined.
    let d = data.get(..ELEMENT_LEN)?;
    let vendor = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
    let codec = u16::from_le_bytes([d[4], d[5]]);
    (vendor == VENDOR_ID && codec == CODEC_ID).then_some(d[6])
}

pub fn element(flags: u8) -> [u8; ELEMENT_LEN] {
    let v = VENDOR_ID.to_le_bytes();
    let c = CODEC_ID.to_le_bytes();
    [v[0], v[1], v[2], v[3], c[0], c[1], flags]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rung {
    pub bitrate: Bitrate,
    pub frames_per_packet: u8,
}

const fn rung(bitrate: Bitrate, frames_per_packet: u8) -> Rung {
    Rung {
        bitrate,
        frames_per_packet,
    }
}

/// Only rates Galaxy Buds are known to decode, highest first. The two lower
/// UHQ rates carry two frames per packet, as Samsung phones send them.
const LADDER_HIFI: &[Rung] = &[
    rung(Bitrate::K229, 1),
    rung(Bitrate::K192, 1),
    rung(Bitrate::K128, 1),
    rung(Bitrate::K88, 1),
];
const LADDER_SSC: &[Rung] = &[
    rung(Bitrate::K229, 1),
    rung(Bitrate::K128, 1),
    rung(Bitrate::K88, 1),
];
const LADDER_UHQ: &[Rung] = &[
    rung(Bitrate::K584, 1),
    rung(Bitrate::K291, 2),
    rung(Bitrate::K152, 2),
];

pub const MAX_RUNGS: usize = 4;
pub const MAX_FRAMES_PER_PACKET: usize = 2;

#[derive(Debug, PartialEq, Eq)]
pub struct SscMode {
    pub name: &'static str,
    pub bit: u8,
    pub enc: Mode,
    /// 24-bit modes take 32-bit input; plain SSC is 16-bit.
    pub hires: bool,
    pub ladder: &'static [Rung],
}

impl SscMode {
    pub const fn rate(&self) -> u32 {
        self.enc.sample_rate()
    }
}

pub static UHQ: SscMode = SscMode {
    name: "uhq",
    bit: flags::UHQ_V2_96K_24,
    enc: Mode::Uhq96k,
    hires: true,
    ladder: LADDER_UHQ,
};
pub static HIFI: SscMode = SscMode {
    name: "hifi",
    bit: flags::HIFI_48K_24,
    enc: Mode::Hifi48k,
    hires: true,
    ladder: LADDER_HIFI,
};
pub static SSC48: SscMode = SscMode {
    name: "ssc48",
    bit: flags::SR_48K,
    enc: Mode::Ssc48k,
    hires: false,
    ladder: LADDER_SSC,
};
pub static SSC44: SscMode = SscMode {
    name: "ssc44",
    bit: flags::SR_44K1,
    enc: Mode::Ssc44k1,
    hires: false,
    ladder: LADDER_SSC,
};

/// Precedence when reading a configuration octet (UHQ 0x0E is UHQ, not HiFi).
static BY_PRECEDENCE: [&SscMode; 4] = [&UHQ, &HIFI, &SSC48, &SSC44];

pub fn mode_from_config(config: u8) -> Option<&'static SscMode> {
    // 0x01 is the sink saying it has UHQ off; it is never valid in a configuration.
    if config & flags::SINK_DISABLES_UHQ != 0 {
        return None;
    }
    BY_PRECEDENCE.into_iter().find(|m| config & m.bit != 0)
}

pub fn mode_by_name(name: &str) -> Option<&'static SscMode> {
    BY_PRECEDENCE.into_iter().find(|m| m.name == name)
}

fn usable(m: &SscMode, caps: u8) -> bool {
    caps & m.bit != 0 && !(m.bit == flags::UHQ_V2_96K_24 && caps & flags::SINK_DISABLES_UHQ != 0)
}

/// The two codecs the plugin registers on the one SSC endpoint. PipeWire
/// shows each as a profile, so switching between them renegotiates the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// `ssc`: HiFi 48 kHz/24, else plain SSC 48 / 44.1 kHz. Never UHQ.
    Main,
    /// `ssc_uhq`: UHQ 96 kHz/24 only.
    Uhq,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Preferences {
    /// `bluez5.a2dp.ssc.mode` for the main codec (`hifi`, `ssc48`, `ssc44`); `None` is automatic.
    pub mode: Option<&'static SscMode>,
    /// `bluez5.a2dp.ssc.uhq-alt-config`: send UHQ as `0x0E` instead of `0x0A`.
    pub uhq_alt_config: bool,
}

/// The configuration octet for a sink's capability octet, or `None` if the
/// sink offers nothing this variant encodes.
pub fn select_config(caps: u8, variant: Variant, prefs: Preferences) -> Option<u8> {
    let m = match variant {
        Variant::Uhq => Some(&UHQ).filter(|m| usable(m, caps))?,
        Variant::Main => prefs
            .mode
            .filter(|m| m.bit != flags::UHQ_V2_96K_24 && usable(m, caps))
            .or_else(|| {
                [&HIFI, &SSC48, &SSC44]
                    .into_iter()
                    .find(|m| usable(m, caps))
            })?,
    };
    let mut config = m.bit | (caps & flags::NO_LIMIT_24BIT);
    if m.bit == flags::UHQ_V2_96K_24 && prefs.uhq_alt_config && caps & flags::HIFI_48K_24 != 0 {
        config |= flags::HIFI_48K_24;
    }
    Some(config)
}

pub const RTP_HEADER_LEN: usize = 12;

pub fn frame_bytes(mode: &SscMode, r: Rung) -> usize {
    ssc_enc::frame_size(mode.enc, r.bitrate)
}

pub fn packet_bytes(mode: &SscMode, r: Rung) -> usize {
    RTP_HEADER_LEN + usize::from(r.frames_per_packet) * frame_bytes(mode, r)
}

/// The rungs a stream may use: the mode's ladder, capped at `max_bps` (the
/// lowest rung always stays) and at what fits the media MTU.
pub fn ladder(mode: &SscMode, mtu: usize, max_bps: Option<u32>) -> Vec<Rung> {
    let lowest = mode.ladder.len() - 1;
    mode.ladder
        .iter()
        .enumerate()
        .filter(|&(i, r)| i == lowest || max_bps.is_none_or(|max| r.bitrate.bps() <= max))
        .map(|(_, r)| *r)
        .filter(|&r| packet_bytes(mode, r) <= mtu)
        .collect()
}

/// Hold before a step up, in `increase_bitpool` calls (media-sink makes at most
/// one a second, only while the socket queue is empty).
const HOLD_MIN: u32 = 10;
const HOLD_MAX: u32 = 80;
/// A step down within this many clean calls after a step up is a failed probe.
const PROBE_WINDOW: u32 = 15;

/// Adaptive bitrate: down at once on a full socket, up slowly, and each rung
/// that failed a probe is tried half as often (crossc `abr.rs`, simplified to
/// the signals PipeWire's media sink gives a codec).
#[derive(Debug)]
pub struct Abr {
    rungs: Vec<Rung>,
    cur: usize,
    clean: u32,
    since_up: Option<u32>,
    /// Clean calls needed before stepping up to each rung.
    hold: [u32; MAX_RUNGS],
}

impl Abr {
    pub fn new(rungs: Vec<Rung>, adaptive: bool) -> Option<Abr> {
        let mut rungs = rungs;
        if rungs.is_empty() || rungs.len() > MAX_RUNGS {
            return None;
        }
        if !adaptive {
            rungs.truncate(1);
        }
        Some(Abr {
            rungs,
            cur: 0,
            clean: 0,
            since_up: None,
            hold: [HOLD_MIN; MAX_RUNGS],
        })
    }

    pub fn current(&self) -> Rung {
        self.rungs[self.cur]
    }

    pub fn top(&self) -> Rung {
        self.rungs[0]
    }

    pub fn is_adaptive(&self) -> bool {
        self.rungs.len() > 1
    }

    /// The link could not take a packet. Returns the new rung if it changed.
    pub fn congested(&mut self) -> Option<Rung> {
        self.clean = 0;
        if self.cur + 1 >= self.rungs.len() {
            return None;
        }
        // Leaving a rung soon after stepping up to it: the probe failed.
        if self.since_up.is_some_and(|n| n < PROBE_WINDOW) {
            self.hold[self.cur] = (self.hold[self.cur] * 2).min(HOLD_MAX);
        }
        self.since_up = None;
        self.cur += 1;
        Some(self.current())
    }

    /// A second without a queued packet. Returns the new rung if it changed.
    pub fn clean(&mut self) -> Option<Rung> {
        if let Some(n) = self.since_up.as_mut() {
            *n += 1;
            if *n >= PROBE_WINDOW {
                self.since_up = None;
                self.hold[self.cur] = (self.hold[self.cur] / 2).max(HOLD_MIN);
            }
        }
        if self.cur == 0 {
            return None;
        }
        self.clean += 1;
        if self.clean < self.hold[self.cur - 1] {
            return None;
        }
        self.clean = 0;
        self.since_up = Some(0);
        self.cur -= 1;
        Some(self.current())
    }
}

/// RTP header: V=2, no padding/extension/CSRC, M=0, PT=96, SSRC=1. Samsung
/// phones advance the timestamp by one per packet, not by samples.
pub fn rtp_header(seq: u16, packet_index: u32) -> [u8; RTP_HEADER_LEN] {
    let mut h = [0u8; RTP_HEADER_LEN];
    h[0] = 0x80;
    h[1] = 96;
    h[2..4].copy_from_slice(&seq.to_be_bytes());
    h[4..8].copy_from_slice(&packet_index.to_be_bytes());
    h[8..12].copy_from_slice(&1u32.to_be_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTO: Preferences = Preferences {
        mode: None,
        uhq_alt_config: false,
    };

    #[test]
    fn element_round_trip_and_extra_octets() {
        assert_eq!(element(0x3E), [0x75, 0, 0, 0, 0x03, 0x01, 0x3E]);
        assert_eq!(parse_element(&element(0x3E)), Some(0x3E));
        assert_eq!(
            parse_element(&[0x75, 0, 0, 0, 0x03, 0x01, 0x16, 0xAA]),
            Some(0x16)
        );
        assert_eq!(parse_element(&[0x75, 0, 0, 0, 0x03, 0x01]), None);
        assert_eq!(parse_element(&[0x75, 0, 0, 0, 0x02, 0x01, 0x10]), None);
        assert_eq!(LOCAL_CAPS, 0x3E);
    }

    #[test]
    fn config_octets_match_the_reference() {
        use Variant::{Main, Uhq};
        // crossc DESIGN.md "Format -> mode".
        assert_eq!(select_config(0x3E, Main, AUTO), Some(0x0C), "HiFi");
        assert_eq!(select_config(0x3E, Uhq, AUTO), Some(0x0A), "UHQ");
        assert_eq!(select_config(0x3F, Uhq, AUTO), None, "sink disables UHQ");
        assert_eq!(select_config(0x3F, Main, AUTO), Some(0x0C));
        assert_eq!(
            select_config(0x38, Main, AUTO),
            Some(0x18),
            "48 kHz SSC only, with 0x08"
        );
        assert_eq!(select_config(0x38, Uhq, AUTO), None);
        assert_eq!(
            select_config(0x10, Main, AUTO),
            Some(0x10),
            "0x08 only when offered"
        );
        assert_eq!(select_config(0x28, Main, AUTO), Some(0x28));
        assert_eq!(
            select_config(0x02, Main, AUTO),
            None,
            "the main codec never sends UHQ"
        );
        assert_eq!(
            select_config(0x80, Main, AUTO),
            None,
            "legacy UHQ is not encoded"
        );
    }

    #[test]
    fn preferences() {
        use Variant::{Main, Uhq};
        let alt = Preferences {
            mode: None,
            uhq_alt_config: true,
        };
        assert_eq!(select_config(0x3E, Uhq, alt), Some(0x0E));
        assert_eq!(select_config(0x3E, Main, alt), Some(0x0C));
        let ssc48 = Preferences {
            mode: Some(&SSC48),
            uhq_alt_config: false,
        };
        assert_eq!(select_config(0x3E, Main, ssc48), Some(0x18));
        let uhq = Preferences {
            mode: Some(&UHQ),
            uhq_alt_config: false,
        };
        assert_eq!(
            select_config(0x3E, Main, uhq),
            Some(0x0C),
            "mode=uhq is the ssc_uhq codec's job"
        );
        let hifi = Preferences {
            mode: Some(&HIFI),
            uhq_alt_config: false,
        };
        assert_eq!(
            select_config(0x38, Main, hifi),
            Some(0x18),
            "falls back when not offered"
        );
        assert_eq!(mode_by_name("ssc44"), Some(&SSC44));
    }

    #[test]
    fn config_precedence() {
        assert_eq!(mode_from_config(0x0A), Some(&UHQ));
        assert_eq!(mode_from_config(0x0E), Some(&UHQ));
        assert_eq!(mode_from_config(0x0C), Some(&HIFI));
        assert_eq!(mode_from_config(0x18), Some(&SSC48));
        assert_eq!(mode_from_config(0x28), Some(&SSC44));
        assert_eq!(mode_from_config(0x08), None);
        assert_eq!(mode_from_config(0x03), None, "0x01 is sink-only");
    }

    #[test]
    fn packet_sizes_match_the_reference() {
        // crossc DESIGN.md "Ladders": packet bytes per rung.
        let sizes = |m: &SscMode| {
            m.ladder
                .iter()
                .map(|&r| packet_bytes(m, r))
                .collect::<Vec<_>>()
        };
        assert_eq!(sizes(&HIFI), [588, 496, 336, 236]);
        assert_eq!(sizes(&SSC48), [588, 336, 236]);
        assert_eq!(sizes(&UHQ), [672, 672, 360]);
    }

    #[test]
    fn ladder_respects_mtu_and_cap() {
        let rates = |v: Vec<Rung>| v.iter().map(|r| r.bitrate.bps() / 1000).collect::<Vec<_>>();
        assert_eq!(rates(ladder(&UHQ, 1005, None)), [584, 291, 152]);
        assert_eq!(rates(ladder(&UHQ, 671, None)), [152]);
        assert_eq!(rates(ladder(&HIFI, 1005, Some(192_000))), [192, 128, 88]);
        assert_eq!(
            rates(ladder(&HIFI, 1005, Some(1))),
            [88],
            "the lowest rung survives any cap"
        );
        assert!(ladder(&HIFI, 200, None).is_empty());
    }

    #[test]
    fn abr_steps_down_at_once_and_up_after_the_hold() {
        let mut a = Abr::new(ladder(&HIFI, 1005, None), true).unwrap();
        assert_eq!(a.clean(), None, "already at the top");
        assert_eq!(a.congested().map(|r| r.bitrate), Some(Bitrate::K192));
        assert_eq!(a.congested().map(|r| r.bitrate), Some(Bitrate::K128));
        for _ in 0..HOLD_MIN - 1 {
            assert_eq!(a.clean(), None);
        }
        assert_eq!(a.clean().map(|r| r.bitrate), Some(Bitrate::K192));
    }

    #[test]
    fn abr_failed_probe_doubles_the_hold() {
        let mut a = Abr::new(ladder(&HIFI, 1005, None), true).unwrap();
        a.congested();
        let mut calls = 0;
        while a.clean().is_none() {
            calls += 1;
        }
        assert_eq!(calls + 1, HOLD_MIN);
        a.congested(); // probe of 229 failed at once
        calls = 0;
        while a.clean().is_none() {
            calls += 1;
        }
        assert_eq!(calls + 1, 2 * HOLD_MIN);
    }

    #[test]
    fn fixed_rate_never_moves() {
        let mut a = Abr::new(ladder(&UHQ, 1005, None), false).unwrap();
        assert!(!a.is_adaptive());
        assert_eq!(a.congested(), None);
        assert_eq!(a.current().bitrate, Bitrate::K584);
    }

    #[test]
    fn rtp_header_bytes() {
        assert_eq!(
            rtp_header(0x1234, 7),
            [0x80, 96, 0x12, 0x34, 0, 0, 0, 7, 0, 0, 0, 1]
        );
    }
}
