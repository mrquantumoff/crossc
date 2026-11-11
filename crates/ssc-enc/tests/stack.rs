//! Stack needed to construct an encoder. Each probe runs in a child process (this test binary re-run with
//! `PROBE_KB` / `PROBE_WHAT` set) because a stack overflow aborts the whole process.
//!
//! `SSC_STACK_REPORT=1 cargo test -p ssc-enc [--release] --test stack -- --nocapture` prints the smallest thread
//! stack (8 KiB steps) each construction path needs. Needs `Encoder::new_boxed` (feature `alloc`, on with `std`).
#![cfg(feature = "alloc")]

use ssc_enc::{Bitrate, Encoder, FRAME_SAMPLES, MAX_FRAME_BYTES, Mode, PcmFrame};

// One function per probe, never inlined, so that a probe's frame holds only its own temporaries.
#[inline(never)]
fn noop() {
    std::hint::black_box(0u8);
}

#[inline(never)]
fn new_boxed() {
    let e = Encoder::new_boxed(Mode::Uhq96k, 2).unwrap();
    std::hint::black_box(&e);
}

#[inline(never)]
fn box_new() {
    let e = Box::new(Encoder::new(Mode::Uhq96k, 2).unwrap());
    std::hint::black_box(&e);
}

#[inline(never)]
fn new_boxed_encode() {
    let mut e = Encoder::new_boxed(Mode::Uhq96k, 2).unwrap();
    let pcm = vec![0x1234i32; 2 * FRAME_SAMPLES];
    let mut out = vec![0u8; MAX_FRAME_BYTES];
    e.encode_frame(PcmFrame::I24(&pcm), Bitrate::K584, &mut out).unwrap();
    std::hint::black_box(&out);
}

fn probe_fn(what: &str) -> fn() {
    match what {
        "noop" => noop,
        "new_boxed" => new_boxed,
        "box_new" => box_new,
        "new_boxed_encode" => new_boxed_encode,
        _ => panic!("unknown probe {what}"),
    }
}

/// Entry point of the child process; does nothing in a normal run.
#[test]
fn probe_entry() {
    if let (Ok(kb), Ok(what)) = (std::env::var("PROBE_KB"), std::env::var("PROBE_WHAT")) {
        let kb: usize = kb.parse().unwrap();
        let f = probe_fn(&what);
        std::thread::Builder::new().stack_size(kb * 1024).spawn(f).unwrap().join().unwrap();
    }
}

fn passes(kb: usize, what: &str) -> bool {
    let exe = std::env::current_exe().unwrap();
    std::process::Command::new(exe)
        .args(["probe_entry", "--exact", "--test-threads=1"])
        .env("PROBE_KB", kb.to_string())
        .env("PROBE_WHAT", what)
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn report() {
    if std::env::var_os("SSC_STACK_REPORT").is_none() {
        return;
    }
    for what in ["noop", "new_boxed", "new_boxed_encode", "box_new"] {
        let first = (8..=1024).step_by(8).find(|&kb| passes(kb, what));
        println!("{what}: first passing thread stack {first:?} KiB");
    }
}

/// `Encoder::new_boxed` never puts the encoder on the stack, in debug or release builds: it fits in a thread
/// stack much smaller than the ~75 KB encoder (measured: 8 KiB debug and release, via a pre-zeroed heap
/// allocation). `Box::new(Encoder::new(..))` does not (measured: 328 KiB debug, 200 KiB release).
#[test]
fn new_boxed_needs_less_stack_than_the_encoder() {
    let kb = 16;
    assert!(kb * 1024 < std::mem::size_of::<Encoder>());
    assert!(passes(kb, "new_boxed"), "Encoder::new_boxed overflowed a {kb} KiB thread stack");
}

/// The documented small-stack pattern: build with `new_boxed`, then encode, in a 256 KiB thread (debug and
/// release; measured 72 KiB debug, 8 KiB release).
#[test]
fn new_boxed_and_encode_fit_in_256k_thread() {
    assert!(passes(256, "new_boxed_encode"), "new_boxed + encode_frame overflowed a 256 KiB thread stack");
}

/// Sanity check of the probe itself: `Box::new(Encoder::new(..))` builds on the stack, so it cannot fit in a
/// stack smaller than the encoder.
#[test]
fn probe_detects_overflow() {
    assert!(!passes(32, "box_new"));
    assert!(passes(8, "noop"));
}
