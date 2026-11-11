// SSC encoder: forward MDCT (one 1728-point block, or eight 216-point blocks).
//
// Ported from samsung-ssc-encoder src/ssc_enc_mdct.c, Copyright (c) 2026 William Ho (WillyBilly06),
// BSD-3-Clause. See NOTICE.
//
// Definition (N2 = number of coefficients, 864 long or 108 short; the block has 2*N2 samples):
//     X[k] = 2/N2 * sum_{n=0}^{2*N2-1} w[n] x[n] cos(pi/N2 * (n + 1/2 + N2/2) * (k + 1/2))
// which is the exact inverse (with overlap-add) of the SSC synthesis. The long block covers the 972 input samples
// [108 previous | 864 new] in the middle of the 1728-sample window: 378 zeros, the rising half of the 108-sample
// overlap window, ones, the falling half, 378 zeros. Short block b covers input samples [108*b, 108*b + 216) with
// the rising and falling overlap halves.
//
// The MDCT is folded into an N2-point DCT-IV, computed with an N2/2-point complex FFT (432 = 4*4*3*3*3,
// 54 = 2*3*3*3).

use crate::consts::{M, N, OVERLAP, SHORT};
use crate::math;
use crate::tables::WINDOW;

const PI_D: f64 = 3.14159265358979323846;

const FACTORS_432: [usize; 10] = [4, 108, 4, 27, 3, 9, 3, 3, 3, 1]; // (radix, remaining length)
const FACTORS_54: [usize; 8] = [2, 27, 3, 9, 3, 3, 3, 1];

#[derive(Clone, Copy, Default)]
#[cfg_attr(feature = "alloc", derive(bytemuck::Zeroable))]
struct Cpx {
    r: f32,
    i: f32,
}

impl Cpx {
    const ZERO: Cpx = Cpx { r: 0.0, i: 0.0 };
}

#[inline(always)]
fn cmul(a: Cpx, b: Cpx) -> Cpx {
    let (ar, ai) = (a.r, a.i);
    Cpx { r: ar * b.r - ai * b.i, i: ar * b.i + ai * b.r }
}

#[cfg_attr(feature = "alloc", derive(bytemuck::Zeroable))]
pub(crate) struct Mdct {
    tw_long: [Cpx; 432],
    tw_short: [Cpx; 54],
    pre_long: [Cpx; 432],
    post_long: [Cpx; 432],
    pre_short: [Cpx; 54],
    post_short: [Cpx; 54],
    win_long: [f32; 2 * N],
    win_short: [f32; 2 * SHORT],
}

fn fill_tw(tw: &mut [Cpx], n: usize) {
    for (k, t) in tw.iter_mut().enumerate().take(n) {
        let a = -2.0 * PI_D * k as f64 / n as f64;
        t.r = math::cos(a) as f32;
        t.i = math::sin(a) as f32;
    }
}

/// DCT-IV of size N2 via an M = N2/2 point FFT:
///   z[n] = (v[2n] + i v[N2-1-2n]) * exp(-i pi n / N2)
///   S[k] = FFT_M(z)[k] * exp(-i pi (k + 1/4) / N2)
///   u[2k] = Re S[k],  u[N2-1-2k] = -Im S[k]
/// The pre/post twiddles also carry the 2/N2 MDCT scale.
fn fill_prepost(pre: &mut [Cpx], post: &mut [Cpx], n2: usize) {
    let m = n2 / 2;
    let scale = 2.0 / n2 as f64;
    for k in 0..m {
        let a = -PI_D * k as f64 / n2 as f64;
        pre[k].r = math::cos(a) as f32;
        pre[k].i = math::sin(a) as f32;
        let b = -PI_D * (k as f64 + 0.25) / n2 as f64;
        post[k].r = (scale * math::cos(b)) as f32;
        post[k].i = (scale * math::sin(b)) as f32;
    }
}

impl Mdct {
    /// All-zero tables; [`Mdct::init`] fills them. A constant so that a whole encoder can start from a constant
    /// and be initialised in place (no large by-value temporaries).
    pub const ZERO: Mdct = Mdct {
        tw_long: [Cpx::ZERO; 432],
        tw_short: [Cpx::ZERO; 54],
        pre_long: [Cpx::ZERO; 432],
        post_long: [Cpx::ZERO; 432],
        pre_short: [Cpx::ZERO; 54],
        post_short: [Cpx::ZERO; 54],
        win_long: [0.0; 2 * N],
        win_short: [0.0; 2 * SHORT],
    };

    /// Computes the twiddles and windows in place.
    pub fn init(&mut self) {
        let m = self;
        fill_tw(&mut m.tw_long, 432);
        fill_tw(&mut m.tw_short, 54);
        fill_prepost(&mut m.pre_long, &mut m.post_long, N);
        fill_prepost(&mut m.pre_short, &mut m.post_short, SHORT);
        // long window: 378 zeros, rise, ones, fall, 378 zeros
        for i in 0..2 * N {
            let n = i as i32 - 378;
            let w = if n < 0 || n >= (N + OVERLAP) as i32 {
                0.0
            } else if n < OVERLAP as i32 {
                WINDOW[n as usize]
            } else if n < N as i32 {
                1.0
            } else {
                WINDOW[N + OVERLAP - 1 - n as usize]
            };
            m.win_long[i] = w;
        }
        for i in 0..OVERLAP {
            m.win_short[i] = WINDOW[i];
            m.win_short[OVERLAP + i] = WINDOW[OVERLAP - 1 - i];
        }
    }

