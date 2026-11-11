//! Service capabilities (AVDTP 1.3 section 8.21): a sequence of
//! `category(1) | LOSC(1) | information elements(LOSC)`.

use alloc::vec::Vec;

use crate::Error;
use crate::codec::CodecInfo;

pub const CAT_MEDIA_TRANSPORT: u8 = 0x01;
pub const CAT_REPORTING: u8 = 0x02;
pub const CAT_RECOVERY: u8 = 0x03;
pub const CAT_CONTENT_PROTECTION: u8 = 0x04;
pub const CAT_HEADER_COMPRESSION: u8 = 0x05;
pub const CAT_MULTIPLEXING: u8 = 0x06;
pub const CAT_MEDIA_CODEC: u8 = 0x07;
pub const CAT_DELAY_REPORTING: u8 = 0x08;

/// Content protection types (Assigned Numbers), little-endian on the wire.
pub const CP_TYPE_DTCP: u16 = 0x0001;
pub const CP_TYPE_SCMS_T: u16 = 0x0002;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Capability {
    MediaTransport,
    Reporting,
    Recovery {
        recovery_type: u8,
        mrws: u8,
        mnmp: u8,
    },
    ContentProtection {
        cp_type: u16,
        value: Vec<u8>,
    },
    /// Bits of the single octet: back channel (0x80), media (0x40), recovery (0x20).
    HeaderCompression {
        back_channel: bool,
        media: bool,
        recovery: bool,
    },
    /// Raw multiplexing elements (fragmentation flag + TSID/TCID entries).
    Multiplexing(Vec<u8>),
    MediaCodec {
        media_type: u8,
        codec: CodecInfo,
    },
    DelayReporting,
    /// Unknown / reserved category, kept raw.
    Unknown {
        category: u8,
        data: Vec<u8>,
    },
}

impl Capability {
    pub fn category(&self) -> u8 {
        match self {
            Self::MediaTransport => CAT_MEDIA_TRANSPORT,
            Self::Reporting => CAT_REPORTING,
            Self::Recovery { .. } => CAT_RECOVERY,
            Self::ContentProtection { .. } => CAT_CONTENT_PROTECTION,
            Self::HeaderCompression { .. } => CAT_HEADER_COMPRESSION,
            Self::Multiplexing(_) => CAT_MULTIPLEXING,
            Self::MediaCodec { .. } => CAT_MEDIA_CODEC,
            Self::DelayReporting => CAT_DELAY_REPORTING,
            Self::Unknown { category, .. } => *category,
        }
    }

    pub fn category_name(category: u8) -> &'static str {
        match category {
            CAT_MEDIA_TRANSPORT => "Media Transport",
            CAT_REPORTING => "Reporting",
            CAT_RECOVERY => "Recovery",
            CAT_CONTENT_PROTECTION => "Content Protection",
            CAT_HEADER_COMPRESSION => "Header Compression",
            CAT_MULTIPLEXING => "Multiplexing",
            CAT_MEDIA_CODEC => "Media Codec",
            CAT_DELAY_REPORTING => "Delay Reporting",
            _ => "Unknown",
        }
    }

    fn decode_one(category: u8, d: &[u8]) -> Result<Self, Error> {
        let bad = Error::BadCapabilityLength { category };
        Ok(match category {
            CAT_MEDIA_TRANSPORT | CAT_REPORTING | CAT_DELAY_REPORTING => {
                if !d.is_empty() {
                    return Err(bad);
                }
                match category {
                    CAT_MEDIA_TRANSPORT => Self::MediaTransport,
                    CAT_REPORTING => Self::Reporting,
                    _ => Self::DelayReporting,
                }
            }
            CAT_RECOVERY => match d {
                [t, w, n] => Self::Recovery {
                    recovery_type: *t,
                    mrws: *w,
                    mnmp: *n,
                },
                _ => return Err(bad),
            },
            CAT_CONTENT_PROTECTION => {
                if d.len() < 2 {
                    return Err(bad);
                }
                Self::ContentProtection {
                    cp_type: u16::from_le_bytes([d[0], d[1]]),
                    value: d[2..].to_vec(),
                }
            }
            CAT_HEADER_COMPRESSION => match d {
                [b] => Self::HeaderCompression {
                    back_channel: b & 0x80 != 0,
                    media: b & 0x40 != 0,
                    recovery: b & 0x20 != 0,
                },
                _ => return Err(bad),
            },
            CAT_MULTIPLEXING => Self::Multiplexing(d.to_vec()),
            CAT_MEDIA_CODEC => {
                if d.len() < 2 {
                    return Err(bad);
                }
                Self::MediaCodec {
                    media_type: d[0] >> 4,
                    codec: CodecInfo::decode(d[1], &d[2..]),
                }
            }
            _ => Self::Unknown {
                category,
                data: d.to_vec(),
            },
        })
    }

    /// Append `category | LOSC | elements`. Returns `Err(BadCapabilityLength)`
    /// if the elements exceed 255 octets.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let mut e = Vec::new();
        match self {
            Self::MediaTransport | Self::Reporting | Self::DelayReporting => {}
            Self::Recovery {
                recovery_type,
                mrws,
                mnmp,
            } => e.extend_from_slice(&[*recovery_type, *mrws, *mnmp]),
            Self::ContentProtection { cp_type, value } => {
                e.extend_from_slice(&cp_type.to_le_bytes());
                e.extend_from_slice(value);
            }
            Self::HeaderCompression {
                back_channel,
                media,
                recovery,
            } => e.push(
                (u8::from(*back_channel) << 7)
                    | (u8::from(*media) << 6)
                    | (u8::from(*recovery) << 5),
            ),
            Self::Multiplexing(d) => e.extend_from_slice(d),
            Self::MediaCodec { media_type, codec } => {
                e.push(media_type << 4);
                e.push(codec.codec_type());
                e.extend_from_slice(&codec.encode());
            }
            Self::Unknown { data, .. } => e.extend_from_slice(data),
        }
        if e.len() > 255 {
            return Err(Error::BadCapabilityLength {
                category: self.category(),
            });
        }
        out.push(self.category());
        out.push(e.len() as u8);
        out.extend_from_slice(&e);
        Ok(())
    }
}

