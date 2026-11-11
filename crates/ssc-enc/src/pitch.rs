// SSC encoder: pitch analysis and the comb prefilter (the decoder runs the inverse postfilter).
//
// Ported from samsung-ssc-encoder src/ssc_enc_pitch.c, Copyright (c) 2026 William Ho (WillyBilly06),
// BSD-3-Clause, itself adapted from libopus 1.1 celt/pitch.c, celt/celt_lpc.c and celt/celt.c, float build
// (Copyright 2007-2009 Xiph.Org Foundation, 2007-2008 CSIRO, BSD-3-Clause). See NOTICE.
//
// SSC has no tapset symbol: the filter always uses CELT tapset 0. C pointer arithmetic with negative offsets is
// expressed as a base index into the full buffer.

use crate::consts::N;
use crate::math::sqrtf;
use crate::tables::WINDOW;

const COMB_GAINS: [f32; 3] = [0.3066406250, 0.2170410156, 0.1296386719];
const MAX_PITCH_LAG: usize = 1024;

fn celt_lpc(lpc: &mut [f32; 4], ac: &[f32; 5], p: usize) {
    let mut error = ac[0];
    for v in lpc.iter_mut() {
        *v = 0.0;
    }
    if ac[0] == 0.0 {
        return;
    }
    for i in 0..p {
        let mut rr = 0f32;
        for j in 0..i {
            rr += lpc[j] * ac[i - j];
        }
        rr += ac[i + 1];
        let r = -rr / error;
        lpc[i] = r;
        for j in 0..(i + 1) >> 1 {
            let t1 = lpc[j];
            let t2 = lpc[i - 1 - j];
            lpc[j] = t1 + r * t2;
            lpc[i - 1 - j] = t2 + r * t1;
        }
        error = error - r * r * error;
        if error < 0.001 * ac[0] {
            break; // 30 dB prediction gain is enough
        }
    }
}

/// Low-pass and decimate by 2, sum of the channels, then a 4th-order LPC whitening (with an extra zero).
pub(crate) fn pitch_downsample(x0: &[f32], x1: &[f32], x_lp: &mut [f32], len: usize) {
    let mut ac = [0f32; 5];
    let mut lpc = [0f32; 4];
    let mut lpc2 = [0f32; 5];
    let mut mem = [0f32; 5];
    let mut tmp = 1f32;
    let c1 = 0.8f32;
    let half = len >> 1;
    for i in 1..half {
        x_lp[i] = 0.5 * (0.5 * (x0[2 * i - 1] + x0[2 * i + 1]) + x0[2 * i])
            + 0.5 * (0.5 * (x1[2 * i - 1] + x1[2 * i + 1]) + x1[2 * i]);
    }
    x_lp[0] = 0.5 * (0.5 * x0[1] + x0[0]) + 0.5 * (0.5 * x1[1] + x1[0]);
    for k in 0..=4 {
        let mut s = 0f64;
        for i in k..half {
            s += x_lp[i] as f64 * x_lp[i - k] as f64;
        }
        ac[k] = s as f32;
    }
    ac[0] *= 1.0001; // noise floor -40 dB
    for i in 1..=4 {
        // lag window
        ac[i] -= ac[i] * (0.008 * i as f32) * (0.008 * i as f32);
    }
    celt_lpc(&mut lpc, &ac, 4);
    for v in lpc.iter_mut() {
        tmp *= 0.9;
        *v *= tmp;
    }
    lpc2[0] = lpc[0] + 0.8;
    lpc2[1] = lpc[1] + c1 * lpc[0];
    lpc2[2] = lpc[2] + c1 * lpc[1];
    lpc2[3] = lpc[3] + c1 * lpc[2];
    lpc2[4] = c1 * lpc[3];
    for i in 0..half {
        let x = x_lp[i];
        let sum = x + lpc2[0] * mem[0] + lpc2[1] * mem[1] + lpc2[2] * mem[2] + lpc2[3] * mem[3] + lpc2[4] * mem[4];
        mem[4] = mem[3];
        mem[3] = mem[2];
        mem[2] = mem[1];
        mem[1] = mem[0];
        mem[0] = x;
        x_lp[i] = sum;
    }
}

