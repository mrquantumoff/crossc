// SSC encoder: public API.
//
// API semantics ported from samsung-ssc-encoder include/ssc_encoder.h, Copyright (c) 2026 William Ho (WillyBilly06),
// BSD-3-Clause. See NOTICE.

//! Encoder for the Samsung Scalable Codec / Samsung Seamless Codec (SSC), the Bluetooth A2DP vendor codec
//! (vendor `0x0000_0075`, codec `0x0103`) used by Galaxy phones and Galaxy Buds.
//!
//! This crate is a faithful Rust port of the BSD-3-Clause C encoder
//! [samsung-ssc-encoder](https://github.com/WillyBilly06/samsung-ssc-encoder) (Copyright (c) 2026 William Ho),
//! which in turn adapts the CELT layer of libopus 1.1 (Copyright Xiph.Org Foundation and contributors,
//! BSD-3-Clause). With the default `std` feature its output is byte-identical to the C reference built with MSVC
//! (`/O2 /fp:precise`, x86-64); see the crate README for the test matrix.
//!
//! # Usage
//!
//! ```
//! use ssc_enc::{Bitrate, Encoder, Mode, PcmFrame, FRAME_SAMPLES, MAX_FRAME_BYTES};
//!
//! // The encoder state is ~75 KB. new_boxed builds it directly on the heap (Box::new(Encoder::new(..)) would
//! // build it on the stack first).
//! # #[cfg(not(feature = "alloc"))] let mut enc = Box::new(Encoder::new(Mode::Uhq96k, 2).unwrap());
//! # #[cfg(feature = "alloc")]
//! let mut enc = Encoder::new_boxed(Mode::Uhq96k, 2).unwrap();
//! let pcm = vec![0i32; 2 * FRAME_SAMPLES]; // 24-bit samples in i32, interleaved L R L R ...
//! let mut frame = [0u8; MAX_FRAME_BYTES];
//! let n = enc.encode_frame(PcmFrame::I24(&pcm), Bitrate::K584, &mut frame).unwrap();
//! assert_eq!(n, ssc_enc::frame_size(Mode::Uhq96k, Bitrate::K584));
//! ```
//!
//! # Frame layout
//!
//! One frame carries 864 samples per channel (always coded as two channels) and has a fixed size given by the
//! codec mode and the bitrate ([`frame_size`]). There is no length field on the wire: a receiver derives the size
//! from the header.
//!
//! | Byte | Contents |
//! |---|---|
//! | 0 | `0xFF` (sync) |
//! | 1 | F-family rates and 256k/328k: `0xEE`. E-family rates: `0xE0 \| (seq >> 4)` (high nibble of an 8-bit sequence number) |
//! | 2 | bits 7-4: `seq & 0xF`; bits 3-2: end-band side information (non-hq rates: activity of band 21, 0-3; E-family: number of uncoded top bands, 0-3); bit 0: always 1 |
//! | 3 | rate code (`0xF0`-`0xF6`, `0xE0`-`0xE6`). E-family except 152k: low 5 bits = rate, top 3 bits = (sub-band index - 1) of the last active part of the top coded band |
//! | 4.. | range-coded CELT-style payload (`frame_size - 4` bytes) |
//!
//! The sequence number counts frames modulo 256 (only the low 4 bits are visible for the F-family header).
//! The decoded audio lags the input by 108 samples (the MDCT overlap).
//!
//! # Features
//!
//! * `std` (default): transcendental functions from the platform C runtime (bit-exact with the C build on the
//!   same platform).
//! * `libm`: for `no_std` builds without `std`; transcendental functions from the `libm` crate. The output is
//!   valid SSC but may differ from the C reference in rare last-place rounding cases.
//! * `alloc` (implied by `std`): [`Encoder::new_boxed`].
//!
//! The crate is `no_std`, does not allocate (except the one allocation of [`Encoder::new_boxed`]), and keeps all
//! state (including scratch buffers) in [`Encoder`].
//!
//! # Stack use
//!
//! [`Encoder`] is about 75 KB. [`Encoder::new`] returns it by value, so it and `Box::new(Encoder::new(..))` need
//! several times that much stack (measured on x86-64 Windows: about 330 KiB in a debug build, 200 KiB optimised).
//! [`Encoder::new_boxed`] allocates the encoder pre-zeroed on the heap (measured: under 8 KiB of stack in debug and
//! optimised builds), and [`Encoder::encode_frame`] itself needs little stack (under 8 KiB optimised). On a small-stack
//! thread, use `new_boxed`, or construct the encoder on a thread with a normal stack and move it over.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(feature = "alloc")]
extern crate alloc as alloc_crate;
#[cfg(feature = "std")]
extern crate std;

