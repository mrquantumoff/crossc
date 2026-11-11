//! RTP fixed header (RFC 3550, 12 octets, no CSRCs) as used on the AVDTP
//! media transport channel. For A2DP vendor codecs the payload type is
//! typically dynamic (96..=127).

/// RTP header fields (version is always 2, CSRC count always 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtpHeader {
    pub padding: bool,
    pub extension: bool,
    pub marker: bool,
    /// 7-bit payload type.
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl RtpHeader {
    pub const LEN: usize = 12;

    pub fn new(payload_type: u8, sequence: u16, timestamp: u32, ssrc: u32) -> Self {
        Self {
            padding: false,
            extension: false,
            marker: false,
            payload_type: payload_type & 0x7F,
            sequence,
            timestamp,
            ssrc,
        }
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0] = 0x80 | (u8::from(self.padding) << 5) | (u8::from(self.extension) << 4);
        b[1] = (u8::from(self.marker) << 7) | (self.payload_type & 0x7F);
        b[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        b[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        b[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        b
    }

    /// Parse a header; returns it and the CSRC-skipped payload offset.
    pub fn decode(b: &[u8]) -> Option<(Self, usize)> {
        if b.len() < Self::LEN || b[0] >> 6 != 2 {
            return None;
        }
        let cc = (b[0] & 0x0F) as usize;
        let off = Self::LEN + 4 * cc;
        if b.len() < off {
            return None;
        }
        Some((
            Self {
                padding: b[0] & 0x20 != 0,
                extension: b[0] & 0x10 != 0,
                marker: b[1] & 0x80 != 0,
                payload_type: b[1] & 0x7F,
                sequence: u16::from_be_bytes([b[2], b[3]]),
                timestamp: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
                ssrc: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
            },
            off,
        ))
    }

    /// Advance to the next packet: sequence + 1 (wrapping), timestamp + `samples` (wrapping).
    pub fn advance(&mut self, samples: u32) {
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(samples);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_known() {
        let h = RtpHeader::new(96, 0x1234, 0xDEADBEEF, 1);
        assert_eq!(
            h.encode(),
            [0x80, 0x60, 0x12, 0x34, 0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0, 1]
        );
        let mut m = h;
        m.marker = true;
        m.padding = true;
        assert_eq!(m.encode()[..2], [0xA0, 0xE0]);
    }

    #[test]
    fn roundtrip_and_wrap() {
        let mut h = RtpHeader::new(0xFF, 0xFFFF, 0xFFFF_FFF0, 7);
        assert_eq!(h.payload_type, 0x7F);
        let (d, off) = RtpHeader::decode(&h.encode()).unwrap();
        assert_eq!((d, off), (h, 12));
        h.advance(0x20);
        assert_eq!(h.sequence, 0);
        assert_eq!(h.timestamp, 0x10);
    }

    #[test]
    fn decode_rejects() {
        assert!(RtpHeader::decode(&[0x80; 11]).is_none());
        assert!(RtpHeader::decode(&[0x40; 12]).is_none()); // version 1
        let mut b = [0u8; 12];
        b[0] = 0x81; // one CSRC but none present
        assert!(RtpHeader::decode(&b).is_none());
        let mut c = [0u8; 16];
        c[0] = 0x81;
        assert_eq!(RtpHeader::decode(&c).unwrap().1, 16);
    }
}
