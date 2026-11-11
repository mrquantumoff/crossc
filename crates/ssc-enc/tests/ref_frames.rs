//! Frame sizes and headers against Samsung's reference frames, as the reference encoder's
//! tests/ssc_conformance.c + tests/run_tests.py check them: for every configuration of run_tests.py that has a
//! reference .ssc, encode the same input with the same settings and compare, frame by frame over the frames both
//! streams have, the frame size and the 4 header bytes. Like the reference harness, a size mismatch is tolerated
//! only where the reference frame was cut short (its capture tool split frames on a 0xA5 fill byte).
//!
//! Uses the vendored vector subset, or the full set from `SSC_VECTORS_DIR`. Needs no C toolchain.
//!
//! The configuration table and the checks replicate samsung-ssc-encoder tests/run_tests.py and
//! tests/ssc_conformance.c (Copyright (c) 2026 William Ho (WillyBilly06), BSD-3-Clause). See NOTICE.

mod common;

use common::*;
use ssc_enc::{Bitrate, Flags, Mode};

struct Row {
    group: &'static str,
    input: &'static str,
    cfg: Config,
    reference: String,
}

const F_RATES: [u32; 7] = [88000, 96000, 128000, 192000, 229000, 256000, 328000];
const E_RATES: [u32; 7] = [152000, 250000, 291000, 308000, 442000, 584000, 886000];

fn b(bps: u32) -> Bitrate {
    Bitrate::from_bps(bps).unwrap()
}

/// The rows of run_tests.py configs() that name a reference file.
fn rows() -> Vec<Row> {
    let mut v = Vec::new();
    let all: Vec<u32> = F_RATES.iter().chain(E_RATES.iter()).copied().collect();
    let hq16 = |bps: u32| if b(bps).is_hq() { Flags::SAMSUNG_HQ16_LEVEL } else { Flags::NONE };
    for &r in &all {
        let mut cfg = Config::new(Mode::Ssc44k1, Fmt::S16, &[b(r)]);
        cfg.flags = hq16(r);
        v.push(Row { group: "44.1 kHz, 16-bit", input: "mix.s16le", cfg, reference: format!("mix_{r}.ssc") });
    }
    for &r in &all {
        let cfg = Config::new(Mode::Ssc44k1, Fmt::S24, &[b(r)]);
        v.push(Row { group: "44.1 kHz mode, 24-bit", input: "mix.s16le", cfg, reference: format!("mix24_{r}.ssc") });
    }
    v.push(Row {
        group: "48 kHz, 16-bit",
        input: "mix.s16le",
        cfg: Config::new(Mode::Ssc48k, Fmt::S16, &[Bitrate::K192]),
        reference: "mix48_192000.ssc".into(),
    });
    v.push(Row {
        group: "48 kHz HiFi, 24-bit",
        input: "mix48k.s16le",
        cfg: Config::new(Mode::Hifi48k, Fmt::S24, &[Bitrate::K192]),
        reference: "hifi48_192000.ssc".into(),
    });
    v.push(Row {
        group: "96 kHz, 16-bit",
        input: "mix.s16le",
        cfg: Config::new(Mode::Uhq96k, Fmt::S16, &[Bitrate::K192]),
        reference: "mix96_192000.ssc".into(),
    });
    for r in [291000, 584000] {
        v.push(Row {
            group: "96 kHz UHQ, 24-bit",
            input: "mix96k.s16le",
            cfg: Config::new(Mode::Uhq96k, Fmt::S24, &[b(r)]),
            reference: format!("uhq96_{r}.ssc"),
        });
    }
    for r in [88000, 192000, 328000, 886000] {
        let mut cfg = Config::new(Mode::Ssc44k1, Fmt::S16, &[b(r)]);
        cfg.flags = hq16(r);
        v.push(Row { group: "tones", input: "sine.s16le", cfg, reference: format!("sine_{r}.ssc") });
    }
    v
}

#[test]
fn sizes_and_headers_match_samsung_reference_frames() {
    let dir = vectors_dir();
    let mut checked = 0;
    let mut frames_total = 0;
    let mut cut_short_total = 0;
    let mut failures = Vec::new();
    for row in rows() {
        let ref_path = dir.join("ref_frames").join(&row.reference);
        if !ref_path.exists() {
            continue; // run_tests.py skips rows whose reference is absent
        }
        let raw = read(&dir.join(row.input));
        let (ours_buf, _) = encode_rust(&row.cfg, &raw);
        let ref_buf = read(&ref_path);
        let ours = parse_ssc(&ours_buf);
        let refs = parse_ssc(&ref_buf);
        let ncmp = ours.len().min(refs.len());
        assert!(ncmp > 0, "{}: no frames to compare", row.reference);
        let (mut size_mismatch, mut header_mismatch, mut cut_short) = (0, 0, 0);
        for k in 0..ncmp {
            let (o, r) = (ours[k], refs[k]);
            if o.len() != r.len() {
                size_mismatch += 1;
                // tolerated only if the reference frame is shorter than its own header says
                if header_frame_size(r, row.cfg.mode).is_some_and(|want| r.len() < want) {
                    cut_short += 1;
                }
            }
            if o[..4] != r[..4] {
                if header_mismatch < 3 {
                    eprintln!("{} frame {k}: header ours {:02x?} ref {:02x?}", row.reference, &o[..4], &r[..4]);
                }
                header_mismatch += 1;
            }
        }
        println!(
            "{:<22} {:<13} {:<20} compared {:>3}: size mismatch {} ({} ref frames cut short), header mismatch {}",
            row.group, row.input, row.reference, ncmp, size_mismatch, cut_short, header_mismatch
        );
        if header_mismatch != 0 || size_mismatch != cut_short {
            failures.push(row.reference.clone());
        }
        checked += 1;
        frames_total += ncmp;
        cut_short_total += cut_short;
    }
    println!("{checked} reference files, {frames_total} frames compared, {cut_short_total} cut-short reference frames");
    assert!(checked >= 30, "only {checked} reference files found in {}", dir.display());
    assert!(failures.is_empty(), "mismatches against: {failures:?}");
}