mod alloc;
mod analysis;
mod bands;
mod consts;
mod ec;
mod encoder;
mod energy;
mod math;
mod mdct;
mod pitch;
mod tables;

pub use encoder::Encoder;

/// Samples per channel per frame.
pub const FRAME_SAMPLES: usize = consts::N;

/// Largest frame in bytes (886 kb/s in the 44.1/48 kHz mode).
pub const MAX_FRAME_BYTES: usize = 2172;

/// Codec mode, as configured over A2DP.
///
/// The bitstream has only two modes: 44.1 and 48 kHz share one (the sample rate is not signalled), and 96 kHz
/// has its own. The variants exist so callers can say what was negotiated; [`Mode::Ssc44k1`], [`Mode::Ssc48k`]
/// and [`Mode::Hifi48k`] encode identically, as do [`Mode::Uhq96k`] and [`Mode::LegacyUhq96k`]. The input
/// sample format is chosen per frame with [`PcmFrame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "alloc", derive(bytemuck::Zeroable))]
#[repr(u8)]
pub enum Mode {
    /// SSC, 44.1 kHz (A2DP flag `0x20`), normally 16-bit input.
    Ssc44k1 = 0,
    /// SSC, 48 kHz (A2DP flag `0x10`), normally 16-bit input.
    Ssc48k,
    /// Seamless HiFi, 48 kHz (A2DP flag `0x04`), normally 24-bit input.
    Hifi48k,
    /// UHQ v2, 96 kHz (A2DP flag `0x02`), normally 24-bit input.
    Uhq96k,
    /// Legacy UHQ, 96 kHz (A2DP flag `0x80`), normally 24-bit input at 328 kb/s.
    LegacyUhq96k,
}

impl Mode {
    /// Sample rate in Hz.
    pub const fn sample_rate(self) -> u32 {
        match self {
            Mode::Ssc44k1 => 44100,
            Mode::Ssc48k | Mode::Hifi48k => 48000,
            Mode::Uhq96k | Mode::LegacyUhq96k => 96000,
        }
    }

    /// Whether this is the 96 kHz codec mode.
    pub const fn is_96k(self) -> bool {
        matches!(self, Mode::Uhq96k | Mode::LegacyUhq96k)
    }
}

/// The 14 SSC bitrates. The discriminant is the header rate code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Bitrate {
    /// 88 kb/s, code `0xF0`.
    K88 = 0xF0,
    /// 96 kb/s, code `0xF1`.
    K96 = 0xF1,
    /// 128 kb/s, code `0xF2`.
    K128 = 0xF2,
    /// 192 kb/s, code `0xF3`.
    K192 = 0xF3,
    /// 229 kb/s, code `0xF4`.
    K229 = 0xF4,
    /// 328 kb/s, code `0xF5` ("hq").
    K328 = 0xF5,
    /// 256 kb/s, code `0xF6` ("hq").
    K256 = 0xF6,
    /// 291 kb/s, code `0xE0` (E-family, "hq").
    K291 = 0xE0,
    /// 308 kb/s, code `0xE1` (E-family, "hq").
    K308 = 0xE1,
    /// 442 kb/s, code `0xE2` (E-family, "hq").
    K442 = 0xE2,
    /// 584 kb/s, code `0xE3` (E-family, "hq").
    K584 = 0xE3,
    /// 886 kb/s, code `0xE4` (E-family, "hq").
    K886 = 0xE4,
    /// 250 kb/s, code `0xE5` (E-family, "hq").
    K250 = 0xE5,
    /// 152 kb/s, code `0xE6` (E-family, "hq", 18 coded bands).
    K152 = 0xE6,
}

impl Bitrate {
    /// All bitrates, in the reference encoder's table order.
    pub const ALL: [Bitrate; 14] = [
        Bitrate::K88,
        Bitrate::K96,
        Bitrate::K128,
        Bitrate::K192,
        Bitrate::K229,
        Bitrate::K328,
        Bitrate::K256,
        Bitrate::K291,
        Bitrate::K308,
        Bitrate::K442,
        Bitrate::K584,
        Bitrate::K886,
        Bitrate::K250,
        Bitrate::K152,
    ];

