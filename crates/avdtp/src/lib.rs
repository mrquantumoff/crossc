//! AVDTP 1.3 signaling (Bluetooth SIG "Audio/Video Distribution Transport
//! Protocol" v1.3) plus the A2DP codec-element formats needed to talk to a sink.
//!
//! * [`signal`]: packet header, message types, signal ids, fragmentation and
//!   reassembly ([`signal::Reassembler`], [`signal::fragment`]).
//! * [`msg`]: builders for every command used by a source and parsers for
//!   their responses (DISCOVER .. DELAYREPORT), including per-signal reject
//!   layouts.
//! * [`caps`]: service capability TLVs (all 8 categories), encode + decode.
//! * [`codec`]: media codec elements: SBC, MPEG-2/4 AAC, vendor (0xFF) and a
//!   typed Samsung Scalable Codec (SSC) decoder.
//! * [`rtp`]: 12-byte RTP header for the media channel.
//!
//! Everything here is `no_std` + `alloc`, never panics on input, and returns
//! [`Error`] for malformed data.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod caps;
pub mod codec;
pub mod msg;
pub mod rtp;
pub mod signal;

/// Decoding errors. Encoding never fails except where documented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Input ended before a required field.
    Truncated,
    /// Packet type / message type combination not valid here.
    BadHeader,
    /// A continue/end packet arrived without a start packet, or with a
    /// different transaction label / too many packets.
    UnexpectedFragment,
    /// Start packet announced fewer than 2 packets, or the end packet came
    /// before/after the announced count.
    BadPacketCount,
    /// A service capability's LOSC overruns the buffer, or a fixed-length
    /// capability has the wrong length.
    BadCapabilityLength { category: u8 },
    /// SEID out of range (valid: 0x01..=0x3E).
    BadSeid(u8),
    /// Trailing bytes / wrong overall length for this message.
    BadLength,
    /// Response does not match the signal that was asked for.
    WrongSignal,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Truncated => f.write_str("truncated"),
            Error::BadHeader => f.write_str("bad header"),
            Error::UnexpectedFragment => f.write_str("unexpected fragment"),
            Error::BadPacketCount => f.write_str("bad packet count"),
            Error::BadCapabilityLength { category } => {
                write!(f, "bad length for capability category {category:#04x}")
            }
            Error::BadSeid(s) => write!(f, "bad SEID {s:#04x}"),
            Error::BadLength => f.write_str("bad message length"),
            Error::WrongSignal => f.write_str("response for a different signal"),
        }
    }
}

/// L2CAP PSM for AVDTP (signaling and transport channels).
pub const PSM_AVDTP: u16 = 0x0019;
