//! AVDTP signaling packet layer (AVDTP 1.3, section 8.4 / 8.5).
//!
//! ```text
//! Single:   | TL(4) PT=00(2) MT(2) | RFA(2) SignalID(6) | payload...
//! Start:    | TL(4) PT=01(2) MT(2) | NOSP(8) | RFA(2) SignalID(6) | payload...
//! Continue: | TL(4) PT=10(2) MT(2) | payload...
//! End:      | TL(4) PT=11(2) MT(2) | payload...
//! ```
//! Continue and End packets carry no signal id; Start carries the number of
//! signal packets (NOSP, including the start packet itself) before it.

use alloc::vec::Vec;

use crate::Error;

/// Message type (2 bits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageType {
    Command = 0,
    GeneralReject = 1,
    ResponseAccept = 2,
    ResponseReject = 3,
}

impl MessageType {
    fn from_bits(b: u8) -> Self {
        match b & 3 {
            0 => Self::Command,
            1 => Self::GeneralReject,
            2 => Self::ResponseAccept,
            _ => Self::ResponseReject,
        }
    }
}

/// Packet type (2 bits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketType {
    Single = 0,
    Start = 1,
    Continue = 2,
    End = 3,
}

impl PacketType {
    fn from_bits(b: u8) -> Self {
        match b & 3 {
            0 => Self::Single,
            1 => Self::Start,
            2 => Self::Continue,
            _ => Self::End,
        }
    }
}

/// Signal identifier (6 bits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalId {
    Discover,
    GetCapabilities,
    SetConfiguration,
    GetConfiguration,
    Reconfigure,
    Open,
    Start,
    Close,
    Suspend,
    Abort,
    SecurityControl,
    GetAllCapabilities,
    DelayReport,
    /// Reserved / unknown value (kept so general-reject can echo it).
    Unknown(u8),
}

impl SignalId {
    pub fn from_u8(v: u8) -> Self {
        match v & 0x3F {
            0x01 => Self::Discover,
            0x02 => Self::GetCapabilities,
            0x03 => Self::SetConfiguration,
            0x04 => Self::GetConfiguration,
            0x05 => Self::Reconfigure,
            0x06 => Self::Open,
            0x07 => Self::Start,
            0x08 => Self::Close,
            0x09 => Self::Suspend,
            0x0A => Self::Abort,
            0x0B => Self::SecurityControl,
            0x0C => Self::GetAllCapabilities,
            0x0D => Self::DelayReport,
            o => Self::Unknown(o),
        }
    }
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Discover => 0x01,
            Self::GetCapabilities => 0x02,
            Self::SetConfiguration => 0x03,
            Self::GetConfiguration => 0x04,
            Self::Reconfigure => 0x05,
            Self::Open => 0x06,
            Self::Start => 0x07,
            Self::Close => 0x08,
            Self::Suspend => 0x09,
            Self::Abort => 0x0A,
            Self::SecurityControl => 0x0B,
            Self::GetAllCapabilities => 0x0C,
            Self::DelayReport => 0x0D,
            Self::Unknown(v) => v & 0x3F,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Discover => "DISCOVER",
            Self::GetCapabilities => "GET_CAPABILITIES",
            Self::SetConfiguration => "SET_CONFIGURATION",
            Self::GetConfiguration => "GET_CONFIGURATION",
            Self::Reconfigure => "RECONFIGURE",
            Self::Open => "OPEN",
            Self::Start => "START",
            Self::Close => "CLOSE",
            Self::Suspend => "SUSPEND",
            Self::Abort => "ABORT",
            Self::SecurityControl => "SECURITY_CONTROL",
            Self::GetAllCapabilities => "GET_ALL_CAPABILITIES",
            Self::DelayReport => "DELAYREPORT",
            Self::Unknown(_) => "UNKNOWN",
        }
    }
}

/// A complete (reassembled) signaling message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// Transaction label (4 bits).
    pub label: u8,
    pub message_type: MessageType,
    pub signal: SignalId,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn new(label: u8, message_type: MessageType, signal: SignalId, payload: Vec<u8>) -> Self {
        Self {
            label: label & 0x0F,
            message_type,
            signal,
            payload,
        }
    }

    /// Encode as a single packet (no fragmentation).
    pub fn encode_single(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(2 + self.payload.len());
        v.push(hdr_byte(self.label, PacketType::Single, self.message_type));
        v.push(self.signal.to_u8());
        v.extend_from_slice(&self.payload);
        v
    }
}

fn hdr_byte(label: u8, pt: PacketType, mt: MessageType) -> u8 {
    ((label & 0x0F) << 4) | ((pt as u8) << 2) | (mt as u8)
}

