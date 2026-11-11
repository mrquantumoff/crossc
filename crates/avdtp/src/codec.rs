//! Media codec information elements (the payload of the Media Codec service
//! capability after the media-type and codec-type octets), per A2DP 1.3/1.4.

use alloc::vec::Vec;

/// Media type (upper nibble of the first Media Codec octet).
pub const MEDIA_TYPE_AUDIO: u8 = 0x00;

/// A2DP codec types.
pub const CODEC_SBC: u8 = 0x00;
pub const CODEC_MPEG12: u8 = 0x01;
pub const CODEC_AAC: u8 = 0x02;
pub const CODEC_USAC: u8 = 0x03;
pub const CODEC_ATRAC: u8 = 0x04;
pub const CODEC_VENDOR: u8 = 0xFF;

/// Samsung Electronics company id and the Scalable Codec id.
pub const SAMSUNG_VENDOR_ID: u32 = 0x0000_0075;
pub const SSC_CODEC_ID: u16 = 0x0103;

/// Decoded codec information element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecInfo {
    Sbc(Sbc),
    Aac(Aac),
    Vendor(Vendor),
    /// Any other codec type, raw.
    Other {
        codec_type: u8,
        data: Vec<u8>,
    },
    /// A known codec type whose element has the wrong length; kept raw so a
    /// probe can still show it.
    Malformed {
        codec_type: u8,
        data: Vec<u8>,
    },
}

impl CodecInfo {
    pub fn decode(codec_type: u8, data: &[u8]) -> Self {
        let r = match codec_type {
            CODEC_SBC => Sbc::decode(data).map(Self::Sbc),
            CODEC_AAC => Aac::decode(data).map(Self::Aac),
            CODEC_VENDOR => Vendor::decode(data).map(Self::Vendor),
            _ => {
                return Self::Other {
                    codec_type,
                    data: data.to_vec(),
                };
            }
        };
        r.unwrap_or_else(|| Self::Malformed {
            codec_type,
            data: data.to_vec(),
        })
    }

    pub fn codec_type(&self) -> u8 {
        match self {
            Self::Sbc(_) => CODEC_SBC,
            Self::Aac(_) => CODEC_AAC,
            Self::Vendor(_) => CODEC_VENDOR,
            Self::Other { codec_type, .. } | Self::Malformed { codec_type, .. } => *codec_type,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Sbc(s) => s.encode().to_vec(),
            Self::Aac(a) => a.encode().to_vec(),
            Self::Vendor(v) => v.encode(),
            Self::Other { data, .. } | Self::Malformed { data, .. } => data.clone(),
        }
    }
}

/// SBC codec element (4 octets). Fields are the raw capability bitmasks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sbc {
    /// 0x80 16k, 0x40 32k, 0x20 44.1k, 0x10 48k
    pub sampling_frequency: u8,
    /// 0x08 mono, 0x04 dual, 0x02 stereo, 0x01 joint stereo
    pub channel_mode: u8,
    /// 0x80 4, 0x40 8, 0x20 12, 0x10 16
    pub block_length: u8,
    /// 0x08 4, 0x04 8
    pub subbands: u8,
    /// 0x02 SNR, 0x01 loudness
    pub allocation_method: u8,
    pub min_bitpool: u8,
    pub max_bitpool: u8,
}

impl Sbc {
    pub fn decode(d: &[u8]) -> Option<Self> {
        if d.len() != 4 {
            return None;
        }
        Some(Self {
            sampling_frequency: d[0] & 0xF0,
            channel_mode: d[0] & 0x0F,
            block_length: d[1] & 0xF0,
            subbands: d[1] & 0x0C,
            allocation_method: d[1] & 0x03,
            min_bitpool: d[2],
            max_bitpool: d[3],
        })
    }
    pub fn encode(&self) -> [u8; 4] {
        [
            (self.sampling_frequency & 0xF0) | (self.channel_mode & 0x0F),
            (self.block_length & 0xF0) | (self.subbands & 0x0C) | (self.allocation_method & 0x03),
            self.min_bitpool,
            self.max_bitpool,
        ]
    }
    pub fn sample_rates(&self) -> impl Iterator<Item = u32> + '_ {
        [(0x80, 16000), (0x40, 32000), (0x20, 44100), (0x10, 48000)]
            .into_iter()
            .filter(|(b, _)| self.sampling_frequency & b != 0)
            .map(|(_, r)| r)
    }
}

