//! PCM sources for `play`: a sine tone, a log sweep, or a WAV file, delivered
//! one SSC frame (864 samples per channel) at a time in the encoder input
//! format that keeps the source's full precision.

pub use crossc_core::pcm::FrameBuf;
use crossc_core::pcm::{le16_to_i16, le16_to_i24, le24_to_i24, le32_to_f32, le32_to_s32};
use ssc_enc::FRAME_SAMPLES;

use crate::wav::{SampleFormat, Wav};

const FS24: f64 = 8_388_608.0;
const FS16: f64 = 32_768.0;

fn q24(x: f64) -> i32 {
    (x * FS24).round().clamp(-FS24, FS24 - 1.0) as i32
}
fn q16(x: f64) -> i16 {
    (x * FS16).round().clamp(-FS16, FS16 - 1.0) as i16
}

/// Where the audio comes from.
pub enum Source {
    /// Stereo sine; `bits24` picks I24 (HiFi/UHQ) or I16 (plain SSC).
    Tone {
        freq: f64,
        amp: f64,
        rate: u32,
        bits24: bool,
        n: u64,
    },
    /// Stereo log sweep `f0` -> `f1` over `len` samples (then silence).
    Sweep {
        f0: f64,
        f1: f64,
        amp: f64,
        rate: u32,
        bits24: bool,
        len: u64,
        n: u64,
    },
    /// WAV file; `hires` feeds 16-bit content as I24 (`<< 8`) instead of I16.
    Wav { wav: Wav, pos: usize, hires: bool },
}

impl Source {
    pub fn tone(freq: f64, level_dbfs: f64, rate: u32, bits24: bool) -> Self {
        Source::Tone {
            freq,
            amp: 10f64.powf(level_dbfs / 20.0),
            rate,
            bits24,
            n: 0,
        }
    }

    /// Log sweep 20 Hz -> 0.45 fs at -12 dBFS lasting `seconds`.
    pub fn sweep(rate: u32, seconds: f64, bits24: bool) -> Self {
        Source::Sweep {
            f0: 20.0,
            f1: 0.45 * f64::from(rate),
            amp: 10f64.powf(-12.0 / 20.0),
            rate,
            bits24,
            len: (seconds * f64::from(rate)).round().max(1.0) as u64,
            n: 0,
        }
    }

    pub fn channels(&self) -> usize {
        match self {
            Source::Wav { wav, .. } => usize::from(wav.channels),
            _ => 2,
        }
    }

    /// Frames (864-sample blocks) available, None = endless.
    pub fn total_frames(&self) -> Option<u64> {
        match self {
            Source::Wav { wav, .. } => Some(wav.frames().div_ceil(FRAME_SAMPLES) as u64),
            _ => None,
        }
    }

    /// An empty buffer of the right type and size for this source.
    pub fn new_buf(&self) -> FrameBuf {
        let n = FRAME_SAMPLES * self.channels();
        let int = |bits24: bool| {
            if bits24 {
                FrameBuf::I24(vec![0; n])
            } else {
                FrameBuf::I16(vec![0; n])
            }
        };
        match self {
            Source::Tone { bits24, .. } | Source::Sweep { bits24, .. } => int(*bits24),
            Source::Wav { wav, hires, .. } => match wav.format {
                SampleFormat::Int { bits: 16, .. } => int(*hires),
                SampleFormat::Int { bits: 24, .. } => FrameBuf::I24(vec![0; n]),
                SampleFormat::Int { .. } => FrameBuf::S32(vec![0; n]),
                SampleFormat::Float32 => FrameBuf::F32(vec![0.0; n]),
            },
        }
    }