/// Split a message into signaling packets no larger than `mtu` (the peer's
/// L2CAP MTU for the signaling channel). Uses a single packet when it fits.
/// Returns `None` if `mtu` is too small (< 3) or more than 255 packets would
/// be needed.
pub fn fragment(m: &Message, mtu: usize) -> Option<Vec<Vec<u8>>> {
    if 2 + m.payload.len() <= mtu {
        return Some(alloc::vec![m.encode_single()]);
    }
    if mtu < 3 {
        return None;
    }
    let first = mtu - 3;
    let rest = mtu - 1;
    let remaining = m.payload.len() - first;
    let nosp = 1 + remaining.div_ceil(rest);
    if nosp > 255 {
        return None;
    }
    let mut out = Vec::with_capacity(nosp);
    let mut p = Vec::with_capacity(mtu);
    p.push(hdr_byte(m.label, PacketType::Start, m.message_type));
    p.push(nosp as u8);
    p.push(m.signal.to_u8());
    p.extend_from_slice(&m.payload[..first]);
    out.push(p);
    let mut off = first;
    while off < m.payload.len() {
        let end = (off + rest).min(m.payload.len());
        let pt = if end == m.payload.len() {
            PacketType::End
        } else {
            PacketType::Continue
        };
        let mut p = Vec::with_capacity(1 + end - off);
        p.push(hdr_byte(m.label, pt, m.message_type));
        p.extend_from_slice(&m.payload[off..end]);
        out.push(p);
        off = end;
    }
    Some(out)
}

/// Reassembles signaling packets into [`Message`]s. One instance per
/// signaling channel and direction-agnostic (AVDTP does not interleave
/// fragmented messages on one channel).
#[derive(Debug, Default)]
pub struct Reassembler {
    partial: Option<Partial>,
}

#[derive(Debug)]
struct Partial {
    label: u8,
    message_type: MessageType,
    signal: SignalId,
    nosp: u8,
    received: u8,
    payload: Vec<u8>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while a fragmented message is incomplete.
    pub fn in_progress(&self) -> bool {
        self.partial.is_some()
    }

    /// Feed one L2CAP SDU. Returns `Ok(Some(msg))` when a message is complete,
    /// `Ok(None)` when more fragments are needed. On error the partial state is
    /// discarded (a following Start/Single packet resynchronises).
    pub fn push(&mut self, pkt: &[u8]) -> Result<Option<Message>, Error> {
        let r = self.push_inner(pkt);
        if r.is_err() {
            self.partial = None;
        }
        r
    }

