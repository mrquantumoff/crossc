//! Audio path contract: how the user-mode service takes the PCM that Windows
//! renders into the driver's ACX endpoint, and how it tells the driver whether
//! the buds are reachable. Added in [`crate::PROTOCOL_VERSION`] 2; a version 1
//! driver fails these codes with `STATUS_INVALID_DEVICE_REQUEST`.
//!
//! Same transport rules as the pipe IOCTLs (`METHOD_BUFFERED`, little-endian
//! layouts with explicit `encode`/`decode`, read + write access, SYSTEM and
//! Administrators only).
//!
//! # Session
//! The service sends [`IOCTL_AUDIO_ATTACH`] once and leaves it pending for its
//! whole life. While it is pending the caller's handle is "attached": only that
//! handle may use the other audio IOCTLs (`STATUS_ACCESS_DENIED` otherwise,
//! except [`IOCTL_AUDIO_GET_STATE`], which any admin handle may call). When the
//! ATTACH request is cancelled or its handle goes away (CancelIoEx, CloseHandle,
//! process exit) the driver detaches by itself: link flag cleared (endpoint
//! shows unplugged), PCM tap off, pending WAIT_EVENT / READ_PCM of that handle
//! cancelled, that handle's L2CAP channels closed. A second ATTACH while one is
//! pending fails with `STATUS_DEVICE_BUSY`.
//!
//! # Statuses and edge cases
//! * No endpoint (ACX disabled in the driver): every audio IOCTL except
//!   GET_STATE fails with `STATUS_NOT_SUPPORTED`; GET_STATE succeeds with
//!   [`AUDIO_FLAG_ACX`] clear.
//! * ATTACH also ends (`STATUS_CANCELLED`) when the device leaves D0 (sleep,
//!   removal, driver update). The service re-attaches once the device is back.
//! * READ_PCM checks its buffer again when it completes: a stream whose frame
//!   does not fit the buffer gives `STATUS_BUFFER_TOO_SMALL` at that point.
//! * The counters are per device object: they restart when the device does.
//!
//! # Rules the driver keeps
//! * The audio engine never waits for the service. With nobody attached, or a
//!   reader that falls behind, rendered PCM is dropped and counted in
//!   [`AudioState::frames_dropped`]; the next chunk the reader gets carries
//!   [`PCM_FLAG_DISCONTINUITY`].
//! * PCM is copied; no memory is shared with user mode.
//! * Every field is validated; bad input is an error status, never a fault.

use crate::{
    FILE_DEVICE_CROSSC, FILE_READ_DATA, FILE_WRITE_DATA, METHOD_BUFFERED, WireError, ctl_code,
};

const RW: u32 = FILE_READ_DATA | FILE_WRITE_DATA;

/// In: nothing. Out: nothing. Pends for the life of the session (see module docs).
pub const IOCTL_AUDIO_ATTACH: u32 = ctl_code(FILE_DEVICE_CROSSC, 0x810, METHOD_BUFFERED, RW);
/// In: [`SetLinkRequest`]. Out: nothing. Attached handle only.
pub const IOCTL_AUDIO_SET_LINK: u32 = ctl_code(FILE_DEVICE_CROSSC, 0x811, METHOD_BUFFERED, RW);
/// In: nothing. Out: [`AudioState`]. Never pends.
pub const IOCTL_AUDIO_GET_STATE: u32 = ctl_code(FILE_DEVICE_CROSSC, 0x812, METHOD_BUFFERED, RW);
/// In: [`WaitEventRequest`]. Out: [`AudioState`]. Completes at once if the
/// driver's [`AudioState::seq`] differs from the caller's, else pends until it
/// changes. One outstanding per handle (`STATUS_DEVICE_BUSY`). Attached handle only.
pub const IOCTL_AUDIO_WAIT_EVENT: u32 = ctl_code(FILE_DEVICE_CROSSC, 0x813, METHOD_BUFFERED, RW);
/// In: nothing. Out: [`PcmHeader`] followed by `frames * frame_bytes()` bytes
/// of interleaved PCM. Pends until at least one frame is available, then
/// returns as many whole frames as fit in the output buffer (never frames of
/// two formats in one reply). The output buffer must hold the header and at
/// least one frame (`STATUS_BUFFER_TOO_SMALL`); [`PCM_READ_BUFFER`] is a good
/// size. One outstanding (`STATUS_DEVICE_BUSY`). Attached handle only.
pub const IOCTL_AUDIO_READ_PCM: u32 = ctl_code(FILE_DEVICE_CROSSC, 0x814, METHOD_BUFFERED, RW);