/// MPEG-2/4 AAC codec element (6 octets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Aac {
    /// 0x80 MPEG-2 LC, 0x40 MPEG-4 LC, 0x20 MPEG-4 LTP, 0x10 MPEG-4 scalable,
    /// 0x08 MPEG-4 HE-AAC, 0x04 HE-AACv2, 0x02 ELDv2, 0x01 (RFA/D-ELD)
    pub object_type: u8,
    /// 12-bit mask: bit 11 = 8000 ... bit 0 = 96000 (see [`Aac::sample_rates`]).
    pub sampling_frequency: u16,
    /// 0x08 = 1 channel, 0x04 = 2 channels (A2DP 1.4 adds 0x02 = 5.1, 0x01 = 7.1).
    pub channels: u8,
    pub vbr: bool,
    /// 23-bit bit rate in bit/s (0 = unknown).
    pub bit_rate: u32,
}

const AAC_RATES: [u32; 12] = [
    8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000,
];

impl Aac {
    pub fn decode(d: &[u8]) -> Option<Self> {
        if d.len() != 6 {
            return None;
        }
        Some(Self {
            object_type: d[0],
            sampling_frequency: (u16::from(d[1]) << 4) | u16::from(d[2] >> 4),
            channels: d[2] & 0x0F,
            vbr: d[3] & 0x80 != 0,
            bit_rate: (u32::from(d[3] & 0x7F) << 16) | (u32::from(d[4]) << 8) | u32::from(d[5]),
        })
    }
    pub fn encode(&self) -> [u8; 6] {
        [
            self.object_type,
            (self.sampling_frequency >> 4) as u8,
            (((self.sampling_frequency & 0x0F) as u8) << 4) | (self.channels & 0x0F),
            (u8::from(self.vbr) << 7) | ((self.bit_rate >> 16) as u8 & 0x7F),
            (self.bit_rate >> 8) as u8,
            self.bit_rate as u8,
        ]
    }
    pub fn sample_rates(&self) -> impl Iterator<Item = u32> + '_ {
        AAC_RATES
            .iter()
            .enumerate()
            .filter(|(i, _)| self.sampling_frequency & (0x800 >> i) != 0)
            .map(|(_, r)| *r)
    }
}

/// Vendor-specific codec element: vendor id (4 octets LE), codec id (2 octets LE), value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vendor {
    pub vendor_id: u32,
    pub codec_id: u16,
    pub data: Vec<u8>,
}

impl Vendor {
    pub fn decode(d: &[u8]) -> Option<Self> {
        if d.len() < 6 {
            return None;
        }
        Some(Self {
            vendor_id: u32::from_le_bytes([d[0], d[1], d[2], d[3]]),
            codec_id: u16::from_le_bytes([d[4], d[5]]),
            data: d[6..].to_vec(),
        })
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(6 + self.data.len());
        v.extend_from_slice(&self.vendor_id.to_le_bytes());
        v.extend_from_slice(&self.codec_id.to_le_bytes());
        v.extend_from_slice(&self.data);
        v
    }
    /// Typed view if this is Samsung SSC.
    pub fn ssc(&self) -> Option<Ssc> {
        if self.vendor_id == SAMSUNG_VENDOR_ID && self.codec_id == SSC_CODEC_ID {
            Ssc::decode(&self.data)
        } else {
            None
        }
    }
}

/// Samsung Scalable Codec capability: one flags octet (any further octets are
/// kept in `extra` - the format beyond the first octet is not known).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ssc {
    pub flags: SscFlags,
    pub extra: Vec<u8>,
}

impl Ssc {
    pub fn decode(d: &[u8]) -> Option<Self> {
        let (&f, rest) = d.split_first()?;
        Some(Self {
            flags: SscFlags(f),
            extra: rest.to_vec(),
        })
    }
    /// Build a vendor element for SET_CONFIGURATION with the given flags.
    pub fn to_vendor(&self) -> Vendor {
        let mut data = alloc::vec![self.flags.0];
        data.extend_from_slice(&self.extra);
        Vendor {
            vendor_id: SAMSUNG_VENDOR_ID,
            codec_id: SSC_CODEC_ID,
            data,
        }
    }
}

/// The SSC flags octet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SscFlags(pub u8);

impl SscFlags {
    pub const LEGACY_96K: u8 = 0x80;
    pub const SR_32K: u8 = 0x40;
    pub const SR_44K1: u8 = 0x20;
    pub const SR_48K: u8 = 0x10;
    pub const NO_BITRATE_LIMIT_24BIT: u8 = 0x08;
    pub const HIFI_48K_24: u8 = 0x04;
    pub const UHQ_V2_96K_24: u8 = 0x02;
    pub const SINK_DISABLES_UHQ_V2: u8 = 0x01;