    /// Fill `buf` (from [`Self::new_buf`]) with the next frame. Returns the
    /// number of real samples per channel written (rest zero-padded); 0 at
    /// the end of a WAV.
    pub fn fill(&mut self, buf: &mut FrameBuf) -> usize {
        match self {
            Source::Tone {
                freq, amp, rate, n, ..
            } => {
                let w = std::f64::consts::TAU * *freq / f64::from(*rate);
                let start = *n;
                synth(buf, |i| *amp * (w * (start + i) as f64).sin());
                *n += FRAME_SAMPLES as u64;
                FRAME_SAMPLES
            }
            Source::Sweep {
                f0,
                f1,
                amp,
                rate,
                len,
                n,
                ..
            } => {
                let t_total = *len as f64 / f64::from(*rate);
                let k = (*f1 / *f0).ln();
                let (f0, amp, rate, len, start) = (*f0, *amp, f64::from(*rate), *len, *n);
                synth(buf, |i| {
                    let s = start + i;
                    if s >= len {
                        return 0.0;
                    }
                    let t = s as f64 / rate;
                    let phase =
                        std::f64::consts::TAU * f0 * t_total / k * ((t / t_total * k).exp() - 1.0);
                    amp * phase.sin()
                });
                *n += FRAME_SAMPLES as u64;
                FRAME_SAMPLES
            }
            Source::Wav { wav, pos, .. } => {
                let bps = wav.bytes_per_sample();
                let ch = usize::from(wav.channels);
                let total = wav.frames();
                let take = (total - *pos).min(FRAME_SAMPLES);
                let raw = &wav.data[*pos * ch * bps..(*pos + take) * ch * bps];
                let real = take * ch;
                match buf {
                    FrameBuf::I16(v) => {
                        le16_to_i16(raw, v);
                        v[real..].fill(0);
                    }
                    FrameBuf::I24(v) => {
                        if bps == 2 {
                            le16_to_i24(raw, v);
                        } else {
                            le24_to_i24(raw, v);
                        }
                        v[real..].fill(0);
                    }
                    FrameBuf::S32(v) => {
                        le32_to_s32(raw, v);
                        v[real..].fill(0);
                    }
                    FrameBuf::F32(v) => {
                        le32_to_f32(raw, v);
                        v[real..].fill(0.0);
                    }
                }
                *pos += take;
                take
            }
        }
    }
}

