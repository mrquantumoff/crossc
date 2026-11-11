//! `cb_refs::CbRefs`: who frees the block bthport's indication callback is
//! called with (crash review F2). The rule under test: the block is freed
//! exactly once, by whoever lets go last, and never while the device or a
//! registration of bthport still holds it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::vec::Vec;

use crate::cb_refs::{CbRefs, Released};
use crate::ring_tests::Rng;

#[test]
fn a_device_without_channels_frees_the_block_itself() {
    let r = CbRefs::new();
    assert_eq!(r.registrations(), 0);
    assert_eq!(r.release_device(), Released::Last);
}

#[test]
fn channels_closed_before_the_device_goes_leave_the_free_to_the_device() {
    let r = CbRefs::new();
    for _ in 0..3 {
        r.add();
    }
    assert_eq!(r.registrations(), 3);
    for left in (0..3).rev() {
        assert_eq!(r.release(), Released::Kept);
        assert_eq!(r.registrations(), left);
    }
    assert_eq!(r.release_device(), Released::Last);
}

/// The case of the finding: the device is deleted while bthport still has a
/// channel. The block must stay, and bthport's release frees it.
#[test]
fn a_registration_that_outlives_the_device_keeps_the_block() {
    let r = CbRefs::new();
    r.add();
    r.add();
    assert_eq!(r.release_device(), Released::Kept);
    assert_eq!(r.registrations(), 2);
    assert_eq!(r.release(), Released::Kept);
    assert_eq!(r.release(), Released::Last);
}

#[test]
fn a_release_without_a_registration_changes_nothing() {
    let r = CbRefs::new();
    assert_eq!(r.release(), Released::Underflow);
    assert_eq!(r.release(), Released::Underflow);
    // The device still holds it, and still frees it.
    r.add();
    assert_eq!(r.release(), Released::Kept);
    assert_eq!(r.release(), Released::Underflow);
    assert_eq!(r.release_device(), Released::Last);
}

#[test]
fn the_device_lets_go_only_once() {
    let r = CbRefs::new();
    r.add();
    assert_eq!(r.release_device(), Released::Kept);
    assert_eq!(r.release_device(), Released::Underflow);
    assert_eq!(r.registrations(), 1);
    assert_eq!(r.release(), Released::Last);
}

/// Random histories of the kind bthport can produce: it registers a channel
/// only while it processes an OPEN, i.e. while the device holds the block;
/// releases and the one device release come in any order. `Last` comes
/// exactly once and is the answer to the very last call.
#[test]
fn every_history_frees_exactly_once_at_its_end() {
    let mut rng = Rng(0x00C0_FFEE_1234_5678);
    for _ in 0..5_000 {
        let r = CbRefs::new();
        let mut adds_left = rng.below(8);
        let mut device = true;
        let mut held = 0u32;
        let mut lasts = 0;
        // 0 = add, 1 = release, 2 = device release
        while device || held > 0 {
            let mut choices = Vec::new();
            if device && adds_left > 0 {
                choices.push(0);
            }
            if held > 0 {
                choices.push(1);
            }
            if device {
                choices.push(2);
            }
            let result = match choices[rng.below(choices.len())] {
                0 => {
                    r.add();
                    adds_left -= 1;
                    held += 1;
                    Released::Kept
                }
                1 => {
                    held -= 1;
                    r.release()
                }
                _ => {
                    device = false;
                    r.release_device()
                }
            };
            assert_eq!(r.registrations(), held);
            assert_ne!(result, Released::Underflow);
            let everybody_let_go = !device && held == 0;
            assert_eq!(result == Released::Last, everybody_let_go, "freed early or not at all");
            if result == Released::Last {
                lasts += 1;
            }
        }
        assert_eq!(lasts, 1);
    }
}

/// Channels open and close on several processors while the device is
/// alive: nobody but the device gets `Last`.
#[test]
fn concurrent_registrations_never_free_under_a_live_device() {
    for round in 0..50 {
        let r = Arc::new(CbRefs::new());
        let lasts = Arc::new(AtomicU32::new(0));
        let mut threads = Vec::new();
        for t in 0..8 {
            let r = Arc::clone(&r);
            let lasts = Arc::clone(&lasts);
            threads.push(thread::spawn(move || {
                for _ in 0..2_000 {
                    r.add();
                    match r.release() {
                        Released::Last => {
                            lasts.fetch_add(1, Ordering::SeqCst);
                        }
                        Released::Kept => {}
                        Released::Underflow => panic!("underflow in thread {t}"),
                    }
                }
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(lasts.load(Ordering::SeqCst), 0, "round {round}");
        assert_eq!(r.registrations(), 0);
        assert_eq!(r.release_device(), Released::Last);
    }
}

/// Device cleanup racing the releases of the channels bthport still had:
/// all registrations exist before, releases and the device release race.
#[test]
fn releases_racing_the_device_release_free_exactly_once() {
    for round in 0..200 {
        let r = Arc::new(CbRefs::new());
        let lasts = Arc::new(AtomicU32::new(0));
        const CHANNELS: usize = 4;
        for _ in 0..CHANNELS {
            r.add();
        }
        let mut threads = Vec::new();
        for _ in 0..CHANNELS {
            let r = Arc::clone(&r);
            let lasts = Arc::clone(&lasts);
            threads.push(thread::spawn(move || match r.release() {
                Released::Last => {
                    lasts.fetch_add(1, Ordering::SeqCst);
                }
                Released::Kept => {}
                Released::Underflow => panic!("underflow"),
            }));
        }
        {
            let r = Arc::clone(&r);
            let lasts = Arc::clone(&lasts);
            threads.push(thread::spawn(move || match r.release_device() {
                Released::Last => {
                    lasts.fetch_add(1, Ordering::SeqCst);
                }
                Released::Kept => {}
                Released::Underflow => panic!("device underflow"),
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(lasts.load(Ordering::SeqCst), 1, "round {round}");
        assert_eq!(r.registrations(), 0);
    }
}