/// Recommended READ_PCM output buffer: header + 4096 frames of 32-bit stereo.
pub const PCM_READ_BUFFER: usize = PcmHeader::SIZE + 4096 * 8;

/// SET_LINK request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetLinkRequest {
    /// True once the buds have an open, configured stream that audio can be
    /// sent to. Drives the endpoint's jack presence.
    pub connected: bool,
}

impl SetLinkRequest {
    pub const SIZE: usize = 4;
    pub fn encode(&self) -> [u8; Self::SIZE] {
        [u8::from(self.connected), 0, 0, 0]
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < Self::SIZE {
            return Err(WireError::TooShort);
        }
        Ok(Self {
            connected: b[0] != 0,
        })
    }
}

/// WAIT_EVENT request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WaitEventRequest {
    /// The [`AudioState::seq`] the caller last saw (0 = none: completes at once).
    pub seq: u32,
}

impl WaitEventRequest {
    pub const SIZE: usize = 4;
    pub fn encode(&self) -> [u8; Self::SIZE] {
        self.seq.to_le_bytes()
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < Self::SIZE {
            return Err(WireError::TooShort);
        }
        Ok(Self {
            seq: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        })
    }
}

/// State of the endpoint's render stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StreamState {
    /// No stream exists (nothing has the endpoint open).
    #[default]
    None,
    /// A stream exists but is not running (created, paused or stopping).
    Paused,
    /// The audio engine is rendering.
    Running,
}

impl StreamState {
    pub fn to_wire(self) -> u32 {
        match self {
            StreamState::None => 0,
            StreamState::Paused => 1,
            StreamState::Running => 2,
        }
    }
    /// Unknown values decode as [`StreamState::None`].
    pub fn from_wire(v: u32) -> Self {
        match v {
            1 => StreamState::Paused,
            2 => StreamState::Running,
            _ => StreamState::None,
        }
    }
}

/// PCM layout of a stream: integer little-endian samples, interleaved, the
/// valid bits in the most significant bits of each container (as
/// `WAVEFORMATEXTENSIBLE` defines it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PcmFormat {
    /// Frames per second. 0 = no format (no stream yet).
    pub sample_rate: u32,
    pub channels: u8,
    /// Bits per sample slot: 16 or 32.
    pub container_bits: u8,
    /// Significant bits per sample: 16 or 24.
    pub valid_bits: u8,
}

impl PcmFormat {
    /// Bytes per frame (all channels).
    pub fn frame_bytes(&self) -> usize {
        usize::from(self.channels) * usize::from(self.container_bits / 8)
    }
    /// A format the contract allows: mono/stereo, 16-in-16 or 24-in-32, rate > 0.
    pub fn is_valid(&self) -> bool {
        self.sample_rate > 0
            && matches!(self.channels, 1 | 2)
            && matches!(
                (self.container_bits, self.valid_bits),
                (16, 16) | (32, 24) | (32, 32)
            )
    }
}

/// The driver has ACX enabled (an endpoint exists at all).
pub const AUDIO_FLAG_ACX: u32 = 1;
/// An ATTACH request is pending.
pub const AUDIO_FLAG_ATTACHED: u32 = 2;
/// The link flag set by SET_LINK.
pub const AUDIO_FLAG_LINK: u32 = 4;

/// GET_STATE / WAIT_EVENT reply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioState {
    /// Changes (never to 0) whenever `flags`, `stream` or `format` change.
    /// The counters do not bump it.
    pub seq: u32,
    /// `AUDIO_FLAG_*`.
    pub flags: u32,
    pub stream: StreamState,
    /// Format of the current stream; all zero when `stream` is `None`.
    pub format: PcmFormat,
    /// Frames the endpoint has rendered since the driver loaded.
    pub frames_rendered: u64,
    /// Of those, frames that were never handed to a reader.
    pub frames_dropped: u64,
}

