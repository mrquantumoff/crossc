//! Mode selection: which SSC mode the endpoint's current PCM format is sent
//! in, given what the buds' SSC capability octet offers. A pure function; the
//! table is the one in docs/DESIGN.md ("Format -> mode"):
//!
//! | Endpoint format | Buds offer | Mode sent |
//! |---|---|---|
//! | 96 kHz | UHQ v2 (`0x02`, and not `0x01`) | UHQ, config `0x0A`, 584 kbps, one frame per packet |
//! | 96 kHz | no UHQ | resampled to 48 kHz, then as the 48 kHz row |
//! | 48 kHz | HiFi (`0x04`) | HiFi, config `0x0C`, 229 kbps, 24-bit (16-bit input widened) |
//! | 48 kHz | 48 kHz SSC only (`0x10`) | SSC 48 kHz, config `0x10` (+`0x08` if offered), 16-bit (24-bit input dithered down) |
//! | any | 44.1 kHz SSC only (`0x20`) | resampled to 44.1 kHz, SSC config `0x20` (+`0x08`) |
//!
//! One departure from the table as written: it gives the octets of the UHQ,
//! HiFi and 44.1 kHz rows with `0x08` ("no bitrate limit / 24-bit")
//! unconditionally, and says "if offered" only on the 48 kHz SSC row. Here
//! `0x08` is sent only if the sink offers it, in every row, so that a
//! configuration never contains a bit the sink did not advertise (the rule
//! `crossc-cli play`, the path proven on real buds, enforces unless forced).
//! For every sink that offers `0x08`, the Buds4 Pro's `0x3E` among them, the
//! octets are exactly the table's; for one that does not, `0x02` / `0x04` /
//! `0x20` go out where the table says `0x0A` / `0x0C` / `0x28`. Which of the
//! two such a sink expects is unknown: none has been seen.
//!
//! Two things the table leaves open are decided here:
//! * Plain SSC (the 16-bit rows) is sent at [`SSC_BITRATE`].
//! * An endpoint rate other than 48 or 96 kHz is treated as the 48 kHz rows
//!   with resampling to the mode's rate.

use avdtp::codec::SscFlags;
use avdtp::rtp::RtpHeader;
use crossc_ioctl::audio::PcmFormat;
use ssc_enc::{Bitrate, Mode};

use crate::pcm::InputKind;
use crate::settings::Settings;

/// Bitrate of the plain (16-bit) SSC modes: the top rate of the family that
/// codes 16-bit input at its own scale.
pub const SSC_BITRATE: Bitrate = Bitrate::K229;

/// The SSC modes the service sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SscMode {
    /// UHQ v2, 96 kHz / 24-bit.
    Uhq,
    /// Seamless HiFi, 48 kHz / 24-bit.
    Hifi,
    /// Plain SSC, 48 kHz / 16-bit.
    Ssc48,
    /// Plain SSC, 44.1 kHz / 16-bit.
    Ssc44,
}

impl SscMode {
    pub fn encoder_mode(self) -> Mode {
        match self {
            SscMode::Uhq => Mode::Uhq96k,
            SscMode::Hifi => Mode::Hifi48k,
            SscMode::Ssc48 => Mode::Ssc48k,
            SscMode::Ssc44 => Mode::Ssc44k1,
        }
    }
    pub fn sample_rate(self) -> u32 {
        self.encoder_mode().sample_rate()
    }
    /// 24-bit modes.
    pub fn hires(self) -> bool {
        matches!(self, SscMode::Uhq | SscMode::Hifi)
    }
    pub fn name(self) -> &'static str {
        match self {
            SscMode::Uhq => "UHQ 96 kHz/24",
            SscMode::Hifi => "HiFi 48 kHz/24",
            SscMode::Ssc48 => "SSC 48 kHz/16",
            SscMode::Ssc44 => "SSC 44.1 kHz/16",
        }
    }
    /// Rates to fall back to when a packet does not fit the media MTU,
    /// highest first: the ones the buds are known to decode.
    fn ladder(self) -> &'static [Bitrate] {
        crate::abr::decodable(self.encoder_mode())
    }
}