    /// One MDCT block. x: 2*n2 samples (unwindowed), win: 2*n2 window. Output with stride.
    fn block(&self, x: &[f32], long: bool, out: &mut [f32], out_off: usize, stride: usize) {
        let mut z = [Cpx::default(); 432];
        let mut zz = [Cpx::default(); 432];
        let mut xw = [0f32; 2 * N];
        let mut v = [0f32; N];
        let (n2, win, pre, post) = if long {
            (N, &self.win_long[..], &self.pre_long[..], &self.post_long[..])
        } else {
            (SHORT, &self.win_short[..], &self.pre_short[..], &self.post_short[..])
        };
        let q = n2 / 2;
        let half = n2 / 2;
        for j in 0..2 * n2 {
            xw[j] = x[j] * win[j];
        }
        // fold the 2*n2 windowed samples into the n2-point DCT-IV input
        for j in 0..q {
            v[j] = -xw[3 * q - 1 - j] - xw[j + 3 * q];
        }
        for j in q..n2 {
            v[j] = xw[j - q] - xw[3 * q - 1 - j];
        }
        for k in 0..half {
            let t = Cpx { r: v[2 * k], i: v[n2 - 1 - 2 * k] };
            z[k] = cmul(t, pre[k]);
        }
        if long {
            fft_rec(&mut zz[..432], &z, 0, 1, &FACTORS_432, &self.tw_long);
        } else {
            fft_rec(&mut zz[..54], &z, 0, 1, &FACTORS_54, &self.tw_short);
        }
        for k in 0..half {
            let s = cmul(zz[k], post[k]);
            out[out_off + 2 * k * stride] = s.r;
            out[out_off + (n2 - 1 - 2 * k) * stride] = -s.i;
        }
    }

    /// input: N + OVERLAP samples (108 old + 864 new). out: 864 coefficients in 16-bit PCM units, either one long
    /// block or 8 interleaved short blocks (out[j*8+b]).
    pub fn frame(&self, input: &[f32], short_blocks: bool, out: &mut [f32]) {
        if !short_blocks {
            // Long block: the 972 input samples start at offset 378 of the 1728-point block. Only the windowed
            // part is non-zero, so build the block from input with zero padding.
            let mut x = [0f32; 2 * N];
            x[378..378 + N + OVERLAP].copy_from_slice(&input[..N + OVERLAP]);
            self.block(&x, true, out, 0, 1);
        } else {
            for b in 0..M as usize {
                self.block(&input[SHORT * b..SHORT * b + 2 * SHORT], false, out, b, M as usize);
            }
        }
    }
}

/// Recursive decimation-in-time mixed-radix DFT: out[k] = sum_n in[n*in_stride] exp(-2 pi i n k / N).
fn fft_rec(out: &mut [Cpx], inp: &[Cpx], in_off: usize, fstride: usize, factors: &[usize], tw: &[Cpx]) {
    let p = factors[0];
    let m = factors[1];
    if m == 1 {
        for q in 0..p {
            out[q] = inp[in_off + q * fstride];
        }
    } else {
        for q in 0..p {
            fft_rec(&mut out[q * m..(q + 1) * m], inp, in_off + q * fstride, fstride * p, &factors[2..], tw);
        }
    }
    let f = out;
    if p == 4 {
        for k in 0..m {
            let s0 = cmul(f[k + m], tw[k * fstride]);
            let s1 = cmul(f[k + 2 * m], tw[2 * k * fstride]);
            let s2 = cmul(f[k + 3 * m], tw[3 * k * fstride]);
            let s5 = Cpx { r: f[k].r - s1.r, i: f[k].i - s1.i };
            f[k].r += s1.r;
            f[k].i += s1.i;
            let s3 = Cpx { r: s0.r + s2.r, i: s0.i + s2.i };
            let s4 = Cpx { r: s0.r - s2.r, i: s0.i - s2.i };
            f[k + 2 * m].r = f[k].r - s3.r;
            f[k + 2 * m].i = f[k].i - s3.i;
            f[k].r += s3.r;
            f[k].i += s3.i;
            f[k + m].r = s5.r + s4.i;
            f[k + m].i = s5.i - s4.r;
            f[k + 3 * m].r = s5.r - s4.i;
            f[k + 3 * m].i = s5.i + s4.r;
        }
    } else if p == 3 {
        let epi3_i = tw[m * fstride].i; // -sin(2 pi / 3)
        for k in 0..m {
            let s1 = cmul(f[k + m], tw[k * fstride]);
            let s2 = cmul(f[k + 2 * m], tw[2 * k * fstride]);
            let s3 = Cpx { r: s1.r + s2.r, i: s1.i + s2.i };
            let s0 = Cpx { r: (s1.r - s2.r) * epi3_i, i: (s1.i - s2.i) * epi3_i };
            f[k + m].r = f[k].r - 0.5 * s3.r;
            f[k + m].i = f[k].i - 0.5 * s3.i;
            f[k].r += s3.r;
            f[k].i += s3.i;
            f[k + 2 * m].r = f[k + m].r + s0.i;
            f[k + 2 * m].i = f[k + m].i - s0.r;
            f[k + m].r -= s0.i;
            f[k + m].i += s0.r;
        }
    } else {
        // p == 2
        for k in 0..m {
            let t = cmul(f[k + m], tw[k * fstride]);
            f[k + m].r = f[k].r - t.r;
            f[k + m].i = f[k].i - t.i;
            f[k].r += t.r;
            f[k].i += t.i;
        }
    }
}
