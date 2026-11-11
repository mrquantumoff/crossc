//! PCM in the encoder's input formats, and the conversions into them: from
//! raw little-endian sample bytes (WAV data), and from the PCM chunks the
//! driver's endpoint delivers ([`crossc_ioctl::audio`]).

use crossc_ioctl::audio::PcmFormat;
use ssc_enc::PcmFrame;

/// Interleaved samples in one of the encoder input formats.
#[derive(Clone, Debug, PartialEq)]
pub enum FrameBuf {
    I16(Vec<i16>),
    /// 24-bit, right-justified in i32.
    I24(Vec<i32>),
    /// Full 32-bit integer scale.
    S32(Vec<i32>),
    F32(Vec<f32>),
}

impl FrameBuf {
    pub fn pcm(&self) -> PcmFrame<'_> {
        match self {
            FrameBuf::I16(v) => PcmFrame::I16(v),
            FrameBuf::I24(v) => PcmFrame::I24(v),
            FrameBuf::S32(v) => PcmFrame::S32 {
                samples: v,
                bits: 32,
            },
            FrameBuf::F32(v) => PcmFrame::F32(v),
        }
    }
    /// The first `n` samples as encoder input (`None` if fewer are held).
    pub fn head(&self, n: usize) -> Option<PcmFrame<'_>> {
        Some(match self {
            FrameBuf::I16(v) => PcmFrame::I16(v.get(..n)?),
            FrameBuf::I24(v) => PcmFrame::I24(v.get(..n)?),
            FrameBuf::S32(v) => PcmFrame::S32 {
                samples: v.get(..n)?,
                bits: 32,
            },
            FrameBuf::F32(v) => PcmFrame::F32(v.get(..n)?),
        })
    }
    pub fn name(&self) -> &'static str {
        match self {
            FrameBuf::I16(_) => "I16",
            FrameBuf::I24(_) => "I24",
            FrameBuf::S32(_) => "S32{32}",
            FrameBuf::F32(_) => "F32",
        }
    }
    /// Samples held (all channels).
    pub fn len(&self) -> usize {
        match self {
            FrameBuf::I16(v) => v.len(),
            FrameBuf::I24(v) | FrameBuf::S32(v) => v.len(),
            FrameBuf::F32(v) => v.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn clear(&mut self) {
        match self {
            FrameBuf::I16(v) => v.clear(),
            FrameBuf::I24(v) | FrameBuf::S32(v) => v.clear(),
            FrameBuf::F32(v) => v.clear(),
        }
    }
    /// Drop the first `n` samples (all of them if fewer are held).
    pub fn discard(&mut self, n: usize) {
        match self {
            FrameBuf::I16(v) => drop(v.drain(..n.min(v.len()))),
            FrameBuf::I24(v) | FrameBuf::S32(v) => drop(v.drain(..n.min(v.len()))),
            FrameBuf::F32(v) => drop(v.drain(..n.min(v.len()))),
        }
    }
}

// ---------------------------------------------------------------------------
// Raw little-endian sample bytes -> samples (as many as both slices hold).
// ---------------------------------------------------------------------------

pub fn le16_to_i16(raw: &[u8], out: &mut [i16]) {
    for (d, s) in out.iter_mut().zip(raw.as_chunks::<2>().0) {
        *d = i16::from_le_bytes(*s);
    }
}

/// 16-bit samples widened to 24-bit scale (`<< 8`).
pub fn le16_to_i24(raw: &[u8], out: &mut [i32]) {
    for (d, s) in out.iter_mut().zip(raw.as_chunks::<2>().0) {
        *d = i32::from(i16::from_le_bytes(*s)) << 8;
    }
}

/// Packed 3-byte samples, sign-extended.
pub fn le24_to_i24(raw: &[u8], out: &mut [i32]) {
    for (d, s) in out.iter_mut().zip(raw.as_chunks::<3>().0) {
        *d = i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8;
    }
}

pub fn le32_to_s32(raw: &[u8], out: &mut [i32]) {
    for (d, s) in out.iter_mut().zip(raw.as_chunks::<4>().0) {
        *d = i32::from_le_bytes(*s);
    }
}

pub fn le32_to_f32(raw: &[u8], out: &mut [f32]) {
    for (d, s) in out.iter_mut().zip(raw.as_chunks::<4>().0) {
        *d = f32::from_le_bytes(*s);
    }
}

// ---------------------------------------------------------------------------
// Endpoint PCM chunks -> encoder input
// ---------------------------------------------------------------------------

/// The encoder input format a stream is fed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// 16-bit ([`ssc_enc::PcmFrame::I16`]).
    I16,
    /// 24-bit right-justified ([`ssc_enc::PcmFrame::I24`]).
    I24,
    /// Full 32-bit scale ([`ssc_enc::PcmFrame::S32`] with 32 bits).
    S32,
}

