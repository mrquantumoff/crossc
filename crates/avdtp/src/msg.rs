//! Typed AVDTP signaling messages: command builders and response parsers.
//!
//! Payload layouts (AVDTP 1.3 section 8.6 - 8.19):
//!
//! | signal | command | accept | reject |
//! |---|---|---|---|
//! | DISCOVER | - | n x SEP info (2 octets) | error |
//! | GET_(ALL_)CAPABILITIES | ACP SEID | capabilities | error |
//! | SET_CONFIGURATION | ACP SEID, INT SEID, caps | - | category, error |
//! | GET_CONFIGURATION | ACP SEID | capabilities | error |
//! | RECONFIGURE | ACP SEID, caps | - | category, error |
//! | OPEN / CLOSE | ACP SEID | - | error |
//! | START / SUSPEND | ACP SEID list | - | ACP SEID, error |
//! | ABORT | ACP SEID | - | (none: abort has no reject) |
//! | DELAYREPORT | ACP SEID, delay (BE16, 1/10 ms) | - | error |
//! | general reject | - | header only (1.3) | |
//!
//! SEIDs are carried in the upper 6 bits of their octet.

use alloc::vec::Vec;

use crate::Error;
use crate::caps::{self, Capability};
use crate::signal::{Message, MessageType, SignalId};

pub const SEID_MIN: u8 = 0x01;
pub const SEID_MAX: u8 = 0x3E;

fn seid_octet(seid: u8) -> u8 {
    (seid & 0x3F) << 2
}
fn seid_from(o: u8) -> Result<u8, Error> {
    let s = o >> 2;
    if (SEID_MIN..=SEID_MAX).contains(&s) {
        Ok(s)
    } else {
        Err(Error::BadSeid(s))
    }
}

/// Media type in SEP info.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaType {
    Audio,
    Video,
    Multimedia,
    Other(u8),
}
impl MediaType {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Audio,
            1 => Self::Video,
            2 => Self::Multimedia,
            o => Self::Other(o),
        }
    }
}

/// Stream end point role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SepType {
    Source,
    Sink,
}

/// One entry of a DISCOVER response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SepInfo {
    pub seid: u8,
    pub in_use: bool,
    pub media_type: MediaType,
    pub tsep: SepType,
}

impl SepInfo {
    pub fn encode(&self) -> [u8; 2] {
        let mt = match self.media_type {
            MediaType::Audio => 0,
            MediaType::Video => 1,
            MediaType::Multimedia => 2,
            MediaType::Other(v) => v & 0x0F,
        };
        [
            seid_octet(self.seid) | (u8::from(self.in_use) << 1),
            (mt << 4) | (u8::from(self.tsep == SepType::Sink) << 3),
        ]
    }
}

// ---------------------------------------------------------------------------
// Command builders
// ---------------------------------------------------------------------------

fn cmd(label: u8, s: SignalId, payload: Vec<u8>) -> Message {
    Message::new(label, MessageType::Command, s, payload)
}