impl AudioState {
    pub const SIZE: usize = 40;
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.seq.to_le_bytes());
        b[4..8].copy_from_slice(&self.flags.to_le_bytes());
        b[8..12].copy_from_slice(&self.stream.to_wire().to_le_bytes());
        b[12..16].copy_from_slice(&self.format.sample_rate.to_le_bytes());
        b[16] = self.format.channels;
        b[17] = self.format.container_bits;
        b[18] = self.format.valid_bits;
        // 19..24 reserved
        b[24..32].copy_from_slice(&self.frames_rendered.to_le_bytes());
        b[32..40].copy_from_slice(&self.frames_dropped.to_le_bytes());
        b
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < Self::SIZE {
            return Err(WireError::TooShort);
        }
        Ok(Self {
            seq: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            flags: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            stream: StreamState::from_wire(u32::from_le_bytes([b[8], b[9], b[10], b[11]])),
            format: PcmFormat {
                sample_rate: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
                channels: b[16],
                container_bits: b[17],
                valid_bits: b[18],
            },
            frames_rendered: u64::from_le_bytes([
                b[24], b[25], b[26], b[27], b[28], b[29], b[30], b[31],
            ]),
            frames_dropped: u64::from_le_bytes([
                b[32], b[33], b[34], b[35], b[36], b[37], b[38], b[39],
            ]),
        })
    }
    pub fn attached(&self) -> bool {
        self.flags & AUDIO_FLAG_ATTACHED != 0
    }
    pub fn link(&self) -> bool {
        self.flags & AUDIO_FLAG_LINK != 0
    }
    pub fn acx(&self) -> bool {
        self.flags & AUDIO_FLAG_ACX != 0
    }
}

/// Frames were lost before this chunk (reader too slow, tap was off, the
/// stream restarted or its format changed). `first_frame` shows the gap.
pub const PCM_FLAG_DISCONTINUITY: u32 = 1;

/// Header of a READ_PCM reply; the samples follow it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PcmHeader {
    /// Index of the chunk's first frame in the endpoint's render count
    /// ([`AudioState::frames_rendered`] numbering).
    pub first_frame: u64,
    /// Whole frames in this chunk (>= 1).
    pub frames: u32,
    /// `PCM_FLAG_*`.
    pub flags: u32,
    pub format: PcmFormat,
}