/// Parse a full capability list.
pub fn decode(mut d: &[u8]) -> Result<Vec<Capability>, Error> {
    let mut v = Vec::new();
    while !d.is_empty() {
        let [cat, losc, rest @ ..] = d else {
            return Err(Error::Truncated);
        };
        let losc = *losc as usize;
        if rest.len() < losc {
            return Err(Error::BadCapabilityLength { category: *cat });
        }
        v.push(Capability::decode_one(*cat, &rest[..losc])?);
        d = &rest[losc..];
    }
    Ok(v)
}

/// Encode a capability list.
pub fn encode(caps: &[Capability]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    for c in caps {
        c.encode_into(&mut out)?;
    }
    Ok(out)
}

/// SET_CONFIGURATION capability list for Samsung SSC: Media Transport, Media
/// Codec (vendor 0x75 / codec 0x0103 with the one-octet `config`), and Delay
/// Reporting when `delay_reporting` (only if the sink advertised it).
pub fn ssc_configuration(config: u8, delay_reporting: bool) -> Vec<Capability> {
    let mut v = alloc::vec![
        Capability::MediaTransport,
        Capability::MediaCodec {
            media_type: crate::codec::MEDIA_TYPE_AUDIO,
            codec: crate::codec::Ssc::configuration(config),
        },
    ];
    if delay_reporting {
        v.push(Capability::DelayReporting);
    }
    v
}

#[cfg(test)]
pub(crate) mod tests_support {
    /// A realistic GET_ALL_CAPABILITIES accept payload: transport, SCMS-T CP,
    /// SSC vendor codec (flags 0x16), delay reporting.
    pub const SSC_SEP_CAPS: &[u8] = &[
        0x01, 0x00, // media transport
        0x04, 0x02, 0x02, 0x00, // content protection SCMS-T
        0x07, 0x09, 0x00, 0xFF, 0x75, 0x00, 0x00, 0x00, 0x03, 0x01, 0x16, // vendor SSC
        0x08, 0x00, // delay reporting
    ];

