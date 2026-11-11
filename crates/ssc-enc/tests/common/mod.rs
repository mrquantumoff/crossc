//! Shared helpers for the ssc-enc integration tests.
#![allow(dead_code)]

use ssc_enc::{Bitrate, Encoder, FRAME_SAMPLES, Flags, FrameInfo, MAX_FRAME_BYTES, Mode, PcmFrame};
use std::path::{Path, PathBuf};

/// Input sample format fed to the encoder. The raw test input is s16le, except for the `*Raw` formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fmt {
    /// `PcmFrame::I16`.
    S16,
    /// `PcmFrame::I24` with sample * 256 (Samsung's HiFi/UHQ path for 16-bit content).
    S24,
    /// `PcmFrame::F32` with sample / 32768.
    F32,
    /// `PcmFrame::F32`, raw f32le stereo input passed through unchanged (for non-finite / out-of-range tests).
    F32Raw,
    /// `PcmFrame::I24`, raw s32le input (24-bit values with nonzero low bits) passed through unchanged.
    I24Raw,
    /// `PcmFrame::S32 { bits }`, raw s32le input passed through unchanged.
    S32Raw(u32),
}

impl Fmt {
    pub fn name(self) -> &'static str {
        match self {
            Fmt::S16 => "s16",
            Fmt::S24 => "s24",
            Fmt::F32 => "f32",
            Fmt::F32Raw => "f32raw",
            Fmt::I24Raw | Fmt::S32Raw(_) => "s32raw",
        }
    }

    /// `bits_per_sample` for the C `ssc_encode_s32` call of the raw s32 formats.
    pub fn bits(self) -> Option<u32> {
        match self {
            Fmt::I24Raw => Some(24),
            Fmt::S32Raw(b) => Some(b),
            _ => None,
        }
    }

    /// Label: the oracle format name, plus the bit depth and front end for the raw s32 formats.
    pub fn label(self) -> String {
        match self {
            Fmt::I24Raw => "i24raw".into(),
            Fmt::S32Raw(b) => format!("s32raw{b}"),
            _ => self.name().into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub mode: Mode,
    pub fmt: Fmt,
    pub mono: bool,
    pub flags: Flags,
    /// Frame k uses rates[k % len].
    pub rates: Vec<Bitrate>,
}

impl Config {
    pub fn new(mode: Mode, fmt: Fmt, rates: &[Bitrate]) -> Self {
        Config { mode, fmt, mono: false, flags: Flags::NONE, rates: rates.to_vec() }
    }

    pub fn label(&self) -> String {
        let rates: Vec<String> = self.rates.iter().map(|r| (r.bps() / 1000).to_string()).collect();
        format!(
            "{:?}-{}{}-f{}-{}",
            self.mode,
            self.fmt.label(),
            if self.mono { "-mono" } else { "" },
            self.flags.bits(),
            rates.join("_")
        )
    }
}

/// Directory with the test vectors: `SSC_VECTORS_DIR` if set (e.g. the full set in a samsung-ssc-encoder
/// checkout's tests/vectors), else the vendored subset.
pub fn vectors_dir() -> PathBuf {
    match std::env::var_os("SSC_VECTORS_DIR") {
        Some(d) => PathBuf::from(d),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("vectors"),
    }
}

pub fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Frames of a `[u16 LE length][frame]` stream (truncated trailing entries are dropped, like the C harness).
pub fn parse_ssc(d: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 2 <= d.len() {
        let l = d[p] as usize | (d[p + 1] as usize) << 8;
        p += 2;
        if p + l > d.len() {
            break;
        }
        out.push(&d[p..p + l]);
        p += l;
    }
    out
}

/// Frame size derived from a frame header (what a receiver does), or None if the header is not valid.
pub fn header_frame_size(f: &[u8], mode: Mode) -> Option<usize> {
    if f.len() < 4 || f[0] != 0xFF {
        return None;
    }
    // The low 5 bits of byte 3 identify the rate in both families (0x10..0x16 = F0..F6, 0x00..0x06 = E0..E6).
    let code = 0xE0 | (f[3] & 0x1F);
    Bitrate::from_code(code).map(|b| ssc_enc::frame_size(mode, b))
}

/// Encodes raw s16le input with the Rust encoder. Returns the `[u16 len][frame]` stream and per-frame info.
pub fn encode_rust(cfg: &Config, raw: &[u8]) -> (Vec<u8>, Vec<FrameInfo>) {
    if cfg.fmt == Fmt::F32Raw {
        return encode_rust_f32raw(cfg, raw);
    }
    if cfg.fmt.bits().is_some() {
        return encode_rust_s32raw(cfg, raw);
    }
    let ch = if cfg.mono { 1 } else { 2 };
    let samples: Vec<i16> = raw.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    let per_frame = FRAME_SAMPLES * ch;
    let nframes = samples.len() / per_frame;
    let mut enc = Box::new(Encoder::new(cfg.mode, ch).unwrap());
    enc.set_flags(cfg.flags);
    let mut stream = Vec::new();
    let mut infos = Vec::new();
    let mut out = [0u8; MAX_FRAME_BYTES];
    let mut s32 = vec![0i32; per_frame];
    let mut f32s = vec![0f32; per_frame];
    for k in 0..nframes {
        let p = &samples[k * per_frame..(k + 1) * per_frame];
        let rate = cfg.rates[k % cfg.rates.len()];
        let pcm = match cfg.fmt {
            Fmt::S16 => PcmFrame::I16(p),
            Fmt::S24 => {
                for (d, &s) in s32.iter_mut().zip(p) {
                    *d = s as i32 * 256;
                }
                PcmFrame::I24(&s32)
            }
            Fmt::F32 => {
                for (d, &s) in f32s.iter_mut().zip(p) {
                    *d = s as f32 / 32768.0;
                }
                PcmFrame::F32(&f32s)
            }
            Fmt::F32Raw | Fmt::I24Raw | Fmt::S32Raw(_) => unreachable!(),
        };
        let n = enc.encode_frame(pcm, rate, &mut out).unwrap_or_else(|e| panic!("{}: frame {k}: {e}", cfg.label()));
        assert_eq!(n, ssc_enc::frame_size(cfg.mode, rate));
        stream.extend_from_slice(&(n as u16).to_le_bytes());
        stream.extend_from_slice(&out[..n]);
        infos.push(enc.last_frame_info().unwrap());
    }
    (stream, infos)
}

fn encode_rust_f32raw(cfg: &Config, raw: &[u8]) -> (Vec<u8>, Vec<FrameInfo>) {
    let samples: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    let mut enc = Box::new(Encoder::new(cfg.mode, 2).unwrap());
    enc.set_flags(cfg.flags);
    let (mut stream, mut infos) = (Vec::new(), Vec::new());
    let mut out = [0u8; MAX_FRAME_BYTES];
    for (k, p) in samples.chunks_exact(2 * FRAME_SAMPLES).enumerate() {
        let rate = cfg.rates[k % cfg.rates.len()];
        let n = enc.encode_frame(PcmFrame::F32(p), rate, &mut out).unwrap();
        stream.extend_from_slice(&(n as u16).to_le_bytes());
        stream.extend_from_slice(&out[..n]);
        infos.push(enc.last_frame_info().unwrap());
    }
    (stream, infos)
}

fn encode_rust_s32raw(cfg: &Config, raw: &[u8]) -> (Vec<u8>, Vec<FrameInfo>) {
    let ch = if cfg.mono { 1 } else { 2 };
    let samples: Vec<i32> = raw.chunks_exact(4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    let mut enc = Box::new(Encoder::new(cfg.mode, ch).unwrap());
    enc.set_flags(cfg.flags);
    let (mut stream, mut infos) = (Vec::new(), Vec::new());
    let mut out = [0u8; MAX_FRAME_BYTES];
    for (k, p) in samples.chunks_exact(ch * FRAME_SAMPLES).enumerate() {
        let rate = cfg.rates[k % cfg.rates.len()];
        let pcm = match cfg.fmt {
            Fmt::I24Raw => PcmFrame::I24(p),
            Fmt::S32Raw(bits) => PcmFrame::S32 { samples: p, bits },
            _ => unreachable!(),
        };
        let n = enc.encode_frame(pcm, rate, &mut out).unwrap_or_else(|e| panic!("{}: frame {k}: {e}", cfg.label()));
        stream.extend_from_slice(&(n as u16).to_le_bytes());
        stream.extend_from_slice(&out[..n]);
        infos.push(enc.last_frame_info().unwrap());
    }
    (stream, infos)
}

/// Raw s32le PCM with `bits` significant bits (right-justified), built from s16le input: each 16-bit sample is
/// scaled to `bits` and its new low bits are filled with deterministic dither, so the samples use the full
/// precision (for bits = 32 this covers the whole i32 range, including i32::MIN for -32768).
pub fn widen_s16(raw: &[u8], bits: u32) -> Vec<u8> {
    let mut lcg: u32 = 0x2545_f491;
    let mut out = Vec::with_capacity(raw.len() * 2);
    for b in raw.chunks_exact(2) {
        let s = i16::from_le_bytes([b[0], b[1]]) as i64;
        lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let shift = bits - 16;
        let low = if shift == 0 { 0 } else { (lcg >> (32 - shift)) as i64 };
        let v = (s << shift) | low;
        debug_assert!(v >= -(1i64 << (bits - 1)) && v < (1i64 << (bits - 1)));
        out.extend_from_slice(&(v as i32).to_le_bytes());
    }
    out
}

/// Same line format as `ssc_oracle enc` writes to its info file.
pub fn info_line(i: &FrameInfo) -> String {
    format!(
        "{} {} {} {} {} {} {} {} {} {} {} {} {}",
        i.bytes,
        i.bitrate,
        i.hq as i32,
        i.seq,
        i.end_band,
        i.transient as i32,
        i.pf_on as i32,
        i.pf_period,
        i.pf_gain_q,
        i.silence as i32,
        i.coded_bands,
        i.tell_bits,
        i.total_bits
    )
}

/// Deterministic stereo stress signal (s16le), following make_synthetic() of samsung-ssc-encoder
/// tests/run_tests.py (Copyright (c) 2026 William Ho, BSD-3-Clause; a different random generator, so not the same
/// samples): silence, tones and a
/// chirp with noise bursts, full-scale clipped noise, a full-scale square wave against a sine, isolated clicks,
/// silence again and very quiet noise.
pub fn synthetic(rate: u32, seconds: f64) -> Vec<u8> {
    let mut lcg: u64 = 0x1234_5678_9abc_def0;
    let mut rnd = move || {
        lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((lcg >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    };
    let n = (rate as f64 * seconds) as usize;
    let mut out = Vec::with_capacity(n * 4);
    let pi2 = 2.0 * std::f64::consts::PI;
    for i in 0..n {
        let t = i as f64 / rate as f64;
        let seg = 8 * i / n;
        let (l, r) = match seg {
            0 | 6 => (0.0, 0.0),
            1 | 2 => {
                let burst = if (i / 4410) % 4 == 0 { 0.25 } else { 0.0 };
                (
                    0.3 * (pi2 * 440.0 * t).sin() + 0.15 * (pi2 * (200.0 + 3000.0 * t) * t).sin() + burst * rnd(),
                    0.3 * (pi2 * 660.0 * t + 1.0).sin() + 0.1 * (pi2 * 5000.0 * t).sin(),
                )
            }
            3 => (1.5 * rnd(), 1.5 * rnd()),
            4 => (if (pi2 * 997.0 * t).sin() >= 0.0 { 1.0 } else { -1.0 }, (pi2 * 3001.0 * t).sin()),
            5 => {
                let v = if i % (rate as usize / 7) < 3 { 0.9 } else { 0.0 };
                (v, v)
            }
            _ => (0.0002 * rnd(), 0.0002 * rnd()),
        };
        for v in [l, r] {
            let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}

/// Quantizes a float sample (full scale +-1.0) to right-justified 24-bit with TPDF dither of +-1 LSB.
fn quantize24(v: f64, rnd: &mut impl FnMut() -> f64) -> i32 {
    let fs = (1i64 << 23) as f64;
    let d = (rnd() + rnd()) * 0.5; // triangular, +-1 LSB
    (v * fs + d).round().clamp(-fs, fs - 1.0) as i32
}

fn lcg_unit(seed: u64) -> impl FnMut() -> f64 {
    let mut s = seed;
    move || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((s >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
}

/// dBFS -> linear amplitude.
pub fn db(dbfs: f64) -> f64 {
    10f64.powf(dbfs / 20.0)
}

/// Native 24-bit stereo test programme (interleaved, right-justified i32, full scale +-2^23), synthesised in f64
/// and quantized straight to 24 bits with TPDF dither, so every sample carries real 24-bit information (unlike
/// 16-bit material widened with filler low bits). Four equal segments:
/// 1. log sine sweep 20 Hz -> 0.45 * rate at -1 dBFS (L) and its quadrature (R);
/// 2. sub-16-bit detail: 1 kHz at -110 dBFS (L), 3.5 kHz at -100 dBFS (R), both below the 16-bit LSB (-96 dBFS);
/// 3. multitone at -40 dBFS (100 Hz, 1 kHz, 10 kHz, plus 30 kHz when rate >= 88.2 kHz) over -100 dBFS noise;
/// 4. decaying harmonic "notes" whose tails fade from -6 dBFS to below -130 dBFS, through the 16-bit floor.
pub fn hires24(rate: u32, seconds: f64) -> Vec<i32> {
    let mut rnd = lcg_unit(0x5eed_2424_c0de_0001);
    let n = (rate as f64 * seconds) as usize;
    let fr = rate as f64;
    let pi2 = 2.0 * std::f64::consts::PI;
    let (f0, f1) = (20.0, 0.45 * fr);
    let seg_len = n / 4;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f64 / fr;
        let seg = (i / seg_len.max(1)).min(3);
        let ts = (i - seg * seg_len) as f64 / fr;
        let (l, r) = match seg {
            0 => {
                let dur = seg_len as f64 / fr;
                let k = (f1 / f0).ln();
                let ph = pi2 * f0 * dur / k * ((ts / dur * k).exp() - 1.0);
                (db(-1.0) * ph.sin(), db(-1.0) * ph.cos())
            }
            1 => (db(-110.0) * (pi2 * 1000.0 * t).sin(), db(-100.0) * (pi2 * 3500.0 * t).sin()),
            2 => {
                let mut v = (pi2 * 100.0 * t).sin() + (pi2 * 1000.0 * t).sin() + (pi2 * 10000.0 * t).sin();
                if rate >= 88200 {
                    v += (pi2 * 30000.0 * t).sin();
                }
                let a = db(-40.0);
                (a * v + db(-100.0) * rnd(), a * v * 0.7 + db(-100.0) * rnd())
            }
            _ => {
                let note = (ts / 0.75) as usize;
                let tn = ts - note as f64 * 0.75;
                let f = [220.0, 277.18, 329.63, 440.0][note % 4];
                let env = db(-6.0) * (-tn * 18.0).exp(); // ~ -156 dB/s decay
                let mut v = 0.0;
                for h in 1..=8 {
                    v += (pi2 * f * h as f64 * tn).sin() / h as f64;
                }
                (env * v * 0.5, env * v * 0.45)
            }
        };
        out.push(quantize24(l, &mut rnd));
        out.push(quantize24(r, &mut rnd));
    }
    out
}

/// Pure stereo tone (same on both channels) quantized to 24 bits with TPDF dither.
pub fn tone24(rate: u32, seconds: f64, freq: f64, dbfs: f64) -> Vec<i32> {
    let mut rnd = lcg_unit(0x70ae_0024);
    let n = (rate as f64 * seconds) as usize;
    let a = db(dbfs);
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let v = a * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin();
        let q = quantize24(v, &mut rnd);
        out.push(q);
        out.push(q);
    }
    out
}

pub fn s32le(samples: &[i32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Level in dBFS of the component at `freq` in `x` (mono f64, full scale 1.0), via a Blackman-Harris windowed
/// single-bin DFT; amplitude-calibrated for a sine exactly at `freq`.
pub fn tone_level_db(x: &[f64], rate: u32, freq: f64) -> f64 {
    let n = x.len();
    let pi2 = 2.0 * std::f64::consts::PI;
    let (mut re, mut im, mut wsum) = (0.0, 0.0, 0.0);
    for (i, &v) in x.iter().enumerate() {
        let p = pi2 * i as f64 / (n - 1) as f64;
        let w = 0.35875 - 0.48829 * p.cos() + 0.14128 * (2.0 * p).cos() - 0.01168 * (3.0 * p).cos();
        let ph = pi2 * freq * i as f64 / rate as f64;
        re += v * w * ph.cos();
        im -= v * w * ph.sin();
        wsum += w;
    }
    20.0 * (2.0 * (re * re + im * im).sqrt() / wsum).log10()
}
