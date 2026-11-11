//! Comparison against the C reference encoder and the BSD checker decoder (feature `oracle`).
//!
//! Build the oracle first: `crates\ssc-enc\oracle\build_oracle.cmd <samsung-ssc-encoder checkout>`, then
//! `cargo test -p ssc-enc --release --features oracle`. `SSC_ORACLE` overrides the oracle path; `SSC_VECTORS_DIR`
//! runs against the full vector set instead of the vendored subset.
//!
//! (a) byte identity: the Rust output (frames and every `FrameInfo` field) equals the C encoder's for every mode,
//!     all 14 bitrates, every input vector, all four PCM front ends (including real 24-bit and 16/20/32-bit
//!     `S32` input with nonzero low bits), mono input, the encoder flags, per-frame
//!     bitrate switching and a synthetic stress signal;
//! (c) decode check: every Rust frame decodes without error in the C checker decoder, and the decoder's final
//!     range-coder position equals the encoder's (the symbol round-trip of tests/ssc_conformance.c).
#![cfg(feature = "oracle")]

mod common;

use common::*;
use ssc_enc::{Bitrate, Flags, Mode};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

const MODES: [Mode; 5] = [Mode::Ssc44k1, Mode::Ssc48k, Mode::Hifi48k, Mode::Uhq96k, Mode::LegacyUhq96k];
const FMTS: [Fmt; 3] = [Fmt::S16, Fmt::S24, Fmt::F32];

fn oracle() -> PathBuf {
    let p = match std::env::var_os("SSC_ORACLE") {
        Some(p) => PathBuf::from(p),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/c-oracle/ssc_oracle.exe"),
    };
    assert!(p.exists(), "C oracle not found at {} (run oracle/build_oracle.cmd or set SSC_ORACLE)", p.display());
    p
}

