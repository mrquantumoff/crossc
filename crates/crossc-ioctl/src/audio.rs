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
//! # Volume (protocol 3)
//! The endpoint can carry a hardware volume (an ACX volume and mute element)
//! whose level the inbox AVRCP stack sends to the buds as absolute volume
//! (see `docs/DESIGN.md`, "Volume"). [`AudioState`] then reports it:
//! [`AUDIO_FLAG_HW_VOLUME`] (Windows no longer scales the PCM itself),
//! [`AUDIO_FLAG_AVRCP`] (the buds apply that volume: stream unscaled),
//! [`AUDIO_FLAG_MUTED`] and the levels in [`VolumeState`]. With `HW_VOLUME`
//! set and `AVRCP` clear nobody applies the volume, so the service must
//! (software gain); with `HW_VOLUME` clear Windows already has. A change of
//! the volume flags or levels bumps [`AudioState::seq`]; the AVRCP request
//! counters do not.
//!
//! The reply grew from [`AudioState::SIZE_V2`] to [`AudioState::SIZE`] bytes
//! behind the v2 fields. A v3 driver accepts an output buffer of at least
//! `SIZE_V2` bytes and fills as much of `SIZE` as fits, so a v2 caller still
//! works; [`AudioState::decode`] accepts a `SIZE_V2` reply (a v2 driver) and
//! reads the volume fields as zero, which means "no hardware volume".
//!
//! # Default format (protocol 4)
//! The endpoint offers the formats of [`OFFERED_FORMATS`]. Windows plays at
//! the *default* one: ACX answers `KSPROPERTY_PIN_PROPOSEDATAFORMAT2` with
//! the default of the host pin's mode list, and the audio engine then uses
//! that format whatever the user picked in the sound settings. So the
//! service makes the default follow that choice with
//! [`IOCTL_AUDIO_SET_DEFAULT_FORMAT`] (any admin handle, attached or not;
//! `STATUS_INVALID_PARAMETER` for a format not in the list,
//! `STATUS_NOT_SUPPORTED` with ACX off). The driver stores the choice in the
//! device's hardware key (`DefaultSampleRate`, `DefaultBits`), reads it
//! back whenever it builds the endpoint, and reports the default in
//! [`AudioState::default_format`] (a change bumps [`AudioState::seq`]). A
//! stream that is already open keeps its format; the new default is what
//! the next stream the engine opens gets.
//!
//! The reply grew from [`AudioState::SIZE_V3`] to [`AudioState::SIZE`] bytes;
//! a v4 driver fills as much as the caller's buffer holds, and
//! [`AudioState::decode`] reads a shorter reply's default format as all
//! zero ("unknown": a v3 or older driver).
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
/// Protocol 4. In: [`SetDefaultFormatRequest`]. Out: nothing. Any admin
/// handle; never pends. See the module docs, "Default format".
pub const IOCTL_AUDIO_SET_DEFAULT_FORMAT: u32 =
    ctl_code(FILE_DEVICE_CROSSC, 0x815, METHOD_BUFFERED, RW);

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

/// The formats the endpoint offers, in the driver's order. The first is the
/// default when nothing else was chosen (and the fallback for a stored
/// choice the driver does not know).
pub const OFFERED_FORMATS: [PcmFormat; 3] = [
    // HiFi
    PcmFormat {
        sample_rate: 48_000,
        channels: 2,
        container_bits: 32,
        valid_bits: 24,
    },
    // UHQ
    PcmFormat {
        sample_rate: 96_000,
        channels: 2,
        container_bits: 32,
        valid_bits: 24,
    },
    // 16-bit
    PcmFormat {
        sample_rate: 48_000,
        channels: 2,
        container_bits: 16,
        valid_bits: 16,
    },
];

/// Index in [`OFFERED_FORMATS`] of exactly `f`.
pub fn offered_index(f: &PcmFormat) -> Option<usize> {
    OFFERED_FORMATS.iter().position(|o| o == f)
}

/// Index in [`OFFERED_FORMATS`] of the format for a sample rate and a bit
/// depth (valid bits), as the sound settings name a format and as the
/// driver stores its default: the format of that rate and depth, else the
/// first one of that rate (only 24-bit exists at 96 kHz), else `None`.
/// `bits` 0 means "not given".
pub fn offered_for(rate: u32, bits: u32) -> Option<usize> {
    let depth = |o: &PcmFormat| match bits {
        0 => true,
        b if b >= 24 => o.valid_bits == 24,
        b => u32::from(o.valid_bits) == b,
    };
    OFFERED_FORMATS
        .iter()
        .position(|o| o.sample_rate == rate && depth(o))
        .or_else(|| OFFERED_FORMATS.iter().position(|o| o.sample_rate == rate))
}