/// How one endpoint format is streamed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub mode: SscMode,
    /// SSC configuration octet for SET_CONFIGURATION.
    pub config: u8,
    pub encoder: Mode,
    /// The configured bitrate: what the stream is sent at, and with
    /// adaptive bitrate the highest it is sent at ([`crate::abr`]).
    pub bitrate: Bitrate,
    /// SSC frames per media packet (at `bitrate`).
    pub frames_per_packet: usize,
    /// Rate the endpoint PCM is converted to first (`None`: none needed).
    pub resample_to: Option<u32>,
    /// Encoder input format.
    pub input: InputKind,
}

impl Plan {
    pub fn frame_bytes(&self) -> usize {
        ssc_enc::frame_size(self.encoder, self.bitrate)
    }
    pub fn packet_bytes(&self) -> usize {
        RtpHeader::LEN + self.frames_per_packet * self.frame_bytes()
    }
    /// Same AVDTP configuration: switching between two such plans needs no
    /// CLOSE / SET_CONFIGURATION, only another conversion of the input.
    pub fn same_stream(&self, other: &Plan) -> bool {
        self.config == other.config
            && self.mode == other.mode
            && self.bitrate == other.bitrate
            && self.frames_per_packet == other.frames_per_packet
    }
    /// Lower the bitrate until a packet fits `limit` bytes. `Ok(Some(old))`
    /// if it was lowered, `Err` if not even the lowest rate fits.
    pub fn fit_mtu(&mut self, limit: usize) -> Result<Option<Bitrate>, String> {
        if self.packet_bytes() <= limit {
            return Ok(None);
        }
        let old = self.bitrate;
        for &b in self.mode.ladder() {
            if b.bps() < old.bps() {
                self.bitrate = b;
                if self.packet_bytes() <= limit {
                    return Ok(Some(old));
                }
            }
        }
        self.bitrate = old;
        Err(format!(
            "{}-byte packets ({}, {} kbps) exceed the media MTU limit {limit}, and no lower rate fits",
            self.packet_bytes(),
            self.mode.name(),
            old.bps() / 1000
        ))
    }
    /// One line for the log.
    pub fn describe(&self) -> String {
        format!(
            "{}: config {:#04x}, {} kbps, {} frame(s)/packet, {}-byte packets, input {:?}{}",
            self.mode.name(),
            self.config,
            self.bitrate.bps() / 1000,
            self.frames_per_packet,
            self.packet_bytes(),
            self.input,
            match self.resample_to {
                Some(r) => format!(", resampled to {r} Hz"),
                None => String::new(),
            }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// Not a format the audio contract allows.
    BadFormat(PcmFormat),
    /// The capability octet offers no mode this service can send.
    NoMode(u8),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::BadFormat(p) => write!(
                f,
                "unsupported endpoint format: {} Hz, {} channel(s), {} valid bits in {}",
                p.sample_rate, p.channels, p.valid_bits, p.container_bits
            ),
            PlanError::NoMode(caps) => write!(
                f,
                "the SSC capability octet {caps:#04x} offers none of UHQ (0x02), HiFi (0x04), 48 kHz (0x10) or 44.1 kHz (0x20)"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// The plan for endpoint format `fmt` on a sink whose SSC capability octet
/// is `caps`. See the module docs for the table.
pub fn select_plan(fmt: PcmFormat, caps: SscFlags, s: &Settings) -> Result<Plan, PlanError> {
    if !fmt.is_valid() {
        return Err(PlanError::BadFormat(fmt));
    }
    let (mode, wanted, bitrate) = if fmt.sample_rate == 96_000 && caps.uhq_v2_usable() {
        (SscMode::Uhq, s.uhq_config, s.uhq_bitrate)
    } else if caps.hifi_48k_24() {
        (SscMode::Hifi, SscFlags::CONFIG_HIFI, s.hifi_bitrate)
    } else if caps.has(SscFlags::SR_48K) {
        (SscMode::Ssc48, SscFlags::CONFIG_SSC_48K, SSC_BITRATE)
    } else if caps.has(SscFlags::SR_44K1) {
        (SscMode::Ssc44, SscFlags::CONFIG_SSC_44K1, SSC_BITRATE)
    } else {
        return Err(PlanError::NoMode(caps.0));
    };
    let resample_to = (fmt.sample_rate != mode.sample_rate()).then_some(mode.sample_rate());
    let input = match resample_to {
        // The resampler works in floating point; its output is quantised
        // to the mode's depth.
        Some(_) if mode.hires() => InputKind::I24,
        _ => InputKind::for_format(fmt, mode.hires()),
    };
    Ok(Plan {
        mode,
        // Never a bit the sink does not offer (drops 0x08 where it is absent).
        config: wanted & caps.0,
        encoder: mode.encoder_mode(),
        bitrate,
        frames_per_packet: 1,
        resample_to,
        input,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn fmt(rate: u32, container: u8, valid: u8) -> PcmFormat {
        PcmFormat {
            sample_rate: rate,
            channels: 2,
            container_bits: container,
            valid_bits: valid,
        }
    }
    const F96: PcmFormat = fmt(96000, 32, 24);
    const F48: PcmFormat = fmt(48000, 32, 24);
    const F48_16: PcmFormat = fmt(48000, 16, 16);

    fn plan(f: PcmFormat, caps: u8) -> Result<Plan, PlanError> {
        select_plan(f, SscFlags(caps), &Settings::default())
    }

    /// (mode, config, kbps, resample, input) of a plan.
    type Row = (SscMode, u8, u32, Option<u32>, InputKind);

    fn row(p: Plan) -> Row {
        assert_eq!(p.frames_per_packet, 1, "one frame per packet");
        assert_eq!(p.encoder, p.mode.encoder_mode());
        (
            p.mode,
            p.config,
            p.bitrate.bps() / 1000,
            p.resample_to,
            p.input,
        )
    }

    use InputKind::{I16, I24};
    use SscMode::{Hifi, Ssc44, Ssc48, Uhq};

    #[test]
    fn buds4_pro_caps_0x3e() {
        assert_eq!(row(plan(F96, 0x3E).unwrap()), (Uhq, 0x0A, 584, None, I24));
        assert_eq!(row(plan(F48, 0x3E).unwrap()), (Hifi, 0x0C, 229, None, I24));
        assert_eq!(
            row(plan(F48_16, 0x3E).unwrap()),
            (Hifi, 0x0C, 229, None, I24),
            "16-bit input widened"
        );
        let p = plan(F96, 0x3E).unwrap();
        assert_eq!((p.frame_bytes(), p.packet_bytes()), (660, 672));
        let p = plan(F48, 0x3E).unwrap();
        assert_eq!((p.frame_bytes(), p.packet_bytes()), (576, 588));
    }

    #[test]
    fn no_uhq_caps_0x3c_resamples_96k_to_hifi() {
        assert_eq!(
            row(plan(F96, 0x3C).unwrap()),
            (Hifi, 0x0C, 229, Some(48000), I24)
        );
        assert_eq!(row(plan(F48, 0x3C).unwrap()), (Hifi, 0x0C, 229, None, I24));
    }

    #[test]
    fn no_hifi_caps_0x30_and_0x38_use_plain_ssc_48k() {
        // 0x38: +0x08 because it is offered; 0x30: without.
        assert_eq!(row(plan(F48, 0x38).unwrap()), (Ssc48, 0x18, 229, None, I16));
        assert_eq!(row(plan(F48, 0x30).unwrap()), (Ssc48, 0x10, 229, None, I16));
        assert_eq!(
            row(plan(F48_16, 0x30).unwrap()),
            (Ssc48, 0x10, 229, None, I16)
        );
        assert_eq!(
            row(plan(F96, 0x38).unwrap()),
            (Ssc48, 0x18, 229, Some(48000), I16)
        );
        assert_eq!(
            row(plan(F96, 0x30).unwrap()),
            (Ssc48, 0x10, 229, Some(48000), I16)
        );
    }

    #[test]
    fn only_44k1_caps_0x20_resamples_everything() {
        for f in [F96, F48, F48_16] {
            assert_eq!(
                row(plan(f, 0x20).unwrap()),
                (Ssc44, 0x20, 229, Some(44100), I16)
            );
            assert_eq!(
                row(plan(f, 0x28).unwrap()),
                (Ssc44, 0x28, 229, Some(44100), I16)
            );
        }
    }

    #[test]
    fn caps_0x00_offer_nothing() {
        for f in [F96, F48, F48_16] {
            assert_eq!(plan(f, 0x00), Err(PlanError::NoMode(0x00)));
        }
        // Bits that name no mode we send do not help either.
        for caps in [0x01, 0x08, 0x40, 0x80, 0xC9] {
            assert_eq!(plan(F48, caps), Err(PlanError::NoMode(caps)));
            assert_eq!(plan(F96, caps), Err(PlanError::NoMode(caps)));
        }
        assert!(
            PlanError::NoMode(0x08).to_string().contains("0x08"),
            "the reason names the octet"
        );
    }

    #[test]
    fn uhq_disable_bit_0x01_turns_uhq_off() {
        // 0x3F = Buds4 Pro caps + "sink disables UHQ v2".
        assert_eq!(
            row(plan(F96, 0x3F).unwrap()),
            (Hifi, 0x0C, 229, Some(48000), I24)
        );
        assert_eq!(row(plan(F48, 0x3F).unwrap()), (Hifi, 0x0C, 229, None, I24));
        // UHQ and nothing else, but disabled: no mode at all.
        assert_eq!(plan(F96, 0x0B), Err(PlanError::NoMode(0x0B)));
        // Disabled UHQ next to plain SSC.
        assert_eq!(
            row(plan(F96, 0x13).unwrap()),
            (Ssc48, 0x10, 229, Some(48000), I16)
        );
        // The disable bit never ends up in a configuration.
        for caps in 0..=255u8 {
            for f in [F96, F48] {
                if let Ok(p) = plan(f, caps) {
                    assert_eq!(p.config & 0x01, 0, "caps {caps:#04x}");
                }
            }
        }
    }

    #[test]
    fn hifi_and_uhq_drop_0x08_when_the_sink_does_not_offer_it() {
        // The departure from the table named in the module docs. 0x16 is the
        // fake sink's default octet.
        assert_eq!(row(plan(F96, 0x16).unwrap()), (Uhq, 0x02, 584, None, I24));
        assert_eq!(row(plan(F48, 0x16).unwrap()), (Hifi, 0x04, 229, None, I24));
        // ... and keep it whenever it is offered.
        assert_eq!(plan(F96, 0x1E).unwrap().config, 0x0A);
        assert_eq!(plan(F48, 0x1E).unwrap().config, 0x0C);
    }

    #[test]
    fn plain_ssc_uses_229_kbps() {
        // First decision of the module docs.
        assert_eq!(SSC_BITRATE, Bitrate::K229);
        assert!(!SSC_BITRATE.is_hq() && !SSC_BITRATE.is_e_family());
        for caps in [0x10, 0x18, 0x20, 0x28, 0x30] {
            assert_eq!(plan(F48, caps).unwrap().bitrate, Bitrate::K229);
        }
    }

    #[test]
    fn other_rates_follow_the_48k_rows_resampled() {
        // Second decision of the module docs.
        let f44 = fmt(44100, 16, 16);
        let f192 = fmt(192_000, 32, 24);
        assert_eq!(
            row(plan(f44, 0x3E).unwrap()),
            (Hifi, 0x0C, 229, Some(48000), I24)
        );
        assert_eq!(
            row(plan(f192, 0x3E).unwrap()),
            (Hifi, 0x0C, 229, Some(48000), I24),
            "UHQ is for 96 kHz only"
        );
        assert_eq!(
            row(plan(f44, 0x30).unwrap()),
            (Ssc48, 0x10, 229, Some(48000), I16)
        );
        assert_eq!(row(plan(f44, 0x20).unwrap()), (Ssc44, 0x20, 229, None, I16));
    }

    #[test]
    fn input_depths() {
        let f32v = fmt(48000, 32, 32);
        assert_eq!(plan(f32v, 0x3E).unwrap().input, InputKind::S32);
        assert_eq!(
            plan(fmt(96000, 32, 32), 0x3E).unwrap().input,
            InputKind::S32
        );
        assert_eq!(plan(fmt(96000, 16, 16), 0x3E).unwrap().input, I24);
        assert_eq!(plan(f32v, 0x30).unwrap().input, I16);
        // Resampled input is quantised to the mode's depth whatever came in.
        assert_eq!(plan(fmt(96000, 32, 32), 0x3C).unwrap().input, I24);
        // Mono is planned like stereo.
        let mono = PcmFormat { channels: 1, ..F48 };
        assert_eq!(plan(mono, 0x3E).unwrap(), plan(F48, 0x3E).unwrap());
    }

    #[test]
    fn invalid_formats_are_refused() {
        for f in [
            PcmFormat::default(),
            fmt(48000, 24, 24),
            fmt(0, 32, 24),
            PcmFormat { channels: 6, ..F48 },
            fmt(48000, 16, 24),
        ] {
            assert_eq!(plan(f, 0x3E), Err(PlanError::BadFormat(f)));
        }
        assert!(
            PlanError::BadFormat(fmt(48000, 24, 24))
                .to_string()
                .contains("24 valid bits in 24")
        );
    }

    #[test]
    fn settings_tune_uhq_and_hifi() {
        let s = Settings {
            uhq_config: 0x0E,
            hifi_bitrate: Bitrate::K229,
            uhq_bitrate: Bitrate::K291,
            ..Settings::default()
        };
        let p = select_plan(F96, SscFlags(0x3E), &s).unwrap();
        assert_eq!((p.config, p.bitrate), (0x0E, Bitrate::K291));
        let p = select_plan(F48, SscFlags(0x3E), &s).unwrap();
        assert_eq!((p.config, p.bitrate), (0x0C, Bitrate::K229));
        // 0x0E on a sink without HiFi: only the offered bits go out.
        let p = select_plan(F96, SscFlags(0x3A), &s).unwrap();
        assert_eq!(p.config, 0x0A);
    }

    /// The DESIGN.md table as data: the first row whose condition holds
    /// gives the mode. `need` bits must all be set, `forbid` bits clear.
    struct TableRow {
        rate_96k_only: bool,
        need: u8,
        forbid: u8,
        mode: SscMode,
        config: u8,
        kbps: u32,
    }

    const TABLE: [TableRow; 4] = [
        TableRow {
            rate_96k_only: true,
            need: 0x02,
            forbid: 0x01,
            mode: Uhq,
            config: 0x0A,
            kbps: 584,
        },
        TableRow {
            rate_96k_only: false,
            need: 0x04,
            forbid: 0,
            mode: Hifi,
            config: 0x0C,
            kbps: 229,
        },
        TableRow {
            rate_96k_only: false,
            need: 0x10,
            forbid: 0,
            mode: Ssc48,
            config: 0x18,
            kbps: 229,
        },
        TableRow {
            rate_96k_only: false,
            need: 0x20,
            forbid: 0,
            mode: Ssc44,
            config: 0x28,
            kbps: 229,
        },
    ];

    #[test]
    fn every_caps_octet_follows_the_table() {
        let formats = [
            F96,
            F48,
            F48_16,
            fmt(96000, 16, 16),
            fmt(48000, 32, 32),
            PcmFormat { channels: 1, ..F96 },
        ];
        let mut modes_seen = std::collections::HashSet::new();
        for caps in 0..=255u8 {
            for f in formats {
                let want = TABLE.iter().find(|r| {
                    (!r.rate_96k_only || f.sample_rate == 96000)
                        && caps & r.need == r.need
                        && caps & r.forbid == 0
                });
                let got = plan(f, caps);
                let Some(r) = want else {
                    assert_eq!(got, Err(PlanError::NoMode(caps)), "caps {caps:#04x} {f:?}");
                    continue;
                };
                let p = got.unwrap_or_else(|e| panic!("caps {caps:#04x} {f:?}: {e}"));
                let ctx = format!("caps {caps:#04x} {f:?}");
                assert_eq!(p.mode, r.mode, "{ctx}");
                // The table's octet, minus 0x08 where the sink lacks it.
                let config = if caps & 0x08 != 0 {
                    r.config
                } else {
                    r.config & !0x08
                };
                assert_eq!(p.config, config, "{ctx}");
                assert_eq!(p.config & !caps, 0, "{ctx}: only offered bits");
                assert_eq!(p.bitrate.bps() / 1000, r.kbps, "{ctx}");
                assert_eq!(p.frames_per_packet, 1, "{ctx}");
                let rate = r.mode.sample_rate();
                assert_eq!(
                    p.resample_to,
                    (f.sample_rate != rate).then_some(rate),
                    "{ctx}"
                );
                let hires = matches!(r.mode, Uhq | Hifi);
                let input = match (hires, p.resample_to.is_some(), f.valid_bits) {
                    (false, _, _) => I16,
                    (true, false, 32) => InputKind::S32,
                    (true, _, _) => I24,
                };
                assert_eq!(p.input, input, "{ctx}");
                assert!(p.packet_bytes() <= 1005, "{ctx}");
                modes_seen.insert(p.mode);
            }
        }
        assert_eq!(modes_seen.len(), 4);
    }

    #[test]
    fn bitrate_steps_down_to_fit_the_mtu() {
        let mut p = plan(F96, 0x3E).unwrap();
        assert_eq!(p.fit_mtu(1005), Ok(None));
        assert_eq!(p.fit_mtu(672), Ok(None), "672-byte packets fit exactly");
        assert_eq!(p.fit_mtu(671), Ok(Some(Bitrate::K584)));
        assert_eq!(p.bitrate, Bitrate::K291, "the next rate the buds decode");
        assert!(p.packet_bytes() <= 671);
        let mut p = plan(F48, 0x3E).unwrap();
        assert_eq!(p.fit_mtu(300), Ok(Some(Bitrate::K229)));
        assert_eq!(p.bitrate, Bitrate::K96);
        let mut p = plan(F48, 0x3E).unwrap();
        let e = p.fit_mtu(48).unwrap_err();
        assert!(e.contains("exceed the media MTU limit 48"), "{e}");
        assert_eq!(p.bitrate, Bitrate::K229, "unchanged when nothing fits");
    }

    #[test]
    fn same_stream_ignores_the_input_conversion() {
        let a = plan(F48, 0x3E).unwrap();
        let b = plan(F48_16, 0x3E).unwrap();
        assert!(a.same_stream(&b), "HiFi either way");
        assert!(!a.same_stream(&plan(F96, 0x3E).unwrap()));
        // 96 kHz resampled to HiFi is the same stream as 48 kHz HiFi.
        assert!(a.same_stream(&plan(F96, 0x3C).unwrap()));
        assert!(
            a.describe()
                .contains("HiFi 48 kHz/24: config 0x0c, 229 kbps")
        );
        assert!(
            plan(F96, 0x3C)
                .unwrap()
                .describe()
                .contains("resampled to 48000 Hz")
        );
    }
}