impl InputKind {
    /// What a stream of `fmt` is fed as: the 24-bit modes (`hires`: HiFi,
    /// UHQ) take 24-in-32 as I24, 32 valid bits as S32 and 16-bit widened to
    /// I24; plain SSC takes 16-bit (wider input is dithered down).
    pub fn for_format(fmt: PcmFormat, hires: bool) -> Self {
        match (hires, fmt.container_bits, fmt.valid_bits) {
            (false, _, _) => InputKind::I16,
            (true, 32, 32) => InputKind::S32,
            (true, _, _) => InputKind::I24,
        }
    }
    /// An empty buffer of this kind.
    pub fn new_buf(self) -> FrameBuf {
        match self {
            InputKind::I16 => FrameBuf::I16(Vec::new()),
            InputKind::I24 => FrameBuf::I24(Vec::new()),
            InputKind::S32 => FrameBuf::S32(Vec::new()),
        }
    }
}

/// Noise source for TPDF dither (xorshift32; deterministic, so the same
/// input always encodes to the same output).
#[derive(Clone, Debug)]
pub struct Dither(u32);

impl Default for Dither {
    fn default() -> Self {
        Dither(0x9E37_79B9)
    }
}

impl Dither {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
    /// Triangular noise in -255..=255 (1/256 of a 16-bit step), mean 0.
    fn tpdf(&mut self) -> i32 {
        let r = self.next();
        ((r >> 8) & 0xFF) as i32 - ((r >> 16) & 0xFF) as i32
    }
    /// A 24-bit sample to 16 bits with TPDF dither. Samples that already are
    /// 16-bit values (low byte zero) pass unchanged, so 16-bit content and
    /// digital silence stay exact.
    pub fn i24_to_i16(&mut self, x: i32) -> i16 {
        if x & 0xFF == 0 {
            return (x >> 8).clamp(-32768, 32767) as i16;
        }
        ((x + 128 + self.tpdf()) >> 8).clamp(-32768, 32767) as i16
    }
}

const FS24: f64 = 8_388_608.0;

/// A full-scale-1.0 sample as 24-bit integer (rounded, clamped).
pub fn f64_to_i24(x: f64) -> i32 {
    (x * FS24).round().clamp(-FS24, FS24 - 1.0) as i32
}

/// Iterate the samples of a chunk as 32-bit full-scale integers (a 16-bit
/// sample is shifted up), each frame as (left, right); mono is duplicated.
fn frames_i32(data: &[u8], fmt: PcmFormat) -> impl Iterator<Item = (i32, i32)> + '_ {
    let bytes = usize::from(fmt.container_bits / 8).max(1);
    let ch = usize::from(fmt.channels).max(1);
    let sample = move |b: &[u8]| -> i32 {
        match b {
            [a, c] => i32::from(i16::from_le_bytes([*a, *c])) << 16,
            [a, c, d, e] => i32::from_le_bytes([*a, *c, *d, *e]),
            _ => 0,
        }
    };
    data.chunks_exact(bytes * ch).map(move |f| {
        let l = sample(&f[..bytes]);
        let r = if ch >= 2 {
            sample(&f[bytes..2 * bytes])
        } else {
            l
        };
        (l, r)
    })
}