pub fn discover(label: u8) -> Message {
    cmd(label, SignalId::Discover, Vec::new())
}
pub fn get_capabilities(label: u8, acp_seid: u8) -> Message {
    cmd(
        label,
        SignalId::GetCapabilities,
        alloc::vec![seid_octet(acp_seid)],
    )
}
pub fn get_all_capabilities(label: u8, acp_seid: u8) -> Message {
    cmd(
        label,
        SignalId::GetAllCapabilities,
        alloc::vec![seid_octet(acp_seid)],
    )
}
pub fn set_configuration(
    label: u8,
    acp_seid: u8,
    int_seid: u8,
    config: &[Capability],
) -> Result<Message, Error> {
    let mut p = alloc::vec![seid_octet(acp_seid), seid_octet(int_seid)];
    p.extend_from_slice(&caps::encode(config)?);
    Ok(cmd(label, SignalId::SetConfiguration, p))
}
pub fn get_configuration(label: u8, acp_seid: u8) -> Message {
    cmd(
        label,
        SignalId::GetConfiguration,
        alloc::vec![seid_octet(acp_seid)],
    )
}
pub fn reconfigure(label: u8, acp_seid: u8, config: &[Capability]) -> Result<Message, Error> {
    let mut p = alloc::vec![seid_octet(acp_seid)];
    p.extend_from_slice(&caps::encode(config)?);
    Ok(cmd(label, SignalId::Reconfigure, p))
}
pub fn open(label: u8, acp_seid: u8) -> Message {
    cmd(label, SignalId::Open, alloc::vec![seid_octet(acp_seid)])
}
pub fn start(label: u8, acp_seids: &[u8]) -> Message {
    cmd(
        label,
        SignalId::Start,
        acp_seids.iter().map(|&s| seid_octet(s)).collect(),
    )
}
pub fn close(label: u8, acp_seid: u8) -> Message {
    cmd(label, SignalId::Close, alloc::vec![seid_octet(acp_seid)])
}
pub fn suspend(label: u8, acp_seids: &[u8]) -> Message {
    cmd(
        label,
        SignalId::Suspend,
        acp_seids.iter().map(|&s| seid_octet(s)).collect(),
    )
}
pub fn abort(label: u8, acp_seid: u8) -> Message {
    cmd(label, SignalId::Abort, alloc::vec![seid_octet(acp_seid)])
}
/// `delay` in 1/10 ms units.
pub fn delay_report(label: u8, acp_seid: u8, delay: u16) -> Message {
    let d = delay.to_be_bytes();
    cmd(
        label,
        SignalId::DelayReport,
        alloc::vec![seid_octet(acp_seid), d[0], d[1]],
    )
}

/// Accept with empty payload (what a source answers to e.g. a sink's
/// DELAYREPORT, or general accept).
pub fn accept(label: u8, s: SignalId, payload: Vec<u8>) -> Message {
    Message::new(label, MessageType::ResponseAccept, s, payload)
}
/// Reject carrying just an error code (DISCOVER/GET_*/OPEN/CLOSE/DELAYREPORT).
pub fn reject(label: u8, s: SignalId, error: u8) -> Message {
    Message::new(label, MessageType::ResponseReject, s, alloc::vec![error])
}
/// General reject for an unknown signal (header only, AVDTP 1.3).
pub fn general_reject(label: u8, s: SignalId) -> Message {
    Message::new(label, MessageType::GeneralReject, s, Vec::new())
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Decoded reject payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reject {
    pub error: u8,
    /// SET_CONFIGURATION / RECONFIGURE: first failing service category.
    pub category: Option<u8>,
    /// START / SUSPEND: first failing ACP SEID.
    pub seid: Option<u8>,
}

/// Parse the payload of a ResponseReject for `signal` (layout differs per signal).
pub fn parse_reject(signal: SignalId, p: &[u8]) -> Result<Reject, Error> {
    match signal {
        SignalId::SetConfiguration | SignalId::Reconfigure => match p {
            [c, e] => Ok(Reject {
                error: *e,
                category: Some(*c),
                seid: None,
            }),
            _ => Err(if p.len() < 2 {
                Error::Truncated
            } else {
                Error::BadLength
            }),
        },
        SignalId::Start | SignalId::Suspend => match p {
            [s, e] => Ok(Reject {
                error: *e,
                category: None,
                seid: Some(*s >> 2),
            }),
            _ => Err(if p.len() < 2 {
                Error::Truncated
            } else {
                Error::BadLength
            }),
        },
        _ => match p {
            [e] => Ok(Reject {
                error: *e,
                category: None,
                seid: None,
            }),
            [] => Err(Error::Truncated),
            _ => Err(Error::BadLength),
        },
    }
}

/// Parse a DISCOVER accept.
pub fn parse_discover(p: &[u8]) -> Result<Vec<SepInfo>, Error> {
    if p.is_empty() {
        return Err(Error::Truncated);
    }
    if !p.len().is_multiple_of(2) {
        return Err(Error::BadLength);
    }
    p.as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            Ok(SepInfo {
                seid: seid_from(c[0])?,
                in_use: c[0] & 0x02 != 0,
                media_type: MediaType::from(c[1] >> 4),
                tsep: if c[1] & 0x08 != 0 {
                    SepType::Sink
                } else {
                    SepType::Source
                },
            })
        })
        .collect()
}