fn find_best_pitch(xcorr: &[f32], y: &[f32], len: usize, max_pitch: usize, best_pitch: &mut [i32; 2]) {
    let mut syy = 1f32;
    let mut best_num = [-1f32, -1f32];
    let mut best_den = [0f32, 0f32];
    best_pitch[0] = 0;
    best_pitch[1] = 1;
    for j in 0..len {
        syy += y[j] * y[j];
    }
    for i in 0..max_pitch {
        if xcorr[i] > 0.0 {
            let xc = xcorr[i] * 1e-12;
            let num = xc * xc;
            if num * best_den[1] > best_num[1] * syy {
                if num * best_den[0] > best_num[0] * syy {
                    best_num[1] = best_num[0];
                    best_den[1] = best_den[0];
                    best_pitch[1] = best_pitch[0];
                    best_num[0] = num;
                    best_den[0] = syy;
                    best_pitch[0] = i as i32;
                } else {
                    best_num[1] = num;
                    best_den[1] = syy;
                    best_pitch[1] = i as i32;
                }
            }
        }
        syy += y[i + len] * y[i + len] - y[i] * y[i];
        if syy < 1.0 {
            syy = 1.0;
        }
    }
}

/// Open-loop pitch search on the decimated signal: coarse at 4x decimation, refined at 2x.
pub(crate) fn pitch_search(x_lp: &[f32], y: &[f32], len: usize, max_pitch: usize) -> i32 {
    let mut x_lp4 = [0f32; N / 4 + 1];
    let mut y_lp4 = [0f32; (N + MAX_PITCH_LAG) / 4 + 1];
    let mut xcorr = [0f32; MAX_PITCH_LAG / 2];
    let lag = len + max_pitch;
    let mut best_pitch = [0i32, 0i32];
    for j in 0..len >> 2 {
        x_lp4[j] = x_lp[2 * j];
    }
    for j in 0..lag >> 2 {
        y_lp4[j] = y[2 * j];
    }
    for i in 0..max_pitch >> 2 {
        let mut s = 0f32;
        for j in 0..len >> 2 {
            s += x_lp4[j] * y_lp4[i + j];
        }
        xcorr[i] = s;
    }
    find_best_pitch(&xcorr, &y_lp4, len >> 2, max_pitch >> 2, &mut best_pitch);
    for i in 0..max_pitch >> 1 {
        let mut s = 0f32;
        xcorr[i] = 0.0;
        let ii = i as i32;
        if (ii - 2 * best_pitch[0]).abs() > 2 && (ii - 2 * best_pitch[1]).abs() > 2 {
            continue;
        }
        for j in 0..len >> 1 {
            s += x_lp[j] * y[i + j];
        }
        xcorr[i] = if s > -1.0 { s } else { -1.0 };
    }
    find_best_pitch(&xcorr, y, len >> 1, max_pitch >> 1, &mut best_pitch);
    let mut offset = 0;
    if best_pitch[0] > 0 && best_pitch[0] < (max_pitch as i32 >> 1) - 1 {
        let bp = best_pitch[0] as usize;
        let a = xcorr[bp - 1];
        let b = xcorr[bp];
        let c = xcorr[bp + 1];
        if (c - a) > 0.7 * (b - a) {
            offset = 1;
        } else if (a - c) > 0.7 * (b - c) {
            offset = -1;
        }
    }
    2 * best_pitch[0] - offset
}