    /// Header rate code.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Bits per second.
    pub const fn bps(self) -> u32 {
        match self {
            Bitrate::K88 => 88000,
            Bitrate::K96 => 96000,
            Bitrate::K128 => 128000,
            Bitrate::K192 => 192000,
            Bitrate::K229 => 229000,
            Bitrate::K328 => 328000,
            Bitrate::K256 => 256000,
            Bitrate::K291 => 291000,
            Bitrate::K308 => 308000,
            Bitrate::K442 => 442000,
            Bitrate::K584 => 584000,
            Bitrate::K886 => 886000,
            Bitrate::K250 => 250000,
            Bitrate::K152 => 152000,
        }
    }

    /// "hq" rate class (E-family, 256k, 328k): no TF symbols, 24-bit scale.
    pub const fn is_hq(self) -> bool {
        !matches!(self, Bitrate::K88 | Bitrate::K96 | Bitrate::K128 | Bitrate::K192 | Bitrate::K229)
    }

    /// E-family rate (8-bit sequence number, end-band side information in the header).
    pub const fn is_e_family(self) -> bool {
        (self as u8) & 0xF0 == 0xE0
    }

    /// Looks up a header rate code.
    pub const fn from_code(code: u8) -> Option<Bitrate> {
        let mut i = 0;
        while i < 14 {
            if Self::ALL[i].code() == code {
                return Some(Self::ALL[i]);
            }
            i += 1;
        }
        None
    }

    /// Looks up a bitrate in bits per second.
    pub const fn from_bps(bps: u32) -> Option<Bitrate> {
        let mut i = 0;
        while i < 14 {
            if Self::ALL[i].bps() == bps {
                return Some(Self::ALL[i]);
            }
            i += 1;
        }
        None
    }
}

/// Frame size in bytes, header included.
///
/// `base = bps * 864 / (Fs * 8)` with Fs = 44100 for the 44.1/48 kHz mode and 96000 for the 96 kHz mode,
/// `+ (base * 3) >> 7` for the non-hq rates, `base |= 1`, frame = `base + 3`.
pub const fn frame_size(mode: Mode, bitrate: Bitrate) -> usize {
    payload_bytes(mode.is_96k(), bitrate) + 4
}

pub(crate) const fn payload_bytes(is_96k: bool, bitrate: Bitrate) -> usize {
    let fs: i64 = if is_96k { 96000 } else { 44100 };
    let mut base = ((bitrate.bps() as i64 * consts::N as i64) / (fs * 8)) as i32;
    if !bitrate.is_hq() {
        base += (base * 3) >> 7;
    }
    base |= 1;
    (base - 1) as usize
}

