use std::collections::VecDeque;
use std::vec;
use std::vec::Vec;

use crate::pcm_ring::Ring;

/// Small deterministic generator (xorshift64*), so failures reproduce.
pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn empty_ring_without_storage_does_nothing() {
    let mut r = Ring::new();
    let mut none: [u8; 0] = [];
    assert_eq!((r.capacity(), r.len(), r.free()), (0, 0, 0));
    assert!(r.is_empty());
    assert!(r.push(&mut none, &[]));
    assert!(!r.push(&mut none, &[1]));
    assert!(!r.push_zeros(&mut none, 1));
    assert_eq!(r.pop(&none, &mut [0; 4]), 0);
    r.discard(10);
    r.clear();
    assert_eq!(r, Ring::new());
}

#[test]
fn push_pop_in_order() {
    let mut s = vec![0u8; 8];
    let mut r = Ring::new();
    r.set_capacity(8);
    assert!(r.push(&mut s, &[1, 2, 3]));
    assert!(r.push(&mut s, &[4, 5]));
    assert_eq!((r.len(), r.free()), (5, 3));
    let mut out = [0u8; 4];
    assert_eq!(r.pop(&s, &mut out), 4);
    assert_eq!(out, [1, 2, 3, 4]);
    assert_eq!(r.pop(&s, &mut out), 1);
    assert_eq!(out[0], 5);
    assert!(r.is_empty());
    assert_eq!(r.pop(&s, &mut out), 0);
}

#[test]
fn wraps_around_the_end() {
    let mut s = vec![0u8; 8];
    let mut r = Ring::new();
    r.set_capacity(8);
    assert!(r.push(&mut s, &[1, 2, 3, 4, 5, 6]));
    let mut out = [0u8; 5];
    assert_eq!(r.pop(&s, &mut out), 5);
    // head = 5, one byte left; this push crosses the end of the storage.
    assert!(r.push(&mut s, &[7, 8, 9, 10, 11, 12]));
    assert_eq!(r.len(), 7);
    let mut out = [0u8; 7];
    assert_eq!(r.pop(&s, &mut out), 7);
    assert_eq!(out, [6, 7, 8, 9, 10, 11, 12]);
}

#[test]
fn full_ring_refuses_and_keeps_its_content() {
    let mut s = vec![0u8; 4];
    let mut r = Ring::new();
    r.set_capacity(4);
    assert!(r.push(&mut s, &[1, 2, 3, 4]));
    assert_eq!(r.free(), 0);
    assert!(!r.push(&mut s, &[5]));
    assert!(!r.push_zeros(&mut s, 1));
    let mut out = [0u8; 4];
    assert_eq!(r.pop(&s, &mut out), 4);
    assert_eq!(out, [1, 2, 3, 4]);
}

#[test]
fn discard_drops_the_oldest() {
    let mut s = vec![0u8; 6];
    let mut r = Ring::new();
    r.set_capacity(6);
    assert!(r.push(&mut s, &[1, 2, 3, 4, 5]));
    r.discard(2);
    assert_eq!(r.len(), 3);
    assert!(r.push(&mut s, &[6, 7, 8]));
    let mut out = [0u8; 6];
    assert_eq!(r.pop(&s, &mut out), 6);
    assert_eq!(out, [3, 4, 5, 6, 7, 8]);
    assert!(r.push(&mut s, &[9]));
    r.discard(100);
    assert!(r.is_empty());
}

#[test]
fn zeros_overwrite_old_bytes() {
    let mut s = vec![0xAAu8; 4];
    let mut r = Ring::new();
    r.set_capacity(4);
    assert!(r.push(&mut s, &[1, 2, 3]));
    r.discard(3);
    // tail = 3: one zero at the end, two at the start.
    assert!(r.push_zeros(&mut s, 3));
    let mut out = [0xFFu8; 3];
    assert_eq!(r.pop(&s, &mut out), 3);
    assert_eq!(out, [0, 0, 0]);
}

#[test]
fn wrong_storage_is_refused() {
    let mut s = vec![0u8; 8];
    let mut short = vec![0u8; 7];
    let mut r = Ring::new();
    r.set_capacity(8);
    assert!(!r.push(&mut short, &[1]));
    assert!(!r.push_zeros(&mut short, 1));
    assert!(r.push(&mut s, &[1, 2]));
    assert_eq!(r.pop(&short, &mut [0; 2]), 0);
    assert_eq!(r.len(), 2);
}

#[test]
fn set_capacity_resets() {
    let mut s = vec![0u8; 8];
    let mut r = Ring::new();
    r.set_capacity(8);
    assert!(r.push(&mut s, &[1, 2, 3]));
    r.set_capacity(4);
    assert_eq!((r.capacity(), r.len()), (4, 0));
}

/// Random operations against a `VecDeque` model, for several capacities.
#[test]
fn matches_a_queue_model() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    for cap in [1usize, 2, 3, 7, 8, 64, 255] {
        let mut s = vec![0u8; cap];
        let mut r = Ring::new();
        r.set_capacity(cap);
        let mut model: VecDeque<u8> = VecDeque::new();
        let mut next = 0u8;
        for _ in 0..20_000 {
            match rng.below(5) {
                0 | 1 => {
                    let n = rng.below(cap + 3);
                    let data: Vec<u8> = (0..n)
                        .map(|_| {
                            next = next.wrapping_add(1);
                            next
                        })
                        .collect();
                    let fits = n <= cap - model.len();
                    assert_eq!(r.push(&mut s, &data), fits);
                    if fits {
                        model.extend(data);
                    }
                }
                2 => {
                    let n = rng.below(cap + 3);
                    let fits = n <= cap - model.len();
                    assert_eq!(r.push_zeros(&mut s, n), fits);
                    if fits {
                        model.extend(core::iter::repeat_n(0, n));
                    }
                }
                3 => {
                    let n = rng.below(cap + 3);
                    let mut out = vec![0xEEu8; n];
                    let got = r.pop(&s, &mut out);
                    assert_eq!(got, n.min(model.len()));
                    let want: Vec<u8> = model.drain(..got).collect();
                    assert_eq!(&out[..got], &want[..]);
                    assert!(out[got..].iter().all(|&b| b == 0xEE));
                }
                _ => {
                    let n = rng.below(cap + 3);
                    r.discard(n);
                    model.drain(..n.min(model.len()));
                }
            }
            assert_eq!(r.len(), model.len());
            assert_eq!(r.free(), cap - model.len());
            assert_eq!(r.is_empty(), model.is_empty());
        }
    }
}