    fn push_inner(&mut self, pkt: &[u8]) -> Result<Option<Message>, Error> {
        let b0 = *pkt.first().ok_or(Error::Truncated)?;
        let label = b0 >> 4;
        let pt = PacketType::from_bits(b0 >> 2);
        let mt = MessageType::from_bits(b0);
        match pt {
            PacketType::Single => {
                // A Single packet abandons any partial message.
                self.partial = None;
                let b1 = *pkt.get(1).ok_or(Error::Truncated)?;
                Ok(Some(Message {
                    label,
                    message_type: mt,
                    signal: SignalId::from_u8(b1),
                    payload: pkt[2..].to_vec(),
                }))
            }
            PacketType::Start => {
                self.partial = None;
                if pkt.len() < 3 {
                    return Err(Error::Truncated);
                }
                let nosp = pkt[1];
                if nosp < 2 {
                    return Err(Error::BadPacketCount);
                }
                self.partial = Some(Partial {
                    label,
                    message_type: mt,
                    signal: SignalId::from_u8(pkt[2]),
                    nosp,
                    received: 1,
                    payload: pkt[3..].to_vec(),
                });
                Ok(None)
            }
            PacketType::Continue | PacketType::End => {
                let p = self.partial.as_mut().ok_or(Error::UnexpectedFragment)?;
                if p.label != label || p.message_type != mt {
                    return Err(Error::UnexpectedFragment);
                }
                p.received = p.received.saturating_add(1);
                p.payload.extend_from_slice(&pkt[1..]);
                let last = pt == PacketType::End;
                if last != (p.received == p.nosp) {
                    return Err(Error::BadPacketCount);
                }
                if last {
                    let p = self.partial.take().ok_or(Error::UnexpectedFragment)?;
                    Ok(Some(Message {
                        label: p.label,
                        message_type: p.message_type,
                        signal: p.signal,
                        payload: p.payload,
                    }))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

/// Transaction label allocator (4-bit, wraps).
#[derive(Debug, Default)]
pub struct Labels(u8);

impl Labels {
    pub fn next_label(&mut self) -> u8 {
        let l = self.0;
        self.0 = (self.0 + 1) & 0x0F;
        l
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::vec;

    #[test]
    fn single_roundtrip() {
        let m = Message::new(
            5,
            MessageType::Command,
            SignalId::GetAllCapabilities,
            vec![0x04],
        );
        let b = m.encode_single();
        assert_eq!(b, vec![0x50, 0x0C, 0x04]);
        let mut r = Reassembler::new();
        assert_eq!(r.push(&b).unwrap(), Some(m));
    }

    #[test]
    fn header_bits() {
        // TL=0xA, Single, ResponseReject, signal OPEN, RFA bits set must be ignored.
        let mut r = Reassembler::new();
        let m = r.push(&[0xA3, 0xC6, 0x12]).unwrap().unwrap();
        assert_eq!(m.label, 0xA);
        assert_eq!(m.message_type, MessageType::ResponseReject);
        assert_eq!(m.signal, SignalId::Open);
        assert_eq!(m.payload, vec![0x12]);
    }

    #[test]
    fn all_signal_ids_roundtrip() {
        for v in 0u8..64 {
            assert_eq!(SignalId::from_u8(v).to_u8(), v);
        }
    }

    #[test]
    fn fragment_and_reassemble() {
        let payload: Vec<u8> = (0..200u16).map(|x| x as u8).collect();
        let m = Message::new(
            3,
            MessageType::ResponseAccept,
            SignalId::GetAllCapabilities,
            payload,
        );
        let pkts = fragment(&m, 48).unwrap();
        assert!(pkts.len() > 2);
        assert!(pkts.iter().all(|p| p.len() <= 48));
        // Start packet: NOSP before signal id.
        assert_eq!(pkts[0][0] & 0x0C, 0x04);
        assert_eq!(pkts[0][1] as usize, pkts.len());
        assert_eq!(pkts[0][2], 0x0C);
        // Continue/End: one header byte, no signal id.
        assert_eq!(pkts[1][0], 0x3A); // TL=3, Continue, Accept
        assert_eq!(pkts.last().unwrap()[0], 0x3E); // End
        let mut r = Reassembler::new();
        for p in &pkts[..pkts.len() - 1] {
            assert_eq!(r.push(p).unwrap(), None);
            assert!(r.in_progress());
        }
        assert_eq!(r.push(pkts.last().unwrap()).unwrap(), Some(m));
        assert!(!r.in_progress());
    }

    #[test]
    fn fragment_fits_single() {
        let m = Message::new(0, MessageType::Command, SignalId::Discover, vec![]);
        assert_eq!(fragment(&m, 48).unwrap(), vec![vec![0x00, 0x01]]);
        assert_eq!(
            fragment(
                &Message::new(0, MessageType::Command, SignalId::Discover, vec![0; 10]),
                2
            ),
            None
        );
    }

    #[test]
    fn reassembly_errors() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&[]), Err(Error::Truncated));
        assert_eq!(r.push(&[0x00]), Err(Error::Truncated)); // single w/o signal id
        assert_eq!(r.push(&[0x08, 1, 2]), Err(Error::UnexpectedFragment)); // continue w/o start
        assert_eq!(r.push(&[0x0C, 1]), Err(Error::UnexpectedFragment)); // end w/o start
        assert_eq!(r.push(&[0x04, 1, 0x0C]), Err(Error::BadPacketCount)); // NOSP < 2
        assert_eq!(r.push(&[0x04, 3]), Err(Error::Truncated));
        // label mismatch
        assert_eq!(r.push(&[0x14, 2, 0x0C, 9]).unwrap(), None);
        assert_eq!(r.push(&[0x2C, 9]), Err(Error::UnexpectedFragment));
        assert!(!r.in_progress());
        // End too early (NOSP=3)
        assert_eq!(r.push(&[0x14, 3, 0x0C]).unwrap(), None);
        assert_eq!(r.push(&[0x1C]), Err(Error::BadPacketCount));
        // Continue where End was due (NOSP=2)
        assert_eq!(r.push(&[0x14, 2, 0x0C]).unwrap(), None);
        assert_eq!(r.push(&[0x18]), Err(Error::BadPacketCount));
        // Single resyncs after a dangling start
        assert_eq!(r.push(&[0x14, 2, 0x0C]).unwrap(), None);
        assert!(r.push(&[0x20, 0x01]).unwrap().is_some());
        assert!(!r.in_progress());
    }

    #[test]
    fn labels_wrap() {
        let mut l = Labels::default();
        for i in 0..16 {
            assert_eq!(l.next_label(), i);
        }
        assert_eq!(l.next_label(), 0);
    }

    #[test]
    fn never_panics_on_garbage() {
        // Cheap deterministic fuzz: every 1..=4 byte packet sequence prefix.
        let mut r = Reassembler::new();
        let mut x: u32 = 0x1234_5678;
        for _ in 0..20000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let len = (x % 7) as usize;
            let bytes = x.to_le_bytes();
            let mut v = Vec::new();
            for i in 0..len {
                v.push(bytes[i % 4].wrapping_add(i as u8));
            }
            let _ = r.push(&v);
        }
    }
}