/// Checks the sub-multiples of the period (octave errors) and returns the pitch gain. x: decimated signal,
/// maxperiod/2 history samples followed by N/2 samples.
pub(crate) fn remove_doubling(
    x: &[f32],
    maxperiod: i32,
    minperiod: i32,
    n: i32,
    t0_: &mut i32,
    prev_period: i32,
    prev_gain: f32,
) -> f32 {
    const SECOND_CHECK: [i32; 16] = [0, 0, 3, 2, 3, 2, 5, 2, 3, 2, 3, 2, 5, 2, 3, 2];
    let mut yy_lookup = [0f32; MAX_PITCH_LAG / 2 + 1];
    let mut xcorr = [0f32; 3];
    let mut xx = 0f32;
    let mut xy = 0f32;
    let minperiod0 = minperiod;
    let maxperiod = maxperiod / 2;
    let minperiod = minperiod / 2;
    *t0_ /= 2;
    let prev_period = prev_period / 2;
    let n = n / 2;
    let base = maxperiod as isize; // x += maxperiod
    let at = |i: i32| x[(base + i as isize) as usize];
    if *t0_ >= maxperiod {
        *t0_ = maxperiod - 1;
    }
    let t0 = *t0_;
    let mut t = t0;
    for i in 0..n {
        xx += at(i) * at(i);
        xy += at(i) * at(i - t0);
    }
    yy_lookup[0] = xx;
    let mut yy = xx;
    for i in 1..=maxperiod {
        yy = yy + at(-i) * at(-i) - at(n - i) * at(n - i);
        yy_lookup[i as usize] = if yy > 0.0 { yy } else { 0.0 };
    }
    yy = yy_lookup[t0 as usize];
    let mut best_xy = xy;
    let mut best_yy = yy;
    let g0 = xy / sqrtf(1.0 + xx * yy);
    let mut g = g0;
    for k in 2..=15 {
        let t1 = (2 * t0 + k) / (2 * k);
        if t1 < minperiod {
            break;
        }
        let t1b = if k == 2 {
            if t1 + t0 > maxperiod { t0 } else { t0 + t1 }
        } else {
            (2 * SECOND_CHECK[k as usize] * t0 + k) / (2 * k)
        };
        xy = 0.0;
        let mut xy2 = 0f32;
        for i in 0..n {
            xy += at(i) * at(i - t1);
            xy2 += at(i) * at(i - t1b);
        }
        xy += xy2;
        yy = yy_lookup[t1 as usize] + yy_lookup[t1b as usize];
        let g1 = xy / sqrtf(1.0 + 2.0 * xx * yy);
        let cont = if (t1 - prev_period).abs() <= 1 {
            prev_gain
        } else if (t1 - prev_period).abs() <= 2 && 5 * k * k < t0 {
            0.5 * prev_gain
        } else {
            0.0
        };
        let mut thresh = 0.7 * g0 - cont;
        if thresh < 0.3 {
            thresh = 0.3;
        }
        if t1 < 3 * minperiod {
            thresh = 0.85 * g0 - cont;
            if thresh < 0.4 {
                thresh = 0.4;
            }
        } else if t1 < 2 * minperiod {
            // unreachable (kept from the reference: t1 < 2*minperiod implies t1 < 3*minperiod)
            thresh = 0.9 * g0 - cont;
            if thresh < 0.5 {
                thresh = 0.5;
            }
        }
        if g1 > thresh {
            best_xy = xy;
            best_yy = yy;
            t = t1;
            g = g1;
        }
    }
    if best_xy < 0.0 {
        best_xy = 0.0;
    }
    let mut pg = if best_yy <= best_xy { 1.0 } else { best_xy / (best_yy + 1.0) };
    for k in 0..3 {
        let t1 = t + k - 1;
        let mut s = 0f32;
        for i in 0..n {
            s += at(i) * at(i - t1);
        }
        xcorr[k as usize] = s;
    }
    let offset = if (xcorr[2] - xcorr[0]) > 0.7 * (xcorr[1] - xcorr[0]) {
        1
    } else if (xcorr[0] - xcorr[2]) > 0.7 * (xcorr[1] - xcorr[2]) {
        -1
    } else {
        0
    };
    if pg > g {
        pg = g;
    }
    *t0_ = 2 * t + offset;
    if *t0_ < minperiod0 {
        *t0_ = minperiod0;
    }
    pg
}

/// y[i] = x[i] + g * (3-tap comb at period T), crossfading from (T0, g0) to (T1, g1) over the first `overlap`
/// samples with the squared window. x[xb..] is the current input; it needs T + 2 samples of history before xb.
/// The encoder calls it with negative gains (FIR prefilter); the decoder runs the IIR inverse with positive gains.
pub(crate) fn comb_filter(
    y: &mut [f32],
    x: &[f32],
    xb: usize,
    t0: i32,
    t1: i32,
    n: usize,
    g0: f32,
    g1: f32,
    overlap: usize,
) {
    let g00 = g0 * COMB_GAINS[0];
    let g01 = g0 * COMB_GAINS[1];
    let g02 = g0 * COMB_GAINS[2];
    let g10 = g1 * COMB_GAINS[0];
    let g11 = g1 * COMB_GAINS[1];
    let g12 = g1 * COMB_GAINS[2];
    if g0 == 0.0 && g1 == 0.0 {
        y[..n].copy_from_slice(&x[xb..xb + n]);
        return;
    }
    let at = |i: isize| x[(xb as isize + i) as usize];
    let (t0, t1) = (t0 as isize, t1 as isize);
    let mut x1 = at(-t1 + 1);
    let mut x2 = at(-t1);
    let mut x3 = at(-t1 - 1);
    let mut x4 = at(-t1 - 2);
    let mut i = 0usize;
    while i < overlap {
        let ii = i as isize;
        let f = WINDOW[i] * WINDOW[i];
        let x0 = at(ii - t1 + 2);
        y[i] = at(ii)
            + (1.0 - f) * g00 * at(ii - t0)
            + (1.0 - f) * g01 * (at(ii - t0 + 1) + at(ii - t0 - 1))
            + (1.0 - f) * g02 * (at(ii - t0 + 2) + at(ii - t0 - 2))
            + f * g10 * x2
            + f * g11 * (x1 + x3)
            + f * g12 * (x0 + x4);
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
        i += 1;
    }
    while i < n {
        let ii = i as isize;
        let x0 = at(ii - t1 + 2);
        y[i] = at(ii) + g10 * x2 + g11 * (x1 + x3) + g12 * (x0 + x4);
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
        i += 1;
    }
}
