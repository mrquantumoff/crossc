//! AVDTP signaling client over an [`SduChannel`], and the capability probe
//! (DISCOVER + GET_ALL_CAPABILITIES on every SEP). Pure logic, unit-tested
//! against a fake sink.

use std::time::Duration;

use avdtp::caps::Capability;
use avdtp::msg::{self, Reject, Response, SepInfo};
use avdtp::signal::{Labels, Message, MessageType, Reassembler, SignalId, fragment};

use crate::channel::{ChanError, SduChannel};
use crate::hex;
use crate::peer::{LocalStream, PeerEvent};

pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// AVDTP signaling client.
pub struct Signaling<C: SduChannel> {
    pub ch: C,
    labels: Labels,
    rx: Reassembler,
    /// Log of everything sent/received (hex), for `--verbose`.
    pub trace: Vec<String>,
    /// Our configured stream, once there is one (enables serving peer commands).
    pub stream: Option<LocalStream>,
    /// Events from peer commands answered inside [`Self::request`].
    pub events: Vec<PeerEvent>,
}

impl<C: SduChannel> Signaling<C> {
    pub fn new(ch: C) -> Self {
        Self {
            ch,
            labels: Labels::default(),
            rx: Reassembler::new(),
            trace: Vec::new(),
            stream: None,
            events: Vec::new(),
        }
    }

    pub fn send_msg(&mut self, m: &Message) -> Result<(), ChanError> {
        let pkts = fragment(m, self.ch.out_mtu())
            .ok_or_else(|| ChanError::Protocol("message does not fit MTU".into()))?;
        for p in pkts {
            self.trace.push(format!("> {}", hex::format(&p)));
            self.ch.send(&p)?;
        }
        Ok(())
    }

    /// Receive one complete message.
    pub fn recv_msg(&mut self, timeout: Duration) -> Result<Message, ChanError> {
        loop {
            let sdu = self.ch.recv(timeout)?;
            self.trace.push(format!("< {}", hex::format(&sdu)));
            match self.rx.push(&sdu) {
                Ok(Some(m)) => return Ok(m),
                Ok(None) => continue,
                Err(e) => self
                    .trace
                    .push(format!("  (dropped malformed packet: {e})")),
            }
        }
    }

    /// Answer a peer-initiated command (see [`crate::peer::answer`]): refused
    /// while `stream` is None, served once a stream is configured. Any
    /// resulting event is appended to `events`.
    pub fn answer_peer(&mut self, r: &Message) -> Result<Option<PeerEvent>, ChanError> {
        let (reply, ev) = crate::peer::answer(r, self.stream.as_ref());
        self.trace.push(format!(
            "  peer command {} (label {}): {}",
            r.signal.name(),
            r.label,
            match reply.message_type {
                MessageType::ResponseAccept => "accepted",
                MessageType::GeneralReject => "general reject",
                _ => "rejected",
            }
        ));
        self.send_msg(&reply)?;
        if let Some(e) = &ev {
            self.trace.push(format!("  {e}"));
            self.events.push(e.clone());
        }
        Ok(ev)
    }

    /// Take one SDU that arrived outside [`Self::request`] (the caller waits
    /// for the channel together with other things): a complete peer command
    /// is answered ([`Self::answer_peer`], its event lands in `events`), a
    /// stray response is ignored, a malformed packet is dropped.
    pub fn handle_sdu(&mut self, sdu: &[u8]) -> Result<(), ChanError> {
        self.trace.push(format!("< {}", hex::format(sdu)));
        match self.rx.push(sdu) {
            Ok(Some(m)) if m.message_type == MessageType::Command => {
                self.answer_peer(&m).map(|_| ())
            }
            Ok(Some(m)) => {
                self.trace.push(format!(
                    "  ignoring unsolicited response {} label {}",
                    m.signal.name(),
                    m.label
                ));
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e) => {
                self.trace
                    .push(format!("  (dropped malformed packet: {e})"));
                Ok(())
            }
        }
    }

    /// Send a command and wait for its response. Peer-initiated commands that
    /// arrive meanwhile are answered by [`Self::answer_peer`].
    pub fn request(&mut self, build: impl FnOnce(u8) -> Message) -> Result<Message, ChanError> {
        let label = self.labels.next_label();
        let m = build(label);
        let signal = m.signal;
        self.send_msg(&m)?;
        loop {
            let r = self.recv_msg(RESPONSE_TIMEOUT)?;
            if r.message_type == MessageType::Command {
                self.answer_peer(&r)?;
                continue;
            }
            if r.label == label && r.signal == signal {
                return Ok(r);
            }
            self.trace.push(format!(
                "  ignoring unrelated response {} label {}",
                r.signal.name(),
                r.label
            ));
        }
    }
}

/// Result of probing one SEP.
#[derive(Clone, Debug)]
pub struct SepReport {
    pub info: SepInfo,
    pub via: &'static str,
    pub caps: Result<Vec<Capability>, String>,
}

fn describe_response(signal: SignalId, r: &Message) -> Result<Vec<u8>, String> {
    match msg::classify_response(r, r.label, signal) {
        Ok(Response::Accept(p)) => Ok(p.to_vec()),
        Ok(Response::Reject(Reject { error, .. })) => Err(format!(
            "rejected: {} ({error:#04x})",
            msg::error_name(error)
        )),
        Ok(Response::GeneralReject) => Err("general reject".into()),
        Err(e) => Err(format!("bad response: {e}")),
    }
}