    pub const ALL: [(u8, &'static str); 8] = [
        (Self::LEGACY_96K, "96 kHz (legacy)"),
        (Self::SR_32K, "32 kHz"),
        (Self::SR_44K1, "44.1 kHz"),
        (Self::SR_48K, "48 kHz"),
        (Self::NO_BITRATE_LIMIT_24BIT, "no bitrate limit / 24-bit"),
        (Self::HIFI_48K_24, "HiFi 48 kHz/24-bit"),
        (Self::UHQ_V2_96K_24, "UHQ v2 96 kHz/24-bit"),
        (Self::SINK_DISABLES_UHQ_V2, "sink disables UHQ v2"),
    ];

    pub fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
    /// Names of all set bits, MSB first.
    pub fn names(self) -> impl Iterator<Item = &'static str> {
        Self::ALL
            .into_iter()
            .filter(move |(b, _)| self.0 & b != 0)
            .map(|(_, n)| n)
    }
    /// 96 kHz/24-bit (UHQ v2) usable: advertised and not disabled by the sink.
    pub fn uhq_v2_usable(self) -> bool {
        self.has(Self::UHQ_V2_96K_24) && !self.has(Self::SINK_DISABLES_UHQ_V2)
    }
    /// 48 kHz/24-bit HiFi advertised.
    pub fn hifi_48k_24(self) -> bool {
        self.has(Self::HIFI_48K_24)
    }

    /// Configuration octet for Seamless HiFi 48 kHz/24-bit (`0x04 | 0x08`).
    pub const CONFIG_HIFI: u8 = Self::HIFI_48K_24 | Self::NO_BITRATE_LIMIT_24BIT;
    /// Configuration octet for UHQ v2 96 kHz/24-bit as Samsung's phone stack
    /// sends it (`0x02 | 0x08`, community disassembly).
    pub const CONFIG_UHQ_V2: u8 = Self::UHQ_V2_96K_24 | Self::NO_BITRATE_LIMIT_24BIT;
    /// Alternative UHQ v2 octet (`0x02 | 0x04 | 0x08`) another implementation
    /// uses; reported to give clean audio on Buds3 Pro. Unverified which one
    /// a given sink expects.
    pub const CONFIG_UHQ_V2_ALT: u8 =
        Self::UHQ_V2_96K_24 | Self::HIFI_48K_24 | Self::NO_BITRATE_LIMIT_24BIT;
    /// Configuration octet for plain SSC at 48 kHz (`0x10 | 0x08`).
    pub const CONFIG_SSC_48K: u8 = Self::SR_48K | Self::NO_BITRATE_LIMIT_24BIT;
    /// Configuration octet for plain SSC at 44.1 kHz (`0x20 | 0x08`).
    pub const CONFIG_SSC_44K1: u8 = Self::SR_44K1 | Self::NO_BITRATE_LIMIT_24BIT;

    /// Bits of `config` that this capability octet does not advertise
    /// (0 = every configured bit is offered by the sink). Bit 0x01 ("sink
    /// disables UHQ v2") is a sink-side flag and never valid in a
    /// configuration, so it is always reported as missing when set.
    pub fn missing_for(self, config: SscFlags) -> u8 {
        (config.0 & !self.0) | (config.0 & Self::SINK_DISABLES_UHQ_V2)
    }
}