/// One frame of interleaved PCM input: `FRAME_SAMPLES * channels` samples (L R L R ... for stereo).
#[derive(Clone, Copy, Debug)]
pub enum PcmFrame<'a> {
    /// 16-bit samples.
    I16(&'a [i16]),
    /// 24-bit samples in `i32`, right-justified (full scale +-2^23), as used for HiFi / UHQ.
    I24(&'a [i32]),
    /// `i32` samples with `bits` significant bits, right-justified (16..=32).
    S32 {
        /// The samples.
        samples: &'a [i32],
        /// Significant bits per sample, 16..=32.
        bits: u32,
    },
    /// Float samples in [-1, 1).
    F32(&'a [f32]),
}

impl PcmFrame<'_> {
    fn len(&self) -> usize {
        match self {
            PcmFrame::I16(s) => s.len(),
            PcmFrame::I24(s) => s.len(),
            PcmFrame::S32 { samples, .. } => samples.len(),
            PcmFrame::F32(s) => s.len(),
        }
    }
}

/// Encoder options (the reference encoder's `SSC_ENC_FLAG_*`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "alloc", derive(bytemuck::Zeroable))]
pub struct Flags(u32);

impl Flags {
    /// No options (the default, and what a normal stream uses).
    pub const NONE: Flags = Flags(0);
    /// Reproduce the level of Samsung's encoder for 16-bit input at the "hq" rates (250k, 256k, 291k, 308k,
    /// 328k, 442k, 584k, 886k): it scales such input as if it were 24-bit, so the coded signal is 48 dB quieter
    /// and the end-band side information differs. Only useful to compare against Samsung's output; only affects
    /// [`PcmFrame::I16`] input.
    pub const SAMSUNG_HQ16_LEVEL: Flags = Flags(0x1);
    /// Disable the transient detector (always long blocks).
    pub const NO_TRANSIENTS: Flags = Flags(0x2);
    /// Disable the pitch prefilter (the frame then signals "postfilter off").
    pub const NO_POSTFILTER: Flags = Flags(0x4);
    /// Do not apply Samsung's fixed high-frequency shaping (up to -1.7 dB above ~6-16 kHz depending on the
    /// rate). The header side information is computed the same way either way.
    pub const FLAT_RESPONSE: Flags = Flags(0x8);

    /// Raw bits (same values as the C `SSC_ENC_FLAG_*` macros).
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Builds flags from the C bit values; unknown bits are dropped.
    pub const fn from_bits_truncate(bits: u32) -> Flags {
        Flags(bits & 0xF)
    }

    /// Whether every flag in `other` is set.
    pub const fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::ops::BitOr for Flags {
    type Output = Flags;
    fn bitor(self, rhs: Flags) -> Flags {
        Flags(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for Flags {
    fn bitor_assign(&mut self, rhs: Flags) {
        self.0 |= rhs.0;
    }
}

/// Errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// `channels` must be 1 (duplicated to both coded channels) or 2.
    InvalidChannels(usize),
    /// The PCM slice does not hold `FRAME_SAMPLES * channels` samples.
    InvalidSampleCount {
        /// Samples required.
        expected: usize,
        /// Samples given.
        got: usize,
    },
    /// [`PcmFrame::S32`] with `bits` outside 16..=32.
    InvalidBitDepth(u32),
    /// The output buffer is smaller than the frame.
    BufferTooSmall {
        /// Bytes required ([`frame_size`]).
        needed: usize,
        /// Bytes given.
        got: usize,
    },
    /// The range coder ran out of space (cannot happen with valid input; mirrors `SSC_ENC_ERR_INTERNAL`).
    Internal,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::InvalidChannels(c) => write!(f, "unsupported channel count {c} (1 or 2)"),
            Error::InvalidSampleCount { expected, got } => {
                write!(f, "expected {expected} interleaved samples, got {got}")
            }
            Error::InvalidBitDepth(b) => write!(f, "unsupported bit depth {b} (16..=32)"),
            Error::BufferTooSmall { needed, got } => write!(f, "output buffer too small: {got} < {needed} bytes"),
            Error::Internal => f.write_str("internal range coder error"),
        }
    }
}

impl core::error::Error for Error {}

/// Details of the last encoded frame (the reference encoder's `ssc_encoder_frame_info`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "alloc", derive(bytemuck::Zeroable))]
pub struct FrameInfo {
    /// Total frame size, header included.
    pub bytes: usize,
    /// Bits per second.
    pub bitrate: u32,
    /// "hq" rate class.
    pub hq: bool,
    /// Sequence number written into the header (0..=255).
    pub seq: u8,
    /// Number of coded bands (18..=22).
    pub end_band: u8,
    /// Short blocks used.
    pub transient: bool,
    /// Pitch postfilter enabled.
    pub pf_on: bool,
    /// Postfilter period in samples (15..=1022), if `pf_on`, else 0.
    pub pf_period: u16,
    /// Postfilter gain index 0..=7 (gain = 0.09375 * (q + 1)), if `pf_on`, else 0.
    pub pf_gain_q: u8,
    /// Silence frame.
    pub silence: bool,
    /// Bands that received PVQ bits.
    pub coded_bands: u8,
    /// Final range-coder position in bits (ec_tell before ec_enc_done).
    pub tell_bits: u32,
    /// Bit budget 8 * min(payload, 1275).
    pub total_bits: u32,
}

impl FrameInfo {
    /// All-zero value (the encoder's initial "no frame yet" state).
    pub(crate) const ZERO: FrameInfo = FrameInfo {
        bytes: 0,
        bitrate: 0,
        hq: false,
        seq: 0,
        end_band: 0,
        transient: false,
        pf_on: false,
        pf_period: 0,
        pf_gain_q: 0,
        silence: false,
        coded_bands: 0,
        tell_bits: 0,
        total_bits: 0,
    };
}