/// Parse a GET_CAPABILITIES / GET_ALL_CAPABILITIES / GET_CONFIGURATION accept.
pub fn parse_capabilities(p: &[u8]) -> Result<Vec<Capability>, Error> {
    caps::decode(p)
}

/// Commands as received from the peer (when it is the initiator).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Discover,
    GetCapabilities {
        acp_seid: u8,
    },
    GetAllCapabilities {
        acp_seid: u8,
    },
    SetConfiguration {
        acp_seid: u8,
        int_seid: u8,
        config: Vec<Capability>,
    },
    GetConfiguration {
        acp_seid: u8,
    },
    Reconfigure {
        acp_seid: u8,
        config: Vec<Capability>,
    },
    Open {
        acp_seid: u8,
    },
    Start {
        acp_seids: Vec<u8>,
    },
    Close {
        acp_seid: u8,
    },
    Suspend {
        acp_seids: Vec<u8>,
    },
    Abort {
        acp_seid: u8,
    },
    DelayReport {
        acp_seid: u8,
        delay: u16,
    },
    /// SECURITY_CONTROL or unknown: raw.
    Other {
        signal: SignalId,
        payload: Vec<u8>,
    },
}

/// Decode a command message (message_type must be Command).
pub fn parse_command(m: &Message) -> Result<Command, Error> {
    if m.message_type != MessageType::Command {
        return Err(Error::BadHeader);
    }
    let p = m.payload.as_slice();
    let one = |p: &[u8]| -> Result<u8, Error> {
        match p {
            [s] => seid_from(*s),
            [] => Err(Error::Truncated),
            _ => Err(Error::BadLength),
        }
    };
    let list = |p: &[u8]| -> Result<Vec<u8>, Error> {
        if p.is_empty() {
            return Err(Error::Truncated);
        }
        p.iter().map(|&s| seid_from(s)).collect()
    };
    Ok(match m.signal {
        SignalId::Discover => {
            if !p.is_empty() {
                return Err(Error::BadLength);
            }
            Command::Discover
        }
        SignalId::GetCapabilities => Command::GetCapabilities { acp_seid: one(p)? },
        SignalId::GetAllCapabilities => Command::GetAllCapabilities { acp_seid: one(p)? },
        SignalId::GetConfiguration => Command::GetConfiguration { acp_seid: one(p)? },
        SignalId::Open => Command::Open { acp_seid: one(p)? },
        SignalId::Close => Command::Close { acp_seid: one(p)? },
        SignalId::Abort => Command::Abort { acp_seid: one(p)? },
        SignalId::Start => Command::Start {
            acp_seids: list(p)?,
        },
        SignalId::Suspend => Command::Suspend {
            acp_seids: list(p)?,
        },
        SignalId::SetConfiguration => {
            let [a, i, rest @ ..] = p else {
                return Err(Error::Truncated);
            };
            Command::SetConfiguration {
                acp_seid: seid_from(*a)?,
                int_seid: seid_from(*i)?,
                config: caps::decode(rest)?,
            }
        }
        SignalId::Reconfigure => {
            let [a, rest @ ..] = p else {
                return Err(Error::Truncated);
            };
            Command::Reconfigure {
                acp_seid: seid_from(*a)?,
                config: caps::decode(rest)?,
            }
        }
        SignalId::DelayReport => match p {
            [a, h, l] => Command::DelayReport {
                acp_seid: seid_from(*a)?,
                delay: u16::from_be_bytes([*h, *l]),
            },
            _ if p.len() < 3 => return Err(Error::Truncated),
            _ => return Err(Error::BadLength),
        },
        s => Command::Other {
            signal: s,
            payload: p.to_vec(),
        },
    })
}