impl PcmHeader {
    pub const SIZE: usize = 24;
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..8].copy_from_slice(&self.first_frame.to_le_bytes());
        b[8..12].copy_from_slice(&self.frames.to_le_bytes());
        b[12..16].copy_from_slice(&self.flags.to_le_bytes());
        b[16..20].copy_from_slice(&self.format.sample_rate.to_le_bytes());
        b[20] = self.format.channels;
        b[21] = self.format.container_bits;
        b[22] = self.format.valid_bits;
        b
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < Self::SIZE {
            return Err(WireError::TooShort);
        }
        Ok(Self {
            first_frame: u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            frames: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            flags: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
            format: PcmFormat {
                sample_rate: u32::from_le_bytes([b[16], b[17], b[18], b[19]]),
                channels: b[20],
                container_bits: b[21],
                valid_bits: b[22],
            },
        })
    }
    /// Bytes of sample data that follow the header.
    pub fn data_bytes(&self) -> usize {
        self.frames as usize * self.format.frame_bytes()
    }
    /// Split a READ_PCM reply into header and sample bytes; `None` if the
    /// reply is shorter than its header says or the format is not valid.
    pub fn split(reply: &[u8]) -> Option<(Self, &[u8])> {
        let h = Self::decode(reply).ok()?;
        if !h.format.is_valid() {
            return None;
        }
        let data = reply.get(Self::SIZE..Self::SIZE.checked_add(h.data_bytes())?)?;
        Some((h, data))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const FMT: PcmFormat = PcmFormat {
        sample_rate: 96000,
        channels: 2,
        container_bits: 32,
        valid_bits: 24,
    };

    #[test]
    fn codes_are_distinct_and_in_the_crossc_range() {
        let codes = [
            crate::IOCTL_GET_INFO,
            crate::IOCTL_OPEN_CHANNEL,
            crate::IOCTL_CLOSE_CHANNEL,
            crate::IOCTL_WRITE,
            crate::IOCTL_READ,
            IOCTL_AUDIO_ATTACH,
            IOCTL_AUDIO_SET_LINK,
            IOCTL_AUDIO_GET_STATE,
            IOCTL_AUDIO_WAIT_EVENT,
            IOCTL_AUDIO_READ_PCM,
        ];
        for (i, a) in codes.iter().enumerate() {
            assert_eq!(a >> 16, FILE_DEVICE_CROSSC);
            assert_eq!(a & 3, METHOD_BUFFERED);
            assert!(codes[i + 1..].iter().all(|b| b != a));
        }
        assert_eq!(IOCTL_AUDIO_ATTACH, 0x8C55_E040);
    }

    #[test]
    fn state_roundtrip() {
        let s = AudioState {
            seq: 7,
            flags: AUDIO_FLAG_ACX | AUDIO_FLAG_ATTACHED | AUDIO_FLAG_LINK,
            stream: StreamState::Running,
            format: FMT,
            frames_rendered: 0x0102_0304_0506_0708,
            frames_dropped: 9,
        };
        let b = s.encode();
        assert_eq!(AudioState::decode(&b), Ok(s));
        assert!(s.attached() && s.link() && s.acx());
        assert_eq!(AudioState::decode(&b[..39]), Err(WireError::TooShort));
        assert_eq!(AudioState::decode(&[0; 40]).unwrap(), AudioState::default());
        assert_eq!(StreamState::from_wire(99), StreamState::None);
    }

    #[test]
    fn requests_roundtrip() {
        assert_eq!(
            SetLinkRequest::decode(&SetLinkRequest { connected: true }.encode()),
            Ok(SetLinkRequest { connected: true })
        );
        assert_eq!(
            SetLinkRequest::decode(&[0; 4]),
            Ok(SetLinkRequest { connected: false })
        );
        assert_eq!(SetLinkRequest::decode(&[1; 3]), Err(WireError::TooShort));
        assert_eq!(
            WaitEventRequest::decode(&WaitEventRequest { seq: 0xAABB_CCDD }.encode()),
            Ok(WaitEventRequest { seq: 0xAABB_CCDD })
        );
        assert_eq!(WaitEventRequest::decode(&[]), Err(WireError::TooShort));
    }

    #[test]
    fn formats() {
        assert_eq!(FMT.frame_bytes(), 8);
        assert!(FMT.is_valid());
        assert!(
            PcmFormat {
                sample_rate: 48000,
                channels: 2,
                container_bits: 16,
                valid_bits: 16
            }
            .is_valid()
        );
        assert!(!PcmFormat::default().is_valid());
        assert!(
            !PcmFormat {
                container_bits: 24,
                valid_bits: 24,
                ..FMT
            }
            .is_valid()
        );
        assert!(!PcmFormat { channels: 6, ..FMT }.is_valid());
    }

    #[test]
    fn pcm_split() {
        let h = PcmHeader {
            first_frame: 1 << 40,
            frames: 3,
            flags: PCM_FLAG_DISCONTINUITY,
            format: FMT,
        };
        let mut reply = Vec::from(h.encode());
        reply.extend_from_slice(&[0x55; 24]);
        let (d, data) = PcmHeader::split(&reply).unwrap();
        assert_eq!(d, h);
        assert_eq!(data.len(), 24);
        // Extra trailing bytes are ignored; a short reply or a bad format is refused.
        reply.push(0);
        assert_eq!(PcmHeader::split(&reply).unwrap().1.len(), 24);
        assert!(PcmHeader::split(&reply[..PcmHeader::SIZE + 23]).is_none());
        assert!(
            PcmHeader::split(
                &PcmHeader {
                    format: PcmFormat::default(),
                    ..h
                }
                .encode()
            )
            .is_none()
        );
        assert!(PcmHeader::split(&[0; 5]).is_none());
        assert!(PCM_READ_BUFFER >= PcmHeader::SIZE + 4096 * FMT.frame_bytes());
    }
}