/// Append the frames of an endpoint chunk (`data` in `fmt`: valid bits in the
/// most significant bits of each container) to `out` as interleaved stereo in
/// `out`'s encoder input format:
/// * I24: 24-in-32 by arithmetic `>> 8`, 16-bit widened `<< 8`;
/// * S32: the container as is (16-bit shifted up);
/// * I16: 16-bit as is, wider input dithered down ([`Dither::i24_to_i16`]);
/// * F32: not an endpoint format, nothing is appended.
///
/// Mono is duplicated to both channels. A trailing partial frame is ignored.
pub fn append_chunk(data: &[u8], fmt: PcmFormat, dither: &mut Dither, out: &mut FrameBuf) {
    let it = frames_i32(data, fmt);
    match out {
        FrameBuf::I24(v) => v.extend(it.flat_map(|(l, r)| [l >> 8, r >> 8])),
        FrameBuf::S32(v) => v.extend(it.flat_map(|(l, r)| [l, r])),
        FrameBuf::I16(v) => {
            if fmt.container_bits == 16 {
                v.extend(it.flat_map(|(l, r)| [(l >> 16) as i16, (r >> 16) as i16]));
            } else {
                for (l, r) in it {
                    v.push(dither.i24_to_i16(l >> 8));
                    v.push(dither.i24_to_i16(r >> 8));
                }
            }
        }
        FrameBuf::F32(_) => {}
    }
}

/// Append the frames of an endpoint chunk to `out` as interleaved stereo
/// f64, full scale 1.0 (resampler input). Mono is duplicated.
pub fn append_chunk_f64(data: &[u8], fmt: PcmFormat, out: &mut Vec<f64>) {
    const FS32: f64 = 2_147_483_648.0;
    out.extend(frames_i32(data, fmt).flat_map(|(l, r)| [f64::from(l) / FS32, f64::from(r) / FS32]));
}