/// Outcome of a request/response exchange.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response<'a> {
    Accept(&'a [u8]),
    Reject(Reject),
    GeneralReject,
}

/// Classify `m` as the response to (`label`, `signal`).
pub fn classify_response(m: &Message, label: u8, signal: SignalId) -> Result<Response<'_>, Error> {
    if m.label != label || m.signal != signal {
        return Err(Error::WrongSignal);
    }
    match m.message_type {
        MessageType::Command => Err(Error::BadHeader),
        MessageType::ResponseAccept => Ok(Response::Accept(&m.payload)),
        MessageType::ResponseReject => Ok(Response::Reject(parse_reject(signal, &m.payload)?)),
        MessageType::GeneralReject => Ok(Response::GeneralReject),
    }
}

/// AVDTP error code name (AVDTP 1.3 table 8.67; A2DP codec errors 0xC1+).
/// AVDTP (0x01-0x31) and A2DP (0xC1-0xE5) error codes. The A2DP table
/// follows BlueZ `profiles/audio/a2dp.h` (`enum a2dp_error_codes`).
pub fn error_name(e: u8) -> &'static str {
    match e {
        0x01 => "BAD_HEADER_FORMAT",
        0x11 => "BAD_LENGTH",
        0x12 => "BAD_ACP_SEID",
        0x13 => "SEP_IN_USE",
        0x14 => "SEP_NOT_IN_USE",
        0x17 => "BAD_SERV_CATEGORY",
        0x18 => "BAD_PAYLOAD_FORMAT",
        0x19 => "NOT_SUPPORTED_COMMAND",
        0x1A => "INVALID_CAPABILITIES",
        0x22 => "BAD_RECOVERY_TYPE",
        0x23 => "BAD_MEDIA_TRANSPORT_FORMAT",
        0x25 => "BAD_RECOVERY_FORMAT",
        0x26 => "BAD_ROHC_FORMAT",
        0x27 => "BAD_CP_FORMAT",
        0x28 => "BAD_MULTIPLEXING_FORMAT",
        0x29 => "UNSUPPORTED_CONFIGURATION",
        0x31 => "BAD_STATE",
        0xC1 => "INVALID_CODEC_TYPE",
        0xC2 => "NOT_SUPPORTED_CODEC_TYPE",
        0xC3 => "INVALID_SAMPLING_FREQUENCY",
        0xC4 => "NOT_SUPPORTED_SAMPLING_FREQUENCY",
        0xC5 => "INVALID_CHANNEL_MODE",
        0xC6 => "NOT_SUPPORTED_CHANNEL_MODE",
        0xC7 => "INVALID_SUBBANDS",
        0xC8 => "NOT_SUPPORTED_SUBBANDS",
        0xC9 => "INVALID_ALLOCATION_METHOD",
        0xCA => "NOT_SUPPORTED_ALLOCATION_METHOD",
        0xCB => "INVALID_MINIMUM_BITPOOL_VALUE",
        0xCC => "NOT_SUPPORTED_MINIMUM_BITPOOL_VALUE",
        0xCD => "INVALID_MAXIMUM_BITPOOL_VALUE",
        0xCE => "NOT_SUPPORTED_MAXIMUM_BITPOOL_VALUE",
        0xCF => "INVALID_LAYER",
        0xD0 => "NOT_SUPPORTED_LAYER",
        0xD1 => "NOT_SUPPORTED_CRC",
        0xD2 => "NOT_SUPPORTED_MPF",
        0xD3 => "NOT_SUPPORTED_VBR",
        0xD4 => "INVALID_BIT_RATE",
        0xD5 => "NOT_SUPPORTED_BIT_RATE",
        0xD6 => "INVALID_OBJECT_TYPE",
        0xD7 => "NOT_SUPPORTED_OBJECT_TYPE",
        0xD8 => "INVALID_CHANNELS",
        0xD9 => "NOT_SUPPORTED_CHANNELS",
        0xDA => "INVALID_VERSION",
        0xDB => "NOT_SUPPORTED_VERSION",
        0xDC => "NOT_SUPPORTED_MAXIMUM_SUL",
        0xDD => "INVALID_BLOCK_LENGTH",
        0xE0 => "INVALID_CP_TYPE",
        0xE1 => "INVALID_CP_FORMAT",
        0xE2 => "INVALID_CODEC_PARAMETER",
        0xE3 => "NOT_SUPPORTED_CODEC_PARAMETER",
        0xE4 => "INVALID_DRC",
        0xE5 => "NOT_SUPPORTED_DRC",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::codec::{CodecInfo, Ssc, SscFlags};
    use crate::signal::{Reassembler, fragment};
    use alloc::vec;

    #[test]
    fn a2dp_error_names() {
        assert_eq!(error_name(0xC6), "NOT_SUPPORTED_CHANNEL_MODE");
        assert_eq!(error_name(0xC7), "INVALID_SUBBANDS");
        assert_eq!(error_name(0xCE), "NOT_SUPPORTED_MAXIMUM_BITPOOL_VALUE");
        assert_eq!(error_name(0xDD), "INVALID_BLOCK_LENGTH");
        assert_eq!(error_name(0xE2), "INVALID_CODEC_PARAMETER");
        assert_eq!(error_name(0xE3), "NOT_SUPPORTED_CODEC_PARAMETER");
        assert_eq!(error_name(0xE5), "NOT_SUPPORTED_DRC");
        // Every code in the defined A2DP ranges has a name.
        for e in (0xC1..=0xDD).chain(0xE0..=0xE5) {
            assert_ne!(error_name(e), "UNKNOWN", "{e:#04x}");
        }
        assert_eq!(error_name(0xDE), "UNKNOWN");
        assert_eq!(error_name(0xE6), "UNKNOWN");
    }

    #[test]
    fn command_encodings() {
        assert_eq!(discover(1).encode_single(), vec![0x10, 0x01]);
        assert_eq!(
            get_capabilities(2, 1).encode_single(),
            vec![0x20, 0x02, 0x04]
        );
        assert_eq!(
            get_all_capabilities(2, 5).encode_single(),
            vec![0x20, 0x0C, 0x14]
        );
        assert_eq!(
            get_configuration(0, 1).encode_single(),
            vec![0x00, 0x04, 0x04]
        );
        assert_eq!(open(3, 1).encode_single(), vec![0x30, 0x06, 0x04]);
        assert_eq!(
            start(4, &[1, 2]).encode_single(),
            vec![0x40, 0x07, 0x04, 0x08]
        );
        assert_eq!(close(5, 1).encode_single(), vec![0x50, 0x08, 0x04]);
        assert_eq!(suspend(6, &[1]).encode_single(), vec![0x60, 0x09, 0x04]);
        assert_eq!(abort(7, 1).encode_single(), vec![0x70, 0x0A, 0x04]);
        assert_eq!(
            delay_report(8, 1, 1500).encode_single(),
            vec![0x80, 0x0D, 0x04, 0x05, 0xDC]
        );
        assert_eq!(
            general_reject(9, SignalId::Unknown(0x30)).encode_single(),
            vec![0x91, 0x30]
        );
        assert_eq!(
            reject(1, SignalId::Open, 0x31).encode_single(),
            vec![0x13, 0x06, 0x31]
        );
    }

    #[test]
    fn set_configuration_ssc() {
        let cfg = vec![
            Capability::MediaTransport,
            Capability::MediaCodec {
                media_type: 0,
                codec: CodecInfo::Vendor(
                    Ssc {
                        flags: SscFlags(SscFlags::SR_48K | SscFlags::HIFI_48K_24),
                        extra: vec![],
                    }
                    .to_vendor(),
                ),
            },
        ];
        let m = set_configuration(0xA, 1, 2, &cfg).unwrap();
        assert_eq!(
            m.encode_single(),
            vec![
                0xA0, 0x03, 0x04, 0x08, 0x01, 0x00, 0x07, 0x09, 0x00, 0xFF, 0x75, 0, 0, 0, 0x03,
                0x01, 0x14
            ]
        );
        // And it parses back as a command.
        let Command::SetConfiguration {
            acp_seid: 1,
            int_seid: 2,
            config,
        } = parse_command(&m).unwrap()
        else {
            panic!()
        };
        assert_eq!(config, cfg);
    }

    #[test]
    fn discover_parse() {
        // SEID 1 SNK audio free; SEID 2 SNK audio in use; SEID 3 SRC video.
        let p = [0x04, 0x08, 0x0A, 0x08, 0x0C, 0x10];
        let s = parse_discover(&p).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(
            s[0],
            SepInfo {
                seid: 1,
                in_use: false,
                media_type: MediaType::Audio,
                tsep: SepType::Sink
            }
        );
        assert!(s[1].in_use);
        assert_eq!(s[2].tsep, SepType::Source);
        assert_eq!(s[2].media_type, MediaType::Video);
        for i in &s {
            let e = i.encode();
            assert_eq!(parse_discover(&e).unwrap()[0], *i);
        }
        assert_eq!(parse_discover(&[]), Err(Error::Truncated));
        assert_eq!(parse_discover(&[0x04]), Err(Error::BadLength));
        assert_eq!(parse_discover(&[0x00, 0x08]), Err(Error::BadSeid(0)));
        assert_eq!(parse_discover(&[0xFC, 0x08]), Err(Error::BadSeid(0x3F)));
    }

    #[test]
    fn reject_layouts() {
        assert_eq!(
            parse_reject(SignalId::SetConfiguration, &[0x07, 0x29]).unwrap(),
            Reject {
                error: 0x29,
                category: Some(7),
                seid: None
            }
        );
        assert_eq!(
            parse_reject(SignalId::Reconfigure, &[0x07, 0x29])
                .unwrap()
                .category,
            Some(7)
        );
        assert_eq!(
            parse_reject(SignalId::Start, &[0x04, 0x31]).unwrap(),
            Reject {
                error: 0x31,
                category: None,
                seid: Some(1)
            }
        );
        assert_eq!(
            parse_reject(SignalId::Suspend, &[0x08, 0x31]).unwrap().seid,
            Some(2)
        );
        assert_eq!(
            parse_reject(SignalId::Open, &[0x31]).unwrap(),
            Reject {
                error: 0x31,
                category: None,
                seid: None
            }
        );
        assert_eq!(parse_reject(SignalId::Open, &[]), Err(Error::Truncated));
        assert_eq!(parse_reject(SignalId::Open, &[1, 2]), Err(Error::BadLength));
        assert_eq!(parse_reject(SignalId::Start, &[1]), Err(Error::Truncated));
        assert_eq!(
            parse_reject(SignalId::SetConfiguration, &[1, 2, 3]),
            Err(Error::BadLength)
        );
        assert_eq!(error_name(0x29), "UNSUPPORTED_CONFIGURATION");
    }

    #[test]
    fn classify() {
        let m = Message::new(
            4,
            MessageType::ResponseAccept,
            SignalId::Discover,
            vec![0x04, 0x08],
        );
        assert_eq!(
            classify_response(&m, 4, SignalId::Discover).unwrap(),
            Response::Accept(&[0x04, 0x08])
        );
        assert_eq!(
            classify_response(&m, 5, SignalId::Discover),
            Err(Error::WrongSignal)
        );
        assert_eq!(
            classify_response(&m, 4, SignalId::Open),
            Err(Error::WrongSignal)
        );
        let g = general_reject(4, SignalId::Discover);
        assert_eq!(
            classify_response(&g, 4, SignalId::Discover).unwrap(),
            Response::GeneralReject
        );
        let r = Message::new(
            4,
            MessageType::ResponseReject,
            SignalId::Start,
            vec![0x04, 0x31],
        );
        assert!(matches!(
            classify_response(&r, 4, SignalId::Start).unwrap(),
            Response::Reject(Reject { seid: Some(1), .. })
        ));
        assert_eq!(
            classify_response(&discover(4), 4, SignalId::Discover),
            Err(Error::BadHeader)
        );
    }

    #[test]
    fn parse_commands() {
        assert_eq!(parse_command(&discover(0)).unwrap(), Command::Discover);
        assert_eq!(
            parse_command(&start(0, &[1, 3])).unwrap(),
            Command::Start {
                acp_seids: vec![1, 3]
            }
        );
        assert_eq!(
            parse_command(&delay_report(0, 2, 300)).unwrap(),
            Command::DelayReport {
                acp_seid: 2,
                delay: 300
            }
        );
        assert_eq!(
            parse_command(&abort(0, 2)).unwrap(),
            Command::Abort { acp_seid: 2 }
        );
        let bad = Message::new(0, MessageType::Command, SignalId::Open, vec![]);
        assert_eq!(parse_command(&bad), Err(Error::Truncated));
        let bad = Message::new(0, MessageType::Command, SignalId::Open, vec![4, 4]);
        assert_eq!(parse_command(&bad), Err(Error::BadLength));
        let bad = Message::new(0, MessageType::Command, SignalId::DelayReport, vec![4, 0]);
        assert_eq!(parse_command(&bad), Err(Error::Truncated));
        let bad = Message::new(0, MessageType::Command, SignalId::SetConfiguration, vec![4]);
        assert_eq!(parse_command(&bad), Err(Error::Truncated));
        let bad = Message::new(0, MessageType::Command, SignalId::Start, vec![]);
        assert_eq!(parse_command(&bad), Err(Error::Truncated));
        let sc = Message::new(
            0,
            MessageType::Command,
            SignalId::SecurityControl,
            vec![4, 1, 2],
        );
        assert!(matches!(parse_command(&sc).unwrap(), Command::Other { .. }));
        assert_eq!(
            parse_command(&accept(0, SignalId::Open, vec![])),
            Err(Error::BadHeader)
        );
    }

    #[test]
    fn fragmented_get_all_capabilities_response() {
        // A long capabilities response (e.g. SSC + big vendor blobs) arriving
        // in 3 packets over a small (32-byte) MTU signaling channel.
        let mut payload = crate::caps::tests_support::ssc_caps();
        payload.extend_from_slice(&[0x07, 0x2C, 0x00, 0xFF]); // LOSC = 2 + 6 + 36
        payload.extend_from_slice(&[0x4F, 0x00, 0x00, 0x00, 0x01, 0x00]);
        payload.extend((0..36).map(|x| x as u8));
        let m = accept(7, SignalId::GetAllCapabilities, payload);
        let pkts = fragment(&m, 32).unwrap();
        assert_eq!(pkts.len(), 3);
        let mut r = Reassembler::new();
        let mut got = None;
        for p in &pkts {
            got = r.push(p).unwrap();
        }
        let got = got.unwrap();
        let Response::Accept(p) = classify_response(&got, 7, SignalId::GetAllCapabilities).unwrap()
        else {
            panic!()
        };
        let caps = parse_capabilities(p).unwrap();
        assert_eq!(caps.len(), 5);
        let ssc = caps
            .iter()
            .find_map(|c| match c {
                Capability::MediaCodec {
                    codec: CodecInfo::Vendor(v),
                    ..
                } => v.ssc(),
                _ => None,
            })
            .unwrap();
        assert_eq!(ssc.flags.0, 0x16);
    }

    #[test]
    fn never_panics() {
        let mut x: u32 = 7;
        for _ in 0..30000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let n = (x % 9) as usize;
            let b = x.to_le_bytes();
            let p: std::vec::Vec<u8> = (0..n)
                .map(|i| b[i % 4] ^ (i as u8).wrapping_mul(37))
                .collect();
            let s = SignalId::from_u8((x >> 8) as u8);
            let _ = parse_discover(&p);
            let _ = parse_reject(s, &p);
            let _ = parse_capabilities(&p);
            let _ = parse_command(&Message::new(0, MessageType::Command, s, p));
        }
    }
}