/// DISCOVER then GET_ALL_CAPABILITIES (falling back to GET_CAPABILITIES) per SEP.
///
/// Only a DISCOVER failure is an `Err`. A channel error (timeout, disconnect)
/// while asking one SEP is recorded in that SEP's report, the remaining SEPs
/// are marked "not probed", and the reports collected so far are returned.
pub fn probe<C: SduChannel>(sig: &mut Signaling<C>) -> Result<Vec<SepReport>, ChanError> {
    let r = sig.request(msg::discover)?;
    let p = describe_response(SignalId::Discover, &r)
        .map_err(|e| ChanError::Protocol(format!("DISCOVER {e}")))?;
    let seps = msg::parse_discover(&p)
        .map_err(|e| ChanError::Protocol(format!("DISCOVER response: {e}")))?;
    let mut out = Vec::new();
    let mut aborted: Option<String> = None;
    for info in seps {
        if let Some(why) = &aborted {
            out.push(SepReport {
                info,
                via: "-",
                caps: Err(format!("not probed: {why}")),
            });
            continue;
        }
        match probe_sep(sig, info.seid) {
            Ok((via, res)) => {
                let caps = res.and_then(|p| {
                    msg::parse_capabilities(&p).map_err(|e| format!("capabilities: {e}"))
                });
                out.push(SepReport { info, via, caps });
            }
            Err((via, e)) => {
                out.push(SepReport {
                    info,
                    via,
                    caps: Err(format!("channel error: {e}")),
                });
                aborted = Some(format!("channel error on SEP {:#04x}", info.seid));
            }
        }
    }
    Ok(out)
}

type SepResult = (&'static str, Result<Vec<u8>, String>);

fn probe_sep<C: SduChannel>(
    sig: &mut Signaling<C>,
    seid: u8,
) -> Result<SepResult, (&'static str, ChanError)> {
    let r = sig
        .request(|l| msg::get_all_capabilities(l, seid))
        .map_err(|e| ("GET_ALL_CAPABILITIES", e))?;
    Ok(match describe_response(SignalId::GetAllCapabilities, &r) {
        Ok(p) => ("GET_ALL_CAPABILITIES", Ok(p)),
        Err(_) => {
            // AVDTP < 1.3 sinks: plain GET_CAPABILITIES.
            let r = sig
                .request(|l| msg::get_capabilities(l, seid))
                .map_err(|e| ("GET_CAPABILITIES", e))?;
            (
                "GET_CAPABILITIES",
                describe_response(SignalId::GetCapabilities, &r),
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeSink;

    #[test]
    fn probe_with_fragmentation_and_peer_command() {
        let mut fake = FakeSink::new(16); // forces Start/Continue/End packets
        fake.inject_peer_command = true;
        let mut sig = Signaling::new(fake);
        let r = probe(&mut sig).unwrap();
        assert_eq!(r.len(), 3);
        assert!(r[2].caps.as_ref().unwrap().len() == 4);
        // We answered the peer's DISCOVER with a reject (label 0xF, NOT_SUPPORTED_COMMAND).
        assert!(sig.ch.sent.iter().any(|p| p == &vec![0xF3, 0x01, 0x19]));
    }

    #[test]
    fn falls_back_to_get_capabilities() {
        let mut fake = FakeSink::new(672);
        fake.no_get_all = true;
        let mut sig = Signaling::new(fake);
        let r = probe(&mut sig).unwrap();
        assert!(
            r.iter()
                .all(|x| x.via == "GET_CAPABILITIES" && x.caps.is_ok())
        );
    }

    #[test]
    fn timeout_propagates() {
        struct Silent;
        impl SduChannel for Silent {
            fn send(&mut self, _: &[u8]) -> Result<(), ChanError> {
                Ok(())
            }
            fn recv(&mut self, _: Duration) -> Result<Vec<u8>, ChanError> {
                Err(ChanError::Timeout)
            }
            fn out_mtu(&self) -> usize {
                48
            }
        }
        assert!(matches!(
            probe(&mut Signaling::new(Silent)),
            Err(ChanError::Timeout)
        ));
    }

    #[test]
    fn sdus_outside_a_request_are_served() {
        use crate::peer::{LocalStream, PeerEvent};
        let mut sig = Signaling::new(FakeSink::new(672));
        sig.stream = Some(LocalStream {
            int_seid: 1,
            config: avdtp::caps::ssc_configuration(0x0C, true),
            caps: avdtp::caps::ssc_configuration(0x3E, true),
        });
        sig.handle_sdu(&msg::delay_report(4, 1, 3400).encode_single())
            .unwrap();
        assert_eq!(
            sig.events,
            vec![PeerEvent::Delay {
                seid: 1,
                delay: 3400
            }]
        );
        assert!(sig.ch.sent.contains(&vec![0x42, 0x0D]), "accepted");
        // A stray response and garbage change nothing.
        let n = sig.ch.sent.len();
        sig.handle_sdu(&msg::accept(3, SignalId::Open, Vec::new()).encode_single())
            .unwrap();
        sig.handle_sdu(&[]).unwrap();
        assert_eq!((sig.ch.sent.len(), sig.events.len()), (n, 1));
    }
}