    pub fn ssc_caps() -> alloc::vec::Vec<u8> {
        SSC_SEP_CAPS.to_vec()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::codec::{CodecInfo, SAMSUNG_VENDOR_ID, SSC_CODEC_ID, Sbc, Vendor};
    use alloc::vec;

    use super::tests_support::SSC_SEP_CAPS;

    #[test]
    fn decode_ssc_sep() {
        let c = decode(SSC_SEP_CAPS).unwrap();
        assert_eq!(c.len(), 4);
        assert_eq!(c[0], Capability::MediaTransport);
        assert_eq!(
            c[1],
            Capability::ContentProtection {
                cp_type: CP_TYPE_SCMS_T,
                value: vec![]
            }
        );
        let Capability::MediaCodec {
            media_type: 0,
            codec: CodecInfo::Vendor(v),
        } = &c[2]
        else {
            panic!("{:?}", c[2])
        };
        assert_eq!((v.vendor_id, v.codec_id), (SAMSUNG_VENDOR_ID, SSC_CODEC_ID));
        assert_eq!(v.ssc().unwrap().flags.0, 0x16);
        assert_eq!(c[3], Capability::DelayReporting);
        assert_eq!(encode(&c).unwrap(), SSC_SEP_CAPS);
    }

    #[test]
    fn all_categories_roundtrip() {
        let caps = vec![
            Capability::MediaTransport,
            Capability::Reporting,
            Capability::Recovery {
                recovery_type: 1,
                mrws: 0x18,
                mnmp: 0x20,
            },
            Capability::ContentProtection {
                cp_type: CP_TYPE_DTCP,
                value: vec![9, 9],
            },
            Capability::HeaderCompression {
                back_channel: true,
                media: false,
                recovery: true,
            },
            Capability::Multiplexing(vec![0x80, 0x08, 0x10]),
            Capability::MediaCodec {
                media_type: 0,
                codec: CodecInfo::Sbc(Sbc::decode(&[0x21, 0x15, 2, 53]).unwrap()),
            },
            Capability::MediaCodec {
                media_type: 0,
                codec: CodecInfo::Vendor(Vendor {
                    vendor_id: 0x4F,
                    codec_id: 0x01,
                    data: vec![1, 2],
                }),
            },
            Capability::DelayReporting,
            Capability::Unknown {
                category: 0x33,
                data: vec![1],
            },
        ];
        let b = encode(&caps).unwrap();
        assert_eq!(decode(&b).unwrap(), caps);
        assert_eq!(&b[..2], &[0x01, 0x00]);
        assert_eq!(&b[4..9], &[0x03, 0x03, 0x01, 0x18, 0x20]);
        assert_eq!(&b[15..18], &[0x05, 0x01, 0xA0]);
    }

    #[test]
    fn malformed_lists() {
        assert_eq!(decode(&[0x01]), Err(Error::Truncated));
        assert_eq!(
            decode(&[0x07, 0x05, 0x00, 0x00]),
            Err(Error::BadCapabilityLength { category: 7 })
        );
        assert_eq!(
            decode(&[0x01, 0x01, 0x00]),
            Err(Error::BadCapabilityLength { category: 1 })
        );
        assert_eq!(
            decode(&[0x03, 0x02, 0x01, 0x02]),
            Err(Error::BadCapabilityLength { category: 3 })
        );
        assert_eq!(
            decode(&[0x04, 0x01, 0x02]),
            Err(Error::BadCapabilityLength { category: 4 })
        );
        assert_eq!(
            decode(&[0x05, 0x00]),
            Err(Error::BadCapabilityLength { category: 5 })
        );
        assert_eq!(
            decode(&[0x07, 0x01, 0x00]),
            Err(Error::BadCapabilityLength { category: 7 })
        );
        assert_eq!(
            decode(&[0x08, 0x01, 0x00]),
            Err(Error::BadCapabilityLength { category: 8 })
        );
        assert_eq!(
            decode(&[0x01, 0xFF]),
            Err(Error::BadCapabilityLength { category: 1 })
        );
        assert_eq!(decode(&[]).unwrap(), vec![]);
        // Bad SBC length keeps the bytes.
        let c = decode(&[0x07, 0x03, 0x00, 0x00, 0x21]).unwrap();
        assert!(matches!(
            &c[0],
            Capability::MediaCodec {
                codec: CodecInfo::Malformed { .. },
                ..
            }
        ));
    }

    #[test]
    fn ssc_configuration_bytes() {
        assert_eq!(
            encode(&ssc_configuration(0x0C, true)).unwrap(),
            [
                0x01, 0x00, 0x07, 0x09, 0x00, 0xFF, 0x75, 0x00, 0x00, 0x00, 0x03, 0x01, 0x0C, 0x08,
                0x00
            ]
        );
        let c = ssc_configuration(0x0A, false);
        assert_eq!(c.len(), 2);
        assert_eq!(
            encode(&c).unwrap(),
            [
                0x01, 0x00, 0x07, 0x09, 0x00, 0xFF, 0x75, 0x00, 0x00, 0x00, 0x03, 0x01, 0x0A
            ]
        );
        assert_eq!(decode(&encode(&c).unwrap()).unwrap(), c);
    }

    #[test]
    fn oversized_encode_rejected() {
        let c = Capability::Multiplexing(vec![0; 256]);
        assert_eq!(
            encode(&[c]),
            Err(Error::BadCapabilityLength { category: 6 })
        );
    }

    #[test]
    fn never_panics() {
        let mut x: u32 = 0xDEAD_BEEF;
        for _ in 0..20000 {
            let mut v = std::vec::Vec::new();
            for _ in 0..(x % 24) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                v.push((x % 12) as u8);
            }
            x = x.wrapping_add(1);
            if let Ok(c) = decode(&v) {
                // Whatever decodes must survive encode -> decode (RFA bits
                // are not preserved, so compare structurally).
                assert_eq!(decode(&encode(&c).unwrap()).unwrap(), c);
            }
        }
    }
}