/// SET_DEFAULT_FORMAT request (protocol 4): one of [`OFFERED_FORMATS`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetDefaultFormatRequest {
    pub format: PcmFormat,
}

impl SetDefaultFormatRequest {
    pub const SIZE: usize = 8;
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.format.sample_rate.to_le_bytes());
        b[4] = self.format.channels;
        b[5] = self.format.container_bits;
        b[6] = self.format.valid_bits;
        // 7 reserved
        b
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < Self::SIZE {
            return Err(WireError::TooShort);
        }
        Ok(Self {
            format: PcmFormat {
                sample_rate: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                channels: b[4],
                container_bits: b[5],
                valid_bits: b[6],
            },
        })
    }
}

/// The driver has ACX enabled (an endpoint exists at all).
pub const AUDIO_FLAG_ACX: u32 = 1;
/// An ATTACH request is pending.
pub const AUDIO_FLAG_ATTACHED: u32 = 2;
/// The link flag set by SET_LINK.
pub const AUDIO_FLAG_LINK: u32 = 4;
/// Protocol 3: the endpoint has a hardware volume (and mute) element, so
/// Windows does not scale the PCM. Clear: no such element (v2 driver, the
/// `DisableAvrcpVolume` kill switch, or the element could not be created);
/// Windows has applied the volume before the PCM reaches the tap.
pub const AUDIO_FLAG_HW_VOLUME: u32 = 8;
/// Protocol 3: hardware volume is active: the inbox AVRCP stack holds the
/// volume event and answers it, so the buds apply [`VolumeState::level`].
/// Stream the PCM unscaled. Clear while `HW_VOLUME` is set: apply the level
/// (and mute) in software.
pub const AUDIO_FLAG_AVRCP: u32 = 16;
/// Protocol 3: the endpoint is muted (mute element). The buds are sent the
/// minimum level; the PCM should be silenced as well.
pub const AUDIO_FLAG_MUTED: u32 = 32;
/// Protocol 3, diagnostics: the AVRCP volume event is enabled (a subscriber
/// exists), whether or not it answers ([`AUDIO_FLAG_AVRCP`] decides).
pub const AUDIO_FLAG_AVRCP_SUBSCRIBED: u32 = 64;
/// Every flag the volume state owns.
pub const AUDIO_FLAGS_VOLUME: u32 =
    AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP | AUDIO_FLAG_MUTED | AUDIO_FLAG_AVRCP_SUBSCRIBED;

/// Lowest hardware volume level, 1/65536 dB: -63.5 dB (`0xFFC08000`), the
/// range the inbox A2DP driver exposes.
pub const VOLUME_LEVEL_MIN: i32 = -4_161_536;
/// Highest hardware volume level: 0 dB.
pub const VOLUME_LEVEL_MAX: i32 = 0;

/// Protocol 3 part of [`AudioState`]. All zero when there is no hardware
/// volume. Levels are in 1/65536 dB within
/// [`VOLUME_LEVEL_MIN`]..=[`VOLUME_LEVEL_MAX`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VolumeState {
    /// The level for the buds (the unmuted value while muted). Normally the
    /// louder channel; right after the buds reported a level it is that
    /// level exactly, while the channels hold Windows' write-back of it
    /// rounded to the 0.5 dB grid.
    pub level: i32,
    /// Per-channel levels as Windows set them (balance).
    pub left: i32,
    pub right: i32,
    /// The AVRCP absolute volume (0..=127) the buds are sent for the
    /// current state (0 while muted).
    pub avrcp: u8,
    /// Diagnostics, wrapping: AVRCP-side property GETs, property SETs (the
    /// buds' level), and volume events the driver generated.
    pub gets: u16,
    pub sets: u16,
    pub events: u16,
}

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
    /// Protocol 3 (zero from a v2 driver).
    pub volume: VolumeState,
    /// Protocol 4: the format the endpoint offers as its default, which is
    /// the one Windows opens its next stream in (module docs, "Default
    /// format"). All zero from a v3 driver: unknown.
    pub default_format: PcmFormat,
}