impl Ssc {
    /// The codec element for SET_CONFIGURATION: exactly one configuration
    /// octet (the SSC configuration is a single byte, no extra octets).
    pub fn configuration(config: u8) -> CodecInfo {
        CodecInfo::Vendor(
            Ssc {
                flags: SscFlags(config),
                extra: Vec::new(),
            }
            .to_vendor(),
        )
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::vec;
    use std::vec::Vec as StdVec;

    #[test]
    fn sbc_roundtrip() {
        let d = [0xFF, 0xFF, 0x02, 0x35];
        let s = Sbc::decode(&d).unwrap();
        assert_eq!(s.sampling_frequency, 0xF0);
        assert_eq!(s.channel_mode, 0x0F);
        assert_eq!(s.min_bitpool, 2);
        assert_eq!(s.max_bitpool, 53);
        assert_eq!(
            s.sample_rates().collect::<StdVec<_>>(),
            vec![16000, 32000, 44100, 48000]
        );
        assert_eq!(s.encode(), d);
        assert!(Sbc::decode(&[0; 3]).is_none());
        assert!(Sbc::decode(&[0; 5]).is_none());
    }

    #[test]
    fn aac_decode() {
        // MPEG-2 LC | MPEG-4 LC, 44.1k+48k, 1+2ch, VBR, 320 kbit/s
        let d = [0xC0, 0x01, 0x8C, 0x84, 0xE2, 0x00];
        let a = Aac::decode(&d).unwrap();
        assert_eq!(a.object_type, 0xC0);
        assert_eq!(a.sampling_frequency, 0x018);
        assert_eq!(a.sample_rates().collect::<StdVec<_>>(), vec![44100, 48000]);
        assert_eq!(a.channels, 0x0C);
        assert!(a.vbr);
        assert_eq!(a.bit_rate, 320_000);
        assert_eq!(a.encode(), d);
        assert!(Aac::decode(&[0; 5]).is_none());
    }

    #[test]
    fn vendor_is_little_endian() {
        let d = [0x75, 0x00, 0x00, 0x00, 0x03, 0x01, 0x16];
        let v = Vendor::decode(&d).unwrap();
        assert_eq!(v.vendor_id, 0x75);
        assert_eq!(v.codec_id, 0x0103);
        assert_eq!(v.encode(), d.to_vec());
        let ssc = v.ssc().unwrap();
        assert_eq!(ssc.flags.0, 0x16);
        assert!(ssc.flags.uhq_v2_usable());
        assert!(ssc.flags.hifi_48k_24());
        assert_eq!(
            ssc.flags.names().collect::<StdVec<_>>(),
            vec!["48 kHz", "HiFi 48 kHz/24-bit", "UHQ v2 96 kHz/24-bit"]
        );
        assert_eq!(ssc.to_vendor(), v);
    }

    #[test]
    fn ssc_flags_semantics() {
        let f = SscFlags(0x03);
        assert!(!f.uhq_v2_usable()); // sink disables it
        assert!(!SscFlags(0x00).hifi_48k_24());
        assert_eq!(SscFlags(0xFF).names().count(), 8);
    }

    #[test]
    fn ssc_configuration_octets() {
        assert_eq!(SscFlags::CONFIG_HIFI, 0x0C);
        assert_eq!(SscFlags::CONFIG_UHQ_V2, 0x0A);
        assert_eq!(SscFlags::CONFIG_UHQ_V2_ALT, 0x0E);
        assert_eq!(SscFlags::CONFIG_SSC_48K, 0x18);
        assert_eq!(SscFlags::CONFIG_SSC_44K1, 0x28);
        let c = Ssc::configuration(0x0C);
        assert_eq!(c.codec_type(), CODEC_VENDOR);
        assert_eq!(c.encode(), vec![0x75, 0, 0, 0, 0x03, 0x01, 0x0C]);
        let CodecInfo::Vendor(v) = c else {
            panic!("vendor")
        };
        assert_eq!(v.ssc().unwrap().flags, SscFlags(0x0C));
    }

    #[test]
    fn ssc_config_vs_caps() {
        let buds = SscFlags(0x3E); // Galaxy Buds 4 Pro
        for c in [0x0C, 0x0A, 0x0E, 0x18, 0x28] {
            assert_eq!(buds.missing_for(SscFlags(c)), 0, "{c:#x}");
        }
        let old = SscFlags(0x16);
        assert_eq!(old.missing_for(SscFlags(0x0C)), 0x08);
        assert_eq!(old.missing_for(SscFlags(0x28)), 0x28);
        assert_eq!(SscFlags(0xFF).missing_for(SscFlags(0x0B)), 0x01);
    }

    #[test]
    fn not_ssc() {
        // Samsung but other codec id; other vendor with SSC id.
        assert!(
            Vendor::decode(&[0x75, 0, 0, 0, 0x02, 0x01, 0x10])
                .unwrap()
                .ssc()
                .is_none()
        );
        assert!(
            Vendor::decode(&[0x2D, 0x01, 0, 0, 0x03, 0x01, 0x10])
                .unwrap()
                .ssc()
                .is_none()
        );
        // SSC with no flags byte
        assert!(
            Vendor::decode(&[0x75, 0, 0, 0, 0x03, 0x01])
                .unwrap()
                .ssc()
                .is_none()
        );
    }

    #[test]
    fn codec_info_dispatch() {
        assert!(matches!(
            CodecInfo::decode(CODEC_SBC, &[0x21, 0x15, 2, 53]),
            CodecInfo::Sbc(_)
        ));
        assert!(matches!(
            CodecInfo::decode(CODEC_SBC, &[0x21]),
            CodecInfo::Malformed { codec_type: 0, .. }
        ));
        assert!(matches!(
            CodecInfo::decode(CODEC_VENDOR, &[1, 2]),
            CodecInfo::Malformed {
                codec_type: 0xFF,
                ..
            }
        ));
        assert!(matches!(
            CodecInfo::decode(CODEC_USAC, &[1, 2, 3]),
            CodecInfo::Other { codec_type: 3, .. }
        ));
        for t in [CODEC_SBC, CODEC_AAC, CODEC_VENDOR, 0x04] {
            for n in 0..10 {
                let d: StdVec<u8> = (0..n).collect();
                let c = CodecInfo::decode(t, &d);
                assert_eq!(c.codec_type(), t);
                assert_eq!(c.encode(), d);
            }
        }
    }
}