/// Append full-scale-1.0 samples (resampler output, interleaved stereo) to
/// `out` in its encoder input format (I16 dithered from 24-bit).
pub fn append_f64(x: &[f64], dither: &mut Dither, out: &mut FrameBuf) {
    match out {
        FrameBuf::I24(v) => v.extend(x.iter().map(|&s| f64_to_i24(s))),
        FrameBuf::S32(v) => v.extend(x.iter().map(|&s| {
            (s * 2_147_483_648.0)
                .round()
                .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
        })),
        FrameBuf::I16(v) => v.extend(x.iter().map(|&s| dither.i24_to_i16(f64_to_i24(s)))),
        FrameBuf::F32(v) => v.extend(x.iter().map(|&s| s as f32)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F24: PcmFormat = PcmFormat {
        sample_rate: 48000,
        channels: 2,
        container_bits: 32,
        valid_bits: 24,
    };
    const F32V: PcmFormat = PcmFormat {
        valid_bits: 32,
        ..F24
    };
    const F16: PcmFormat = PcmFormat {
        sample_rate: 48000,
        channels: 2,
        container_bits: 16,
        valid_bits: 16,
    };

    fn bytes32(v: &[i32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }
    fn bytes16(v: &[i16]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn input_kind_per_contract() {
        assert_eq!(InputKind::for_format(F24, true), InputKind::I24);
        assert_eq!(InputKind::for_format(F32V, true), InputKind::S32);
        assert_eq!(InputKind::for_format(F16, true), InputKind::I24);
        for f in [F24, F32V, F16] {
            assert_eq!(InputKind::for_format(f, false), InputKind::I16);
        }
        assert_eq!(InputKind::I24.new_buf().name(), "I24");
        assert_eq!(InputKind::S32.new_buf().name(), "S32{32}");
        assert_eq!(InputKind::I16.new_buf().name(), "I16");
    }

    #[test]
    fn i24_from_24_in_32_is_an_arithmetic_shift() {
        // Valid bits in the MSBs: 0x7FFFFF00 is +full scale, 0x80000000 is -full scale.
        let raw = bytes32(&[0x7FFF_FF00, i32::MIN, -256, 0x0000_0100]);
        let mut out = FrameBuf::I24(Vec::new());
        append_chunk(&raw, F24, &mut Dither::default(), &mut out);
        assert_eq!(out, FrameBuf::I24(vec![0x7F_FFFF, -0x80_0000, -1, 1]));
        // Garbage below the valid bits is shifted out.
        let mut out = FrameBuf::I24(Vec::new());
        append_chunk(
            &bytes32(&[0x1234_56FF, -1]),
            F24,
            &mut Dither::default(),
            &mut out,
        );
        assert_eq!(out, FrameBuf::I24(vec![0x12_3456, -1]));
    }

    #[test]
    fn s32_for_32_valid_bits_and_widened_16() {
        let raw = bytes32(&[i32::MAX, i32::MIN]);
        let mut out = FrameBuf::S32(Vec::new());
        append_chunk(&raw, F32V, &mut Dither::default(), &mut out);
        assert_eq!(out, FrameBuf::S32(vec![i32::MAX, i32::MIN]));
        assert!(matches!(
            out.pcm(),
            PcmFrame::S32 {
                bits: 32,
                samples: _
            }
        ));
        // 16-bit into a 24-bit mode: << 8.
        let raw = bytes16(&[i16::MAX, i16::MIN, 1, -1]);
        let mut out = FrameBuf::I24(Vec::new());
        append_chunk(&raw, F16, &mut Dither::default(), &mut out);
        assert_eq!(
            out,
            FrameBuf::I24(vec![0x7F_FF00, -0x80_0000, 0x100, -0x100])
        );
        // 16-bit into plain SSC: as is.
        let mut out = FrameBuf::I16(Vec::new());
        append_chunk(&raw, F16, &mut Dither::default(), &mut out);
        assert_eq!(out, FrameBuf::I16(vec![i16::MAX, i16::MIN, 1, -1]));
    }

    #[test]
    fn mono_is_duplicated_and_partial_frames_are_ignored() {
        let mono24 = PcmFormat { channels: 1, ..F24 };
        let mut raw = bytes32(&[0x100, 0x200]);
        raw.extend_from_slice(&[1, 2, 3]); // not a whole frame
        let mut out = FrameBuf::I24(Vec::new());
        append_chunk(&raw, mono24, &mut Dither::default(), &mut out);
        assert_eq!(out, FrameBuf::I24(vec![1, 1, 2, 2]));
        let mono16 = PcmFormat { channels: 1, ..F16 };
        let mut out = FrameBuf::I16(Vec::new());
        append_chunk(&bytes16(&[7, -7]), mono16, &mut Dither::default(), &mut out);
        assert_eq!(out, FrameBuf::I16(vec![7, 7, -7, -7]));
        let mut f = Vec::new();
        append_chunk_f64(&bytes16(&[i16::MIN, 16384]), mono16, &mut f);
        assert_eq!(f, vec![-1.0, -1.0, 0.5, 0.5]);
    }

    #[test]
    fn dither_24_to_16() {
        let mut d = Dither::default();
        // 16-bit values and silence pass exactly.
        for x in [0, 256, -256, 0x7FFF00, -0x800000, 12800] {
            assert_eq!(i32::from(d.i24_to_i16(x)), x >> 8);
        }
        // A level between two 16-bit steps: every output within one step of
        // it, both neighbours used, and the mean is the true level.
        for (x, want) in [(1000 * 256 + 64, 1000.25), (-5 * 256 - 128, -5.5)] {
            let n = 200_000;
            let mut sum = 0i64;
            let (mut lo, mut hi) = (i16::MAX, i16::MIN);
            for _ in 0..n {
                let y = d.i24_to_i16(x);
                sum += i64::from(y);
                lo = lo.min(y);
                hi = hi.max(y);
            }
            let mean = sum as f64 / f64::from(n);
            assert!((mean - want).abs() < 0.01, "mean {mean} want {want}");
            assert!(f64::from(lo) >= want - 1.5 && f64::from(hi) <= want + 1.5);
            assert!(hi > lo, "the noise toggles the last bit");
        }
        // Clamped at the rails.
        assert_eq!(d.i24_to_i16(0x7F_FFFF), i16::MAX);
        assert_eq!(d.i24_to_i16(-0x7F_FFFF - 1), i16::MIN);
        // Deterministic.
        let run = || {
            let mut d = Dither::default();
            (0..64)
                .map(|i| d.i24_to_i16(i * 37 + 1))
                .collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn plain_ssc_dithers_wide_input() {
        // 24-in-32 at exactly 16-bit values: unchanged.
        let raw = bytes32(&[0x0001_0000, -0x0001_0000, 0x7FFF_0000, i32::MIN]);
        let mut out = FrameBuf::I16(Vec::new());
        append_chunk(&raw, F24, &mut Dither::default(), &mut out);
        assert_eq!(out, FrameBuf::I16(vec![1, -1, i16::MAX, i16::MIN]));
        // In between: within one step.
        let raw = bytes32(&[0x0001_8000; 64]);
        let mut out = FrameBuf::I16(Vec::new());
        append_chunk(&raw, F24, &mut Dither::default(), &mut out);
        let FrameBuf::I16(v) = &out else { panic!() };
        assert_eq!(v.len(), 64);
        assert!(v.iter().all(|&y| (0..=3).contains(&y)), "{v:?}");
    }

    #[test]
    fn f64_output_formats() {
        let x = [0.5, -1.0, 1.0, 0.0];
        let mut out = FrameBuf::I24(Vec::new());
        append_f64(&x, &mut Dither::default(), &mut out);
        assert_eq!(
            out,
            FrameBuf::I24(vec![0x40_0000, -0x80_0000, 0x7F_FFFF, 0])
        );
        let mut out = FrameBuf::I16(Vec::new());
        append_f64(&x, &mut Dither::default(), &mut out);
        assert_eq!(out, FrameBuf::I16(vec![16384, i16::MIN, i16::MAX, 0]));
        let mut out = FrameBuf::S32(Vec::new());
        append_f64(&x, &mut Dither::default(), &mut out);
        assert_eq!(out, FrameBuf::S32(vec![1 << 30, i32::MIN, i32::MAX, 0]));
    }

    #[test]
    fn raw_sample_bytes() {
        let mut a = [0i16; 2];
        le16_to_i16(&[0xFF, 0x7F, 0x00, 0x80], &mut a);
        assert_eq!(a, [i16::MAX, i16::MIN]);
        let mut b = [0i32; 3];
        le16_to_i24(&[0xFF, 0x7F, 0x00, 0x80], &mut b);
        assert_eq!(b, [0x7FFF00, -0x800000, 0]);
        le24_to_i24(&[0xFF, 0xFF, 0x7F, 0x00, 0x00, 0x80], &mut b);
        assert_eq!(b, [0x7F_FFFF, -0x80_0000, 0]);
        le32_to_s32(&[0, 0xFF, 0xFF, 0x7F, 0, 0, 0, 0x80], &mut b);
        assert_eq!(b, [0x7FFF_FF00, i32::MIN, 0]);
        let mut f = [0f32; 1];
        le32_to_f32(&1.0f32.to_le_bytes(), &mut f);
        assert_eq!(f, [1.0]);
    }

    #[test]
    fn buffer_bookkeeping() {
        let mut b = FrameBuf::I24(vec![1, 2, 3, 4, 5]);
        assert_eq!((b.len(), b.is_empty()), (5, false));
        assert!(matches!(b.head(4), Some(PcmFrame::I24(s)) if s == [1, 2, 3, 4]));
        assert!(b.head(6).is_none());
        b.discard(4);
        assert_eq!(b, FrameBuf::I24(vec![5]));
        b.discard(9);
        assert!(b.is_empty());
        let mut b = FrameBuf::I16(vec![1]);
        b.clear();
        assert!(b.is_empty());
    }
}