impl AudioState {
    /// Size of the reply in protocol 2, and the smallest output buffer a v3
    /// or v4 driver accepts.
    pub const SIZE_V2: usize = 40;
    /// Size of the protocol 3 reply.
    pub const SIZE_V3: usize = 64;
    /// Size of the full protocol 4 reply.
    pub const SIZE: usize = 72;
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
        // protocol 3
        let v = &self.volume;
        b[40..44].copy_from_slice(&v.level.to_le_bytes());
        b[44..48].copy_from_slice(&v.left.to_le_bytes());
        b[48..52].copy_from_slice(&v.right.to_le_bytes());
        b[52] = v.avrcp;
        // 53..56 reserved
        b[56..58].copy_from_slice(&v.gets.to_le_bytes());
        b[58..60].copy_from_slice(&v.sets.to_le_bytes());
        b[60..62].copy_from_slice(&v.events.to_le_bytes());
        // 62..64 reserved
        // protocol 4
        let d = &self.default_format;
        b[64..68].copy_from_slice(&d.sample_rate.to_le_bytes());
        b[68] = d.channels;
        b[69] = d.container_bits;
        b[70] = d.valid_bits;
        // 71 reserved
        b
    }
    /// Accepts a v2 reply ([`Self::SIZE_V2`] bytes: volume all zero) or a v3
    /// one ([`Self::SIZE_V3`]: default format all zero) as well as a longer
    /// one (bytes past [`Self::SIZE`] are ignored).
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < Self::SIZE_V2 {
            return Err(WireError::TooShort);
        }
        let i32_at = |o: usize| {
            b.get(o..o + 4)
                .map_or(0, |x| i32::from_le_bytes([x[0], x[1], x[2], x[3]]))
        };
        let u16_at = |o: usize| {
            b.get(o..o + 2)
                .map_or(0, |x| u16::from_le_bytes([x[0], x[1]]))
        };
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
            volume: VolumeState {
                level: i32_at(40),
                left: i32_at(44),
                right: i32_at(48),
                avrcp: b.get(52).copied().unwrap_or(0),
                gets: u16_at(56),
                sets: u16_at(58),
                events: u16_at(60),
            },
            default_format: PcmFormat {
                sample_rate: i32_at(64) as u32,
                channels: b.get(68).copied().unwrap_or(0),
                container_bits: b.get(69).copied().unwrap_or(0),
                valid_bits: b.get(70).copied().unwrap_or(0),
            },
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
    /// The endpoint has a hardware volume element (protocol 3).
    pub fn hw_volume(&self) -> bool {
        self.flags & AUDIO_FLAG_HW_VOLUME != 0
    }
    /// The buds apply the volume (protocol 3).
    pub fn avrcp_volume(&self) -> bool {
        self.flags & AUDIO_FLAG_AVRCP != 0
    }
    pub fn muted(&self) -> bool {
        self.flags & AUDIO_FLAG_MUTED != 0
    }
    pub fn avrcp_subscribed(&self) -> bool {
        self.flags & AUDIO_FLAG_AVRCP_SUBSCRIBED != 0
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
    const FMT_48K: PcmFormat = PcmFormat {
        sample_rate: 48000,
        ..FMT
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
            IOCTL_AUDIO_SET_DEFAULT_FORMAT,
        ];
        for (i, a) in codes.iter().enumerate() {
            assert_eq!(a >> 16, FILE_DEVICE_CROSSC);
            assert_eq!(a & 3, METHOD_BUFFERED);
            assert!(codes[i + 1..].iter().all(|b| b != a));
        }
        assert_eq!(IOCTL_AUDIO_ATTACH, 0x8C55_E040);
        assert_eq!(IOCTL_AUDIO_SET_DEFAULT_FORMAT, 0x8C55_E054);
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
            volume: VolumeState::default(),
            default_format: OFFERED_FORMATS[0],
        };
        let b = s.encode();
        assert_eq!(AudioState::decode(&b), Ok(s));
        assert!(s.attached() && s.link() && s.acx());
        assert!(!s.hw_volume() && !s.avrcp_volume() && !s.muted());
        assert_eq!(AudioState::decode(&b[..39]), Err(WireError::TooShort));
        assert_eq!(AudioState::decode(&[0; 40]).unwrap(), AudioState::default());
        assert_eq!(StreamState::from_wire(99), StreamState::None);
    }

    #[test]
    fn v3_state_roundtrip_and_v2_compatibility() {
        let s = AudioState {
            seq: 3,
            flags: AUDIO_FLAG_ACX | AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP | AUDIO_FLAG_MUTED,
            stream: StreamState::Paused,
            format: FMT,
            frames_rendered: 5,
            frames_dropped: 1,
            volume: VolumeState {
                level: -667_751,
                left: -700_000,
                right: -667_751,
                avrcp: 64,
                gets: 0xFFFE,
                sets: 2,
                events: 0x1234,
            },
            default_format: FMT,
        };
        let b = s.encode();
        assert_eq!(b.len(), AudioState::SIZE);
        assert_eq!(AudioState::decode(&b), Ok(s));
        assert!(s.hw_volume() && s.avrcp_volume() && s.muted() && !s.avrcp_subscribed());
        assert_eq!(VOLUME_LEVEL_MIN as u32, 0xFFC0_8000);
        // The v2 bytes are exactly what protocol 2 sent: a v2 caller that
        // gets only the first 40 bytes sees the v2 part of the same state.
        let v2 = AudioState::decode(&b[..AudioState::SIZE_V2]).unwrap();
        assert_eq!(
            v2,
            AudioState {
                volume: VolumeState::default(),
                default_format: PcmFormat::default(),
                ..s
            }
        );
        // Likewise a v3 caller's 64 bytes: everything but the default format.
        let v3 = AudioState::decode(&b[..AudioState::SIZE_V3]).unwrap();
        assert_eq!(
            v3,
            AudioState {
                default_format: PcmFormat::default(),
                ..s
            }
        );
        // A v2 driver's 40-byte reply: no hardware volume.
        let old = AudioState {
            volume: VolumeState::default(),
            flags: AUDIO_FLAG_ACX,
            ..s
        };
        let d = AudioState::decode(&old.encode()[..40]).unwrap();
        assert!(!d.hw_volume() && d.volume == VolumeState::default());
        // A reply cut inside the volume part reads what is there, the rest 0.
        let part = AudioState::decode(&b[..46]).unwrap();
        assert_eq!((part.volume.level, part.volume.left), (-667_751, 0));
        // Longer replies (a later version) are accepted.
        let mut long = [0u8; 80];
        long[..AudioState::SIZE].copy_from_slice(&b);
        assert_eq!(AudioState::decode(&long), Ok(s));
        // The volume flags are disjoint from the v2 ones.
        assert_eq!(
            AUDIO_FLAGS_VOLUME & (AUDIO_FLAG_ACX | AUDIO_FLAG_ATTACHED | AUDIO_FLAG_LINK),
            0
        );
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
        let r = SetDefaultFormatRequest {
            format: OFFERED_FORMATS[1],
        };
        assert_eq!(
            r.encode(),
            [0x00, 0x77, 0x01, 0x00, 2, 32, 24, 0],
            "96000 Hz, stereo, 24 in 32"
        );
        assert_eq!(SetDefaultFormatRequest::decode(&r.encode()), Ok(r));
        assert_eq!(
            SetDefaultFormatRequest::decode(&[0; 7]),
            Err(WireError::TooShort)
        );
    }

    #[test]
    fn offered_formats_and_their_names() {
        assert!(OFFERED_FORMATS.iter().all(PcmFormat::is_valid));
        assert_eq!(OFFERED_FORMATS[0], FMT_48K);
        assert_eq!(offered_index(&FMT), Some(1));
        assert_eq!(offered_index(&PcmFormat { channels: 1, ..FMT }), None);
        // As the sound settings name them.
        assert_eq!(offered_for(96_000, 24), Some(1));
        assert_eq!(offered_for(48_000, 24), Some(0));
        assert_eq!(offered_for(48_000, 16), Some(2));
        // 32-bit (float) is taken as 24, and 96 kHz has 24-bit only.
        assert_eq!(offered_for(48_000, 32), Some(0));
        assert_eq!(offered_for(96_000, 16), Some(1));
        // No depth given: the first of that rate.
        assert_eq!(offered_for(48_000, 0), Some(0));
        assert_eq!(offered_for(96_000, 0), Some(1));
        // A rate the endpoint does not offer.
        assert_eq!(offered_for(44_100, 16), None);
        assert_eq!(offered_for(0, 0), None);
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