fn tmp(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ssc-oracle");
    std::fs::create_dir_all(&d).unwrap();
    d.join(format!("{}-{}-{name}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().expect("cannot run oracle");
    assert!(out.status.success(), "oracle failed: {:?}\n{}", cmd, String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn oracle_enc(cfg: &Config, input: &Path) -> (Vec<u8>, Vec<String>) {
    let out = tmp("c.ssc");
    let info = tmp("c.txt");
    let rates: Vec<String> = cfg.rates.iter().map(|r| r.bps().to_string()).collect();
    let mut cmd = Command::new(oracle());
    cmd.arg("enc").args(["-r", &cfg.mode.sample_rate().to_string(), "-fmt", cfg.fmt.name()]).args([
        "-flags",
        &cfg.flags.bits().to_string(),
        "-b",
        &rates.join(","),
    ]);
    if let Some(bits) = cfg.fmt.bits() {
        cmd.args(["-bits", &bits.to_string()]);
    }
    if cfg.mono {
        cmd.arg("-mono");
    }
    cmd.arg(input).arg(&out).arg(&info);
    run(&mut cmd);
    let s = read(&out);
    let lines = String::from_utf8(read(&info)).unwrap().lines().map(str::to_owned).collect();
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&info);
    (s, lines)
}

/// (ret, tell) per frame from the C checker decoder.
fn oracle_dec(mode: Mode, stream: &[u8]) -> Vec<(i32, i32)> {
    let p = tmp("r.ssc");
    std::fs::write(&p, stream).unwrap();
    let out = run(Command::new(oracle()).args(["dec", "-r", &mode.sample_rate().to_string()]).arg(&p));
    let _ = std::fs::remove_file(&p);
    out.lines()
        .map(|l| {
            let mut it = l.split_whitespace().map(|v| v.parse::<i32>().unwrap());
            (it.next().unwrap(), it.next().unwrap())
        })
        .collect()
}

#[derive(Default)]
struct Tally {
    runs: usize,
    frames: usize,
    byte_mismatch_runs: Vec<String>,
    decode_errors: usize,
    tell_mismatches: usize,
}

impl Tally {
    /// Runs one configuration through both encoders and the checker decoder.
    fn check(&mut self, cfg: &Config, input: &Path, raw: &[u8]) {
        let (ours, infos) = encode_rust(cfg, raw);
        let (theirs, c_infos) = oracle_enc(cfg, input);
        let our_lines: Vec<String> = infos.iter().map(info_line).collect();
        self.runs += 1;
        self.frames += infos.len();
        if ours != theirs || our_lines != c_infos {
            let of = parse_ssc(&ours);
            let tf = parse_ssc(&theirs);
            let first = (0..of.len().max(tf.len()))
                .find(|&k| of.get(k) != tf.get(k) || our_lines.get(k) != c_infos.get(k))
                .unwrap_or(0);
            eprintln!(
                "BYTE MISMATCH {} ({}): first differing frame {first}\n  rust info {:?}\n  c    info {:?}",
                cfg.label(),
                input.display(),
                our_lines.get(first),
                c_infos.get(first)
            );
            self.byte_mismatch_runs.push(cfg.label());
        }
        let dec = oracle_dec(cfg.mode, &ours);
        assert_eq!(dec.len(), infos.len(), "{}: decoder frame count", cfg.label());
        for (k, (&(ret, tell), info)) in dec.iter().zip(&infos).enumerate() {
            if ret != 864 {
                eprintln!("{}: frame {k}: decode error {ret}", cfg.label());
                self.decode_errors += 1;
            }
            if tell != info.tell_bits as i32 {
                if self.tell_mismatches < 5 {
                    eprintln!("{}: frame {k}: encoder tell {}, decoder tell {tell}", cfg.label(), info.tell_bits);
                }
                self.tell_mismatches += 1;
            }
        }
    }

    fn finish(self, what: &str) {
        println!(
            "{what}: {} runs, {} frames, {} byte-mismatching runs, {} decode errors, {} tell mismatches",
            self.runs,
            self.frames,
            self.byte_mismatch_runs.len(),
            self.decode_errors,
            self.tell_mismatches
        );
        assert!(self.runs > 0);
        assert!(self.byte_mismatch_runs.is_empty(), "{what}: Rust != C for {:?}", self.byte_mismatch_runs);
        assert_eq!(self.decode_errors, 0, "{what}: decode errors");
        assert_eq!(self.tell_mismatches, 0, "{what}: tell mismatches");
    }
}

/// Every mode x every bitrate x every front end for one input vector.
fn matrix(input: &str, mono: bool) {
    let path = vectors_dir().join(input);
    let raw = read(&path);
    let mut t = Tally::default();
    for mode in MODES {
        for fmt in FMTS {
            for rate in Bitrate::ALL {
                let mut cfg = Config::new(mode, fmt, &[rate]);
                cfg.mono = mono;
                t.check(&cfg, &path, &raw);
            }
        }
    }
    t.finish(input);
}

#[test]
fn matrix_mix() {
    matrix("mix.s16le", false);
}

#[test]
fn matrix_mix48k() {
    matrix("mix48k.s16le", false);
}

#[test]
fn matrix_mix96k() {
    matrix("mix96k.s16le", false);
}

#[test]
fn matrix_sine() {
    matrix("sine.s16le", false);
}

#[test]
fn matrix_mono() {
    matrix("mono.s16le", true);
}

/// Encoder flags (each one and all together) on music and tones, 16-bit input at every rate (the Samsung hq16
/// level flag only acts there) and 24-bit input.
#[test]
fn flags() {
    let mut t = Tally::default();
    let all = Flags::SAMSUNG_HQ16_LEVEL | Flags::NO_TRANSIENTS | Flags::NO_POSTFILTER | Flags::FLAT_RESPONSE;
    let sets = [Flags::SAMSUNG_HQ16_LEVEL, Flags::NO_TRANSIENTS, Flags::NO_POSTFILTER, Flags::FLAT_RESPONSE, all];
    for input in ["mix.s16le", "sine.s16le"] {
        let path = vectors_dir().join(input);
        let raw = read(&path);
        for mode in [Mode::Ssc44k1, Mode::Uhq96k] {
            for fmt in [Fmt::S16, Fmt::S24] {
                for flags in sets {
                    for rate in Bitrate::ALL {
                        let mut cfg = Config::new(mode, fmt, &[rate]);
                        cfg.flags = flags;
                        t.check(&cfg, &path, &raw);
                    }
                }
            }
        }
    }
    t.finish("flags");
}

/// The bitrate changes on every frame (all 14 in turn, and a UHQ-like 584/291/152 pattern).
#[test]
fn bitrate_switching() {
    let mut t = Tally::default();
    let uhq = [Bitrate::K584, Bitrate::K584, Bitrate::K291, Bitrate::K152, Bitrate::K291];
    for input in ["mix.s16le", "mix96k.s16le", "sine.s16le"] {
        let path = vectors_dir().join(input);
        let raw = read(&path);
        for mode in MODES {
            for fmt in FMTS {
                t.check(&Config::new(mode, fmt, &Bitrate::ALL), &path, &raw);
                t.check(&Config::new(mode, fmt, &uhq), &path, &raw);
            }
        }
    }
    t.finish("bitrate switching");
}

/// Synthetic stress signal (silence, clipping, square wave, clicks, near-silence) at every codec rate, 16- and
/// 24-bit, every bitrate and per-frame switching.
#[test]
fn stress() {
    let mut t = Tally::default();
    for (mode, rate) in [(Mode::Ssc44k1, 44100), (Mode::Ssc48k, 48000), (Mode::Uhq96k, 96000)] {
        let raw = synthetic(rate, 4.0);
        let path = tmp(&format!("synth_{rate}.s16le"));
        std::fs::write(&path, &raw).unwrap();
        for fmt in [Fmt::S16, Fmt::S24] {
            for r in Bitrate::ALL {
                t.check(&Config::new(mode, fmt, &[r]), &path, &raw);
            }
            t.check(&Config::new(mode, fmt, &Bitrate::ALL), &path, &raw);
        }
        let _ = std::fs::remove_file(&path);
    }
    t.finish("stress");
}

/// Non-finite and out-of-range float input (NaN, +-inf, +-1e30, +-4.0, denormals) mixed into music: undefined
/// behaviour in C; the port mirrors the x86-64 MSVC build. The decode check only requires the frames to parse.
#[test]
fn pathological_float_input() {
    let raw = read(&vectors_dir().join("mix.s16le"));
    let specials = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30, -1e30, 4.0, -4.0, 1e-40, -0.0];
    let mut f = Vec::new();
    for (i, b) in raw.chunks_exact(2).enumerate() {
        let frame = i / (2 * ssc_enc::FRAME_SAMPLES);
        let mut v = i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0;
        // frames 4.., every 9th frame clean; otherwise sprinkle one kind of special value
        if frame >= 4 && frame % 9 != 0 && i % (13 + frame % 5) == 0 {
            v = specials[frame % specials.len()];
        }
        f.extend_from_slice(&v.to_le_bytes());
    }
    let path = tmp("special.f32le");
    std::fs::write(&path, &f).unwrap();
    let mut t = Tally::default();
    for mode in [Mode::Ssc44k1, Mode::Uhq96k] {
        for rate in Bitrate::ALL {
            t.check(&Config::new(mode, Fmt::F32Raw, &[rate]), &path, &f);
        }
        t.check(&Config::new(mode, Fmt::F32Raw, &Bitrate::ALL), &path, &f);
    }
    let _ = std::fs::remove_file(&path);
    t.finish("pathological float input");
}

/// Real high-resolution integer input: 24-bit samples with nonzero low bits through `I24` and `S32 { bits: 24 }`,
/// and `S32` at 16, 20 and 32 bits (full-precision values, the whole i32 range at 32 bits), stereo and mono, every
/// bitrate in both codec modes plus per-frame switching.
#[test]
fn wide_integer_input() {
    let mut t = Tally::default();
    let fmts = [Fmt::I24Raw, Fmt::S32Raw(24), Fmt::S32Raw(16), Fmt::S32Raw(20), Fmt::S32Raw(32)];
    for (input, mono, modes) in [
        ("mix.s16le", false, &[Mode::Hifi48k][..]),
        ("mix96k.s16le", false, &[Mode::Uhq96k][..]),
        ("mono.s16le", true, &[Mode::Hifi48k, Mode::Uhq96k][..]),
    ] {
        let raw = read(&vectors_dir().join(input));
        for fmt in fmts {
            let wide = widen_s16(&raw, fmt.bits().unwrap());
            let path = tmp(&format!("{}.s32le", fmt.label()));
            std::fs::write(&path, &wide).unwrap();
            for &mode in modes {
                for rate in Bitrate::ALL {
                    let mut cfg = Config::new(mode, fmt, &[rate]);
                    cfg.mono = mono;
                    t.check(&cfg, &path, &wide);
                }
                let mut cfg = Config::new(mode, fmt, &Bitrate::ALL);
                cfg.mono = mono;
                t.check(&cfg, &path, &wide);
            }
            let _ = std::fs::remove_file(&path);
        }
    }
    t.finish("wide integer input");
}

/// The C reference itself does not depend on the optimisation level: the /O2 oracle and the /Od build give the
/// same streams (every vector, mode class, front end and bitrate).
#[test]
fn c_build_is_optimisation_independent() {
    let o2 = oracle();
    let od = o2.with_file_name("ssc_oracle_od.exe");
    assert!(od.exists(), "{} not found", od.display());
    let mut runs = 0;
    for input in ["mix.s16le", "mix48k.s16le", "mix96k.s16le", "sine.s16le"] {
        let path = vectors_dir().join(input);
        for rate in ["44100", "96000"] {
            for fmt in FMTS {
                for b in Bitrate::ALL {
                    let mut outs = Vec::new();
                    for exe in [&o2, &od] {
                        let (s, i) = (tmp("o.ssc"), tmp("o.txt"));
                        run(Command::new(exe)
                            .args(["enc", "-r", rate, "-fmt", fmt.name(), "-b", &b.bps().to_string()])
                            .arg(&path)
                            .arg(&s)
                            .arg(&i));
                        outs.push((read(&s), read(&i)));
                        let _ = std::fs::remove_file(&s);
                        let _ = std::fs::remove_file(&i);
                    }
                    assert!(outs[0] == outs[1], "/O2 != /Od: {input} {rate} {} {b:?}", fmt.name());
                    runs += 1;
                }
            }
        }
    }
    println!("C /O2 vs /Od: {runs} runs identical");
}

/// The float tables (window, eMeans) have the same bit patterns as the compiled C tables.
#[test]
fn float_tables_match_c() {
    let out = run(Command::new(oracle()).arg("tables"));
    let mut n = 0;
    for line in out.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let i: usize = f[1].parse().unwrap();
        let bits = u32::from_str_radix(f[2], 16).unwrap();
        let ours = match f[0] {
            "window" => ssc_enc_test_tables::window(i),
            "emeans" => ssc_enc_test_tables::e_means(i),
            _ => panic!("unexpected line {line}"),
        };
        assert_eq!(ours, bits, "{line}");
        n += 1;
    }
    assert_eq!(n, 108 + 25);
}

/// The tables are crate-private; the float literals are re-read here from the source file, parsed by Rust
/// exactly as rustc parses them.
mod ssc_enc_test_tables {
    fn table(name: &str) -> Vec<u32> {
        let src = include_str!("../src/tables.rs");
        let start = src.find(&format!("pub(crate) const {name}: [f32;")).unwrap();
        let body = &src[start..];
        let body = &body[body.find("= [").unwrap() + 3..body.find("];").unwrap()];
        body.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|s| s.parse::<f32>().unwrap().to_bits()).collect()
    }
    pub fn window(i: usize) -> u32 {
        table("WINDOW")[i]
    }
    pub fn e_means(i: usize) -> u32 {
        table("E_MEANS")[i]
    }
}

