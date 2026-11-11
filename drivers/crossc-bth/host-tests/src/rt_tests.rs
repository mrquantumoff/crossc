//! `rt_limits`: the RT packet window and the stream count (crash review F7).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::vec::Vec;

use crate::rt_limits::{MAX_PACKET_BYTES, MAX_STREAMS, StreamSlots, packet_bounds, packet_ok};

/// The driver's `MAX_PACKETS`.
const MAX: u32 = 2;
const PAGE: u32 = 4096;

/// bytes/s and frame size of the formats the endpoint offers (acx.rs
/// `FORMATS`), plus 44.1 kHz/16-bit, where 1 ms is not a whole number of
/// frames.
const RATES: [(u32, u32); 4] = [(192_000, 4), (384_000, 8), (768_000, 8), (176_400, 4)];

#[test]
fn the_window_is_one_millisecond_to_five_seconds() {
    assert_eq!(packet_bounds(192_000), (192, 960_000));
    assert_eq!(packet_bounds(384_000), (384, 1_920_000));
    assert_eq!(packet_bounds(768_000), (768, 3_840_000));
    // Rounded down: 44 frames of 4 bytes are 0.9977 ms and must pass.
    assert_eq!(packet_bounds(176_400), (176, 882_000));
}

#[test]
fn both_ends_are_inclusive() {
    for (bps, _) in RATES {
        let (min, max) = packet_bounds(bps);
        assert!(packet_ok(2, MAX, min, bps));
        assert!(packet_ok(2, MAX, max, bps));
        assert!(!packet_ok(2, MAX, min - 1, bps));
        assert!(!packet_ok(2, MAX, max + 1, bps));
    }
}

/// What Windows asks for stays accepted.
#[test]
fn shared_and_exclusive_mode_requests_pass() {
    for (bps, frame) in RATES {
        let ms = |n: u32| (bps as u64 * n as u64 / 1000) as u32 / frame * frame;
        // Shared mode: the audio engine's 10 ms period, two packets.
        assert!(packet_ok(2, MAX, ms(10), bps), "{bps}");
        // Exclusive mode, event driven: 3 ms (the usual minimum period)
        // up to 5000 ms, the longest buffer WASAPI accepts.
        for n in [3, 5, 10, 20, 50, 100, 200, 500, 1000, 2000, 2500, 5000] {
            assert!(packet_ok(2, MAX, ms(n), bps), "{bps} {n} ms");
        }
        // Timer driven: one packet, rounded up to whole pages by ACX; from
        // the smallest (one page) to the 2 s WASAPI accepts in push mode.
        assert!(packet_ok(1, MAX, PAGE, bps), "{bps}");
        for n in [10, 30, 100, 500, 1000, 2000] {
            assert!(packet_ok(1, MAX, ms(n).div_ceil(PAGE) * PAGE, bps), "{bps} {n} ms");
        }
    }
}

/// What the finding used is refused.
#[test]
fn abusive_requests_are_refused() {
    for (bps, frame) in RATES {
        // Nothing, one frame, a quarter of a millisecond.
        assert!(!packet_ok(2, MAX, 0, bps));
        assert!(!packet_ok(2, MAX, frame, bps));
        assert!(!packet_ok(2, MAX, bps / 4000, bps));
        // The old limit let exactly 1 GiB per packet through.
        assert!(!packet_ok(2, MAX, 0x4000_0000, bps));
        assert!(!packet_ok(1, MAX, 0x8000_0000, bps));
        assert!(!packet_ok(2, MAX, u32::MAX, bps));
        // 64 MiB and 128 MiB, the sizes that surely map.
        assert!(!packet_ok(2, MAX, 64 << 20, bps));
        assert!(!packet_ok(2, MAX, 128 << 20, bps));
        // Just past five seconds.
        assert!(!packet_ok(2, MAX, bps * 5 + frame, bps));
        // Wrong packet counts.
        assert!(!packet_ok(0, MAX, bps / 100, bps));
        assert!(!packet_ok(3, MAX, bps / 100, bps));
        assert!(!packet_ok(u32::MAX, MAX, bps / 100, bps));
    }
}

/// The byte rate comes from the stream's format. Whatever it says, a packet
/// is never empty and never larger than the absolute cap, and the bounds
/// are in order.
#[test]
fn any_byte_rate_gives_sane_bounds() {
    for bps in [0, 1, 7, 999, 1000, 1001, 8_000, 192_000, 768_000, 5_644_800, 100_000_000, u32::MAX - 1, u32::MAX] {
        let (min, max) = packet_bounds(bps);
        assert!(min >= 1, "{bps}");
        assert!(min <= max, "{bps}");
        if (bps as u64) < MAX_PACKET_BYTES * 1000 {
            assert!(max as u64 <= MAX_PACKET_BYTES, "{bps}");
        }
        assert!(!packet_ok(2, MAX, 0, bps));
    }
    // 768000 bytes/s is the largest offered format: its 5 s are below the
    // absolute cap, so the cap never cuts into a real request.
    assert!(packet_bounds(768_000).1 as u64 <= MAX_PACKET_BYTES);
    // A byte rate above 1.6 MB/s is cut by the absolute cap.
    assert_eq!(packet_bounds(4_000_000).1 as u64, MAX_PACKET_BYTES);
}

/// The figure the documentation gives for the worst case.
#[test]
fn worst_case_nonpaged_pool_is_about_31_megabytes() {
    let per_packet = packet_bounds(768_000).1 as u64;
    assert_eq!(MAX_STREAMS as u64 * MAX as u64 * per_packet, 30_720_000);
}

#[test]
fn stream_slots_stop_at_the_maximum_and_come_back() {
    let s = StreamSlots::new();
    for n in 0..MAX_STREAMS {
        assert_eq!(s.in_use(), n);
        assert!(s.try_acquire(MAX_STREAMS));
    }
    assert!(!s.try_acquire(MAX_STREAMS));
    assert!(!s.try_acquire(MAX_STREAMS));
    assert_eq!(s.in_use(), MAX_STREAMS);
    s.release();
    assert_eq!(s.in_use(), MAX_STREAMS - 1);
    assert!(s.try_acquire(MAX_STREAMS));
    assert!(!s.try_acquire(MAX_STREAMS));
    for _ in 0..MAX_STREAMS {
        s.release();
    }
    assert_eq!(s.in_use(), 0);
}

#[test]
fn a_release_without_a_slot_does_not_wrap() {
    let s = StreamSlots::new();
    s.release();
    s.release();
    assert_eq!(s.in_use(), 0);
    assert!(s.try_acquire(1));
    assert!(!s.try_acquire(1));
}

/// Pin instances are created from several threads at once.
#[test]
fn concurrent_creates_never_exceed_the_maximum() {
    for _ in 0..50 {
        let s = Arc::new(StreamSlots::new());
        let got = Arc::new(AtomicU32::new(0));
        let peak = Arc::new(AtomicU32::new(0));
        let mut threads = Vec::new();
        for _ in 0..16 {
            let s = Arc::clone(&s);
            let got = Arc::clone(&got);
            let peak = Arc::clone(&peak);
            threads.push(thread::spawn(move || {
                for _ in 0..500 {
                    if s.try_acquire(MAX_STREAMS) {
                        got.fetch_add(1, Ordering::SeqCst);
                        peak.fetch_max(s.in_use(), Ordering::SeqCst);
                        s.release();
                    }
                }
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        assert!(got.load(Ordering::SeqCst) > 0);
        assert!(peak.load(Ordering::SeqCst) <= MAX_STREAMS);
        assert_eq!(s.in_use(), 0);
    }
}
