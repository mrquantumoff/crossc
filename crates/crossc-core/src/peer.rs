//! Answers to commands the sink initiates on the signaling channel.
//!
//! Before a stream exists every peer command is refused. Once a stream is
//! configured, the sink may legitimately
//! talk to our (virtual) source SEP: DELAYREPORT after SET_CONFIGURATION and
//! during streaming, GET_CONFIGURATION / GET_(ALL_)CAPABILITIES / DISCOVER on
//! our SEID, CLOSE / ABORT / SUSPEND to end or pause the stream, and START to
//! (re)start it. A START we accept is a started stream: it is reported as
//! [`PeerEvent::Started`], so that both sides agree about the stream state.

use avdtp::caps::{self, Capability};
use avdtp::msg::{self, MediaType, SepInfo, SepType};
use avdtp::signal::{Message, MessageType, SignalId};

/// What our side of a configured stream looks like to the sink.
#[derive(Clone, Debug)]
pub struct LocalStream {
    /// Our SEID (the INT SEID we sent in SET_CONFIGURATION).
    pub int_seid: u8,
    /// The configuration we set (answer to GET_CONFIGURATION).
    pub config: Vec<Capability>,
    /// Our source SEP's capabilities (answer to GET_ALL_CAPABILITIES;
    /// GET_CAPABILITIES gets the same minus Delay Reporting, AVDTP 1.3 8.7).
    pub caps: Vec<Capability>,
}

/// Something the sink told us that the streaming loop must know about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerEvent {
    /// DELAYREPORT: `delay` in 1/10 ms.
    Delay {
        seid: u8,
        delay: u16,
    },
    /// The sink closed / aborted / suspended the stream: stop sending.
    Closed,
    Aborted,
    Suspended,
    /// The sink started the stream itself (a START for our SEID, which we
    /// accepted): the stream is streaming now, without a START from us.
    Started,
}

impl std::fmt::Display for PeerEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerEvent::Delay { seid, delay } => write!(
                f,
                "sink DELAYREPORT: {}.{} ms (SEID {seid:#04x})",
                delay / 10,
                delay % 10
            ),
            PeerEvent::Closed => f.write_str("sink sent CLOSE"),
            PeerEvent::Aborted => f.write_str("sink sent ABORT"),
            PeerEvent::Suspended => f.write_str("sink sent SUSPEND"),
            PeerEvent::Started => f.write_str("sink sent START"),
        }
    }
}

/// Reply for SET_CONFIGURATION / RECONFIGURE / START / SUSPEND rejects,
/// whose reject payload carries a first octet before the error code.
fn reject_with_first(r: &Message, first: u8, error: u8) -> Message {
    Message::new(
        r.label,
        MessageType::ResponseReject,
        r.signal,
        vec![first, error],
    )
}

const NOT_SUPPORTED_COMMAND: u8 = 0x19;
const BAD_ACP_SEID: u8 = 0x12;

/// The pre-stream refusal (probe/media behaviour).
fn refuse(r: &Message) -> (Message, Option<PeerEvent>) {
    let rej = match r.signal {
        SignalId::Unknown(_) => msg::general_reject(r.label, r.signal),
        // ABORT has no reject; accept it.
        SignalId::Abort => {
            return (
                msg::accept(r.label, r.signal, Vec::new()),
                Some(PeerEvent::Aborted),
            );
        }
        SignalId::SetConfiguration | SignalId::Reconfigure => {
            reject_with_first(r, 0x00, NOT_SUPPORTED_COMMAND)
        }
        SignalId::Start | SignalId::Suspend => {
            let seid = r.payload.first().copied().unwrap_or(0x04);
            reject_with_first(r, seid, NOT_SUPPORTED_COMMAND)
        }
        s => msg::reject(r.label, s, NOT_SUPPORTED_COMMAND),
    };
    (rej, None)
}