/// Native 24-bit programme material (see `hires24`: sweep, sub-16-bit tones, ultrasonic multitone, decays through
/// the 16-bit floor), generated at 24-bit precision rather than widened from 16-bit vectors. Byte identity with C
/// for HiFi / UHQ / legacy UHQ at every bitrate plus per-frame switching, through the I24, S32{24} and F32 front
/// ends, and the decode check on every frame.
#[test]
fn hires24_input() {
    let mut t = Tally::default();
    for mode in [Mode::Hifi48k, Mode::Uhq96k, Mode::LegacyUhq96k] {
        let pcm = hires24(mode.sample_rate(), 4.0);
        let s32 = s32le(&pcm);
        let f32: Vec<u8> = pcm.iter().flat_map(|&s| (s as f32 / 8_388_608.0).to_le_bytes()).collect();
        let p32 = tmp("hires.s32le");
        let pf = tmp("hires.f32le");
        std::fs::write(&p32, &s32).unwrap();
        std::fs::write(&pf, &f32).unwrap();
        for (fmt, path, raw) in [(Fmt::I24Raw, &p32, &s32), (Fmt::S32Raw(24), &p32, &s32), (Fmt::F32Raw, &pf, &f32)] {
            for rate in Bitrate::ALL {
                t.check(&Config::new(mode, fmt, &[rate]), path, raw);
            }
            t.check(&Config::new(mode, fmt, &Bitrate::ALL), path, raw);
        }
        let _ = std::fs::remove_file(&p32);
        let _ = std::fs::remove_file(&pf);
    }
    t.finish("native 24-bit input");
}