/// Write the same synthesized value to both channels of a stereo frame.
fn synth(buf: &mut FrameBuf, f: impl Fn(u64) -> f64) {
    match buf {
        FrameBuf::I24(v) => {
            for (i, p) in v.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let s = q24(f(i as u64));
                p[0] = s;
                p[1] = s;
            }
        }
        FrameBuf::I16(v) => {
            for (i, p) in v.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let s = q16(f(i as u64));
                p[0] = s;
                p[1] = s;
            }
        }
        _ => unreachable!("synthetic sources use I16/I24"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::tests::build;
    use ssc_enc::PcmFrame;

    fn wav_source(
        tag: u16,
        ch: u16,
        bits: u16,
        ext: Option<(u16, u16)>,
        raw: &[u8],
        hires: bool,
    ) -> Source {
        Source::Wav {
            wav: Wav::parse(&build(tag, ch, 48000, bits, ext, raw)).unwrap(),
            pos: 0,
            hires,
        }
    }

    #[test]
    fn tone_level_and_continuity() {
        let mut s = Source::tone(1000.0, -20.0, 48000, true);
        let mut b = s.new_buf();
        assert_eq!(b.name(), "I24");
        let mut peak = 0i32;
        let mut last = Vec::new();
        for _ in 0..3 {
            assert_eq!(s.fill(&mut b), FRAME_SAMPLES);
            let FrameBuf::I24(v) = &b else { panic!() };
            assert!(v.chunks(2).all(|p| p[0] == p[1]), "stereo, same on both");
            peak = peak.max(v.iter().map(|x| x.abs()).max().unwrap());
            last = v.clone();
        }
        // -20 dBFS = 0.1 full scale.
        let want = (0.1 * FS24) as i32;
        assert!((peak - want).abs() < want / 100, "peak {peak} want {want}");
        // Phase continues across frames: sample n=3*864-1 matches the formula.
        let n = 3 * FRAME_SAMPLES - 1;
        let exp = q24(0.1 * (std::f64::consts::TAU * 1000.0 / 48000.0 * n as f64).sin());
        assert_eq!(last[2 * (FRAME_SAMPLES - 1)], exp);
        assert_eq!(
            Source::tone(1000.0, -20.0, 44100, false).new_buf().name(),
            "I16"
        );
    }

    #[test]
    fn sweep_spans_band_and_ends_silent() {
        let rate = 48000;
        let mut s = Source::sweep(rate, 0.5, true);
        let mut b = s.new_buf();
        let frames = (0.5 * 48000.0 / FRAME_SAMPLES as f64).ceil() as usize + 1;
        let mut all = Vec::new();
        for _ in 0..frames {
            s.fill(&mut b);
            let FrameBuf::I24(v) = &b else { panic!() };
            all.extend(v.iter().step_by(2).copied());
        }
        let peak = all.iter().map(|x| x.abs()).max().unwrap();
        let want = (10f64.powf(-12.0 / 20.0) * FS24) as i32;
        assert!(peak <= want + 1 && peak > want * 9 / 10, "{peak} vs {want}");
        assert!(
            all[24000..].iter().all(|&x| x == 0),
            "silent after the sweep"
        );
        // Zero crossings get denser towards the end (frequency rises).
        let zc = |r: &[i32]| r.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        assert!(zc(&all[20000..24000]) > 20 * zc(&all[0..4000]).max(1));
    }

    #[test]
    fn wav_full_scale_maps_to_encoder_full_scale() {
        // 16-bit: max and min, hires -> I24 << 8, plain -> I16.
        let raw16 = [0xFF, 0x7F, 0x00, 0x80];
        let mut s = wav_source(1, 2, 16, None, &raw16, true);
        let mut b = s.new_buf();
        assert_eq!(s.fill(&mut b), 1);
        let FrameBuf::I24(v) = &b else {
            panic!("{}", b.name())
        };
        assert_eq!(&v[..3], &[0x7FFF00, -0x800000, 0]);
        let mut s = wav_source(1, 2, 16, None, &raw16, false);
        let mut b = s.new_buf();
        s.fill(&mut b);
        assert_eq!(b, {
            let mut v = vec![0i16; 2 * FRAME_SAMPLES];
            v[0] = i16::MAX;
            v[1] = i16::MIN;
            FrameBuf::I16(v)
        });
        // 24-bit: sign extension.
        let mut s = wav_source(1, 2, 24, None, &[0xFF, 0xFF, 0x7F, 0x00, 0x00, 0x80], true);
        let mut b = s.new_buf();
        s.fill(&mut b);
        let FrameBuf::I24(v) = &b else { panic!() };
        assert_eq!(&v[..2], &[0x7F_FFFF, -0x80_0000]);
        // 32-bit (24 valid, left-justified): full 32-bit scale, S32{32}.
        let mut s = wav_source(
            0,
            2,
            32,
            Some((24, 1)),
            &[0, 0xFF, 0xFF, 0x7F, 0, 0, 0, 0x80],
            true,
        );
        let mut b = s.new_buf();
        s.fill(&mut b);
        assert!(matches!(b.pcm(), PcmFrame::S32 { bits: 32, .. }));
        let FrameBuf::S32(v) = &b else { panic!() };
        assert_eq!(&v[..2], &[0x7FFF_FF00, i32::MIN]);
        // Float.
        let mut raw = Vec::new();
        raw.extend_from_slice(&1.0f32.to_le_bytes());
        raw.extend_from_slice(&(-1.0f32).to_le_bytes());
        let mut s = wav_source(3, 2, 32, None, &raw, true);
        let mut b = s.new_buf();
        s.fill(&mut b);
        let FrameBuf::F32(v) = &b else { panic!() };
        assert_eq!(&v[..2], &[1.0, -1.0]);
    }

    #[test]
    fn wav_frames_and_end() {
        // Mono 24-bit, 864 + 10 samples -> 2 frames, then 0.
        let raw = vec![0x11u8; 3 * (FRAME_SAMPLES + 10)];
        let mut s = wav_source(1, 1, 24, None, &raw, true);
        assert_eq!(s.channels(), 1);
        assert_eq!(s.total_frames(), Some(2));
        let mut b = s.new_buf();
        assert_eq!(s.fill(&mut b), FRAME_SAMPLES);
        assert_eq!(s.fill(&mut b), 10);
        let FrameBuf::I24(v) = &b else { panic!() };
        assert_eq!(v.len(), FRAME_SAMPLES);
        assert_eq!(v[9], 0x111111);
        assert!(v[10..].iter().all(|&x| x == 0));
        assert_eq!(s.fill(&mut b), 0);
    }
}