/// Answer one peer command `r` (message_type must be Command).
pub fn answer(r: &Message, stream: Option<&LocalStream>) -> (Message, Option<PeerEvent>) {
    let Some(st) = stream else {
        return refuse(r);
    };
    let Ok(cmd) = msg::parse_command(r) else {
        return match r.signal {
            SignalId::Unknown(_) => (msg::general_reject(r.label, r.signal), None),
            // Malformed known command: BAD_LENGTH (0x11) where a plain reject exists.
            _ => refuse(r),
        };
    };
    let accept = |payload: Vec<u8>| msg::accept(r.label, r.signal, payload);
    let ours = |seid: u8| seid == st.int_seid;
    match cmd {
        msg::Command::DelayReport { acp_seid, delay } => (
            accept(Vec::new()),
            Some(PeerEvent::Delay {
                seid: acp_seid,
                delay,
            }),
        ),
        msg::Command::Discover => {
            let sep = SepInfo {
                seid: st.int_seid,
                in_use: true,
                media_type: MediaType::Audio,
                tsep: SepType::Source,
            };
            (accept(sep.encode().to_vec()), None)
        }
        msg::Command::GetConfiguration { acp_seid } if ours(acp_seid) => {
            match caps::encode(&st.config) {
                Ok(p) => (accept(p), None),
                Err(_) => (msg::reject(r.label, r.signal, NOT_SUPPORTED_COMMAND), None),
            }
        }
        msg::Command::GetAllCapabilities { acp_seid } if ours(acp_seid) => {
            match caps::encode(&st.caps) {
                Ok(p) => (accept(p), None),
                Err(_) => (msg::reject(r.label, r.signal, NOT_SUPPORTED_COMMAND), None),
            }
        }
        msg::Command::GetCapabilities { acp_seid } if ours(acp_seid) => {
            let basic: Vec<Capability> = st
                .caps
                .iter()
                .filter(|c| !matches!(c, Capability::DelayReporting))
                .cloned()
                .collect();
            match caps::encode(&basic) {
                Ok(p) => (accept(p), None),
                Err(_) => (msg::reject(r.label, r.signal, NOT_SUPPORTED_COMMAND), None),
            }
        }
        msg::Command::GetConfiguration { .. }
        | msg::Command::GetAllCapabilities { .. }
        | msg::Command::GetCapabilities { .. } => {
            (msg::reject(r.label, r.signal, BAD_ACP_SEID), None)
        }
        msg::Command::Close { .. } => (accept(Vec::new()), Some(PeerEvent::Closed)),
        msg::Command::Abort { .. } => (accept(Vec::new()), Some(PeerEvent::Aborted)),
        msg::Command::Suspend { .. } => (accept(Vec::new()), Some(PeerEvent::Suspended)),
        // Either side may start an open stream (AVDTP 1.3, 6.12). Accepting
        // makes it a started stream, so the caller has to hear about it: a
        // sink that was told "accepted" refuses our own START afterwards.
        msg::Command::Start { acp_seids } => match acp_seids.iter().find(|s| !ours(**s)) {
            None => (accept(Vec::new()), Some(PeerEvent::Started)),
            Some(&other) => (reject_with_first(r, other << 2, BAD_ACP_SEID), None),
        },
        msg::Command::Other {
            signal: SignalId::Unknown(_),
            ..
        } => (msg::general_reject(r.label, r.signal), None),
        _ => refuse(r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream() -> LocalStream {
        LocalStream {
            int_seid: 1,
            config: caps::ssc_configuration(0x0C, true),
            caps: caps::ssc_configuration(0x3E, true),
        }
    }

    #[test]
    fn delay_report_is_accepted() {
        let st = stream();
        let (rep, ev) = answer(&msg::delay_report(5, 1, 1234), Some(&st));
        assert_eq!(rep.message_type, MessageType::ResponseAccept);
        assert_eq!((rep.label, rep.signal), (5, SignalId::DelayReport));
        assert_eq!(
            ev,
            Some(PeerEvent::Delay {
                seid: 1,
                delay: 1234
            })
        );
        assert_eq!(
            ev.unwrap().to_string(),
            "sink DELAYREPORT: 123.4 ms (SEID 0x01)"
        );
        // Without a stream it is refused, as probe/media always did.
        let (rep, ev) = answer(&msg::delay_report(5, 1, 1234), None);
        assert_eq!(rep.message_type, MessageType::ResponseReject);
        assert_eq!(ev, None);
    }

    #[test]
    fn configuration_and_capabilities() {
        let st = stream();
        let (rep, _) = answer(&msg::get_configuration(1, 1), Some(&st));
        assert_eq!(rep.message_type, MessageType::ResponseAccept);
        assert_eq!(msg::parse_capabilities(&rep.payload).unwrap(), st.config);
        let (rep, _) = answer(&msg::get_all_capabilities(2, 1), Some(&st));
        assert_eq!(msg::parse_capabilities(&rep.payload).unwrap(), st.caps);
        let (rep, _) = answer(&msg::get_capabilities(3, 1), Some(&st));
        let basic = msg::parse_capabilities(&rep.payload).unwrap();
        assert_eq!(basic.len(), 2, "no Delay Reporting in GET_CAPABILITIES");
        let (rep, _) = answer(&msg::get_configuration(4, 7), Some(&st));
        assert_eq!(rep.message_type, MessageType::ResponseReject);
        assert_eq!(rep.payload, [BAD_ACP_SEID]);
        let (rep, _) = answer(&msg::discover(6), Some(&st));
        let seps = msg::parse_discover(&rep.payload).unwrap();
        assert_eq!(seps.len(), 1);
        assert_eq!((seps[0].seid, seps[0].tsep), (1, SepType::Source));
    }

    #[test]
    fn stream_ending_commands_and_unknown() {
        let st = stream();
        assert_eq!(
            answer(&msg::close(1, 1), Some(&st)).1,
            Some(PeerEvent::Closed)
        );
        assert_eq!(
            answer(&msg::abort(1, 1), Some(&st)).1,
            Some(PeerEvent::Aborted)
        );
        assert_eq!(answer(&msg::abort(1, 1), None).1, Some(PeerEvent::Aborted));
        assert_eq!(
            answer(&msg::suspend(1, &[1]), Some(&st)).1,
            Some(PeerEvent::Suspended)
        );
        assert_eq!(
            PeerEvent::Started.to_string(),
            "sink sent START",
            "what the logs show"
        );
        let unk = Message::new(2, MessageType::Command, SignalId::Unknown(0x3F), vec![]);
        let (rep, ev) = answer(&unk, Some(&st));
        assert_eq!(rep.message_type, MessageType::GeneralReject);
        assert_eq!(ev, None);
        let setc = msg::set_configuration(3, 1, 2, &st.config).unwrap();
        let (rep, _) = answer(&setc, Some(&st));
        assert_eq!(rep.message_type, MessageType::ResponseReject);
        assert_eq!(rep.payload, [0x00, NOT_SUPPORTED_COMMAND]);
    }

    #[test]
    fn a_start_from_the_sink_is_accepted_and_reported() {
        let st = stream();
        // For our SEID: accepted, and the caller is told the stream runs.
        let (rep, ev) = answer(&msg::start(7, &[1]), Some(&st));
        assert_eq!(rep.message_type, MessageType::ResponseAccept);
        assert_eq!((rep.label, rep.signal), (7, SignalId::Start));
        assert!(rep.payload.is_empty());
        assert_eq!(ev, Some(PeerEvent::Started));
        // For a SEID that is not ours: rejected naming it, nothing started.
        let (rep, ev) = answer(&msg::start(8, &[1, 5]), Some(&st));
        assert_eq!(rep.message_type, MessageType::ResponseReject);
        assert_eq!(rep.payload, [5 << 2, BAD_ACP_SEID]);
        assert_eq!(ev, None);
        // Naming no SEID at all: malformed, refused, nothing started.
        let (rep, ev) = answer(&msg::start(8, &[]), Some(&st));
        assert_eq!(rep.message_type, MessageType::ResponseReject);
        assert_eq!(ev, None);
        // Before a stream exists: refused, as ever.
        let (rep, ev) = answer(&msg::start(9, &[1]), None);
        assert_eq!(rep.message_type, MessageType::ResponseReject);
        assert_eq!(ev, None);
    }
}