/// Encodes 24-bit stereo with the Rust encoder, decodes with the C checker decoder, returns the decoded left
/// channel (full scale 1.0) with the first and last 0.25 s trimmed (codec delay / edges).
fn roundtrip24(mode: Mode, rate: Bitrate, pcm: &[i32]) -> Vec<f64> {
    let raw = s32le(pcm);
    let (stream, _) = encode_rust(&Config::new(mode, Fmt::I24Raw, &[rate]), &raw);
    let p = tmp("fid.ssc");
    let o = tmp("fid.f32le");
    std::fs::write(&p, &stream).unwrap();
    run(Command::new(oracle()).args(["dec", "-r", &mode.sample_rate().to_string(), "-pcm"]).arg(&o).arg(&p));
    let dec = std::fs::read(&o).unwrap();
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(&o);
    let left: Vec<f64> = dec.chunks_exact(8).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64).collect();
    let trim = mode.sample_rate() as usize / 4;
    left[trim..left.len() - trim].to_vec()
}

/// End-to-end proof that the 24-bit path carries real 24-bit information: tones below the 16-bit LSB
/// (-96 dBFS) survive Rust encode -> C decode at the claimed level, and UHQ reproduces ultrasonic content.
#[test]
fn hires24_fidelity() {
    let cases: [(&str, Mode, Bitrate, f64, f64); 6] = [
        ("HiFi 48k/24 192k, 1 kHz -20 dBFS", Mode::Hifi48k, Bitrate::K192, 1000.0, -20.0),
        ("HiFi 48k/24 192k, 1 kHz -110 dBFS", Mode::Hifi48k, Bitrate::K192, 1000.0, -110.0),
        ("HiFi 48k/24 192k, 3.5 kHz -100 dBFS", Mode::Hifi48k, Bitrate::K192, 3500.0, -100.0),
        ("UHQ 96k/24 584k, 1 kHz -110 dBFS", Mode::Uhq96k, Bitrate::K584, 1000.0, -110.0),
        ("UHQ 96k/24 584k, 30 kHz -20 dBFS", Mode::Uhq96k, Bitrate::K584, 30000.0, -20.0),
        ("UHQ 96k/24 584k, 40 kHz -20 dBFS", Mode::Uhq96k, Bitrate::K584, 40000.0, -20.0),
    ];
    for (name, mode, rate, f, lvl) in cases {
        let sr = mode.sample_rate();
        let dec = roundtrip24(mode, rate, &tone24(sr, 3.0, f, lvl));
        let got = tone_level_db(&dec, sr, f);
        // Noise floor reference: level measured 37 Hz away from the tone.
        let floor = tone_level_db(&dec, sr, f + 37.0);
        println!(
            "{name} @ {} bps: input {lvl:.1} dBFS -> decoded {got:.2} dBFS (neighbour bin {floor:.1})",
            rate.bps()
        );
        // Measured on x86-64/MSVC: in-band tones within 0.01 dB, 30/40 kHz within 0.61 dB, floor >= 63 dB down.
        let tol = if f > 20000.0 { 1.0 } else { 0.25 };
        assert!((got - lvl).abs() <= tol, "{name}: decoded {got:.2} dBFS, expected {lvl} +- {tol}");
        assert!(got - floor >= 40.0, "{name}: tone only {:.1} dB above the neighbouring bin", got - floor);
    }
}
