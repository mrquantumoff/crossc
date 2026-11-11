// SSC encoder: band shape quantisation (PVQ), stereo and time-frequency splitting.
//
// Ported from samsung-ssc-encoder src/ssc_enc_bands.c, Copyright (c) 2026 William Ho (WillyBilly06),
// BSD-3-Clause, itself adapted from libopus 1.1 celt/bands.c, celt/vq.c and celt/cwrs.c (Copyright 2007-2009
// Xiph.Org Foundation, 2008-2009 Gregory Maxwell, 2007-2008 CSIRO, 2007-2009 Timothy B. Terriberry,
// BSD-3-Clause). See NOTICE.
//
// This is the encode half of quant_all_bands. It does not resynthesise: the decoder's folding memory, collapse
// masks and noise filling never change the bitstream syntax, only the decoded audio, so the encoder does not need
// them. What it must follow exactly is the integer bit bookkeeping (b, balance, qalloc, remaining_bits, pulse
// counts), which is a mirror of the decoder. Bands are always >= 8 bins wide (LM = 3), so CELT's N == 1 and N == 2
// special cases cannot occur.

use crate::consts::*;
use crate::ec::{Ec, ilog};
use crate::math::{atan2, d2i, f2i, fabsf, floor, floorf, sqrtf};
use crate::tables::{ModeTables, pvq_u};

const QTHETA_OFFSET: i32 = 4;
const LOG_MAX_PSEUDO: i32 = 6;

/// cos(pi/2 * x) for x in [0, 1]: even polynomial, max error 2e-5 (the decoder uses the same one, so the spreading
/// rotation it undoes is exactly the one applied here).
pub(crate) fn cos_half_pi(x: f32) -> f32 {
    let x2 = x * x;
    1.0 + x2 * (-1.2336979 + x2 * (0.25366235 + x2 * (-0.020863481 + x2 * 0.00091892557)))
}

// ---- bit-exact integer helpers (they drive the allocation inside a band) ----
#[inline]
fn frac_mul16(a: i32, b: i32) -> i32 {
    (16384 + (a as i16 as i32) * (b as i16 as i32)) >> 15
}

fn bitexact_cos(x: i32) -> i32 {
    let tmp = (4096 + x * x) >> 13;
    let mut x2 = tmp;
    x2 = (32767 - x2) + frac_mul16(x2, -7651 + frac_mul16(x2, 8277 + frac_mul16(-626, x2)));
    1 + x2
}

fn bitexact_log2tan(isin: i32, icos: i32) -> i32 {
    let lc = ilog(icos as u32);
    let ls = ilog(isin as u32);
    let icos = icos << (15 - lc);
    let isin = isin << (15 - ls);
    (ls - lc) * (1 << 11) + frac_mul16(isin, frac_mul16(isin, -2597) + 7932)
        - frac_mul16(icos, frac_mul16(icos, -2597) + 7932)
}

fn compute_qn(n: i32, b: i32, offset: i32, pulse_cap: i32) -> i32 {
    const EXP2_TABLE8: [i16; 8] = [16384, 17866, 19483, 21247, 23170, 25267, 27554, 30048];
    let n2 = 2 * n - 1;
    let mut qb = (b - pulse_cap - (4 << BITRES)).min((b + n2 * offset) / n2);
    qb = (8 << BITRES).min(qb);
    if qb < (1 << BITRES >> 1) {
        1
    } else {
        let qn = EXP2_TABLE8[(qb & 0x7) as usize] as i32 >> (14 - (qb >> BITRES));
        ((qn + 1) >> 1) << 1
    }
}

#[inline]
fn get_pulses(i: i32) -> i32 {
    if i < 8 { i } else { (8 + (i & 7)) << ((i >> 3) - 1) }
}

#[inline]
fn band_cache(m: &ModeTables, band: usize, lm: i32) -> &'static [u8] {
    &m.cache_bits[m.cache_index[(lm + 1) as usize * NB + band] as usize..]
}

fn bits2pulses(m: &ModeTables, band: usize, lm: i32, mut bits: i32) -> i32 {
    let cache = band_cache(m, band, lm);
    let mut lo = 0i32;
    let mut hi = cache[0] as i32;
    bits -= 1;
    for _ in 0..LOG_MAX_PSEUDO {
        let mid = (lo + hi + 1) >> 1;
        if cache[mid as usize] as i32 >= bits {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    if bits - (if lo == 0 { -1 } else { cache[lo as usize] as i32 }) <= cache[hi as usize] as i32 - bits {
        lo
    } else {
        hi
    }
}

#[inline]
fn pulses2bits(m: &ModeTables, band: usize, lm: i32, pulses: i32) -> i32 {
    let cache = band_cache(m, band, lm);
    if pulses == 0 { 0 } else { cache[pulses as usize] as i32 + 1 }
}

// ---- PVQ codeword index (cwrs.c, table form) ----
#[inline]
fn pvq_v(n: i32, k: i32) -> u32 {
    pvq_u(n, k).wrapping_add(pvq_u(n, k + 1))
}

fn icwrs(n: i32, y: &[i32]) -> u32 {
    let mut j = n - 1;
    let yj = y[j as usize];
    let mut i: u32 = (yj < 0) as u32;
    let mut k = yj.abs();
    loop {
        j -= 1;
        i = i.wrapping_add(pvq_u(n - j, k));
        let yj = y[j as usize];
        k += yj.abs();
        if yj < 0 {
            i = i.wrapping_add(pvq_u(n - j, k + 1));
        }
        if j <= 0 {
            break;
        }
    }
    i
}

fn encode_pulses(y: &[i32], n: i32, k: i32, ec: &mut Ec) {
    ec.uint(icwrs(n, y), pvq_v(n, k));
}

// ---- vq.c ----
fn exp_rotation1(x: &mut [f32], len: usize, stride: usize, c: f32, s: f32) {
    for i in 0..len - stride {
        let x1 = x[i];
        let x2 = x[i + stride];
        x[i + stride] = c * x2 + s * x1;
        x[i] = c * x1 - s * x2;
    }
    let mut i = len as isize - 2 * stride as isize - 1;
    while i >= 0 {
        let iu = i as usize;
        let x1 = x[iu];
        let x2 = x[iu + stride];
        x[iu + stride] = c * x2 + s * x1;
        x[iu] = c * x1 - s * x2;
        i -= 1;
    }
}

fn exp_rotation(x: &mut [f32], len: usize, dir: i32, stride: usize, k: i32, spread: i32) {
    const SPREAD_FACTOR: [i32; 3] = [15, 10, 5];
    if 2 * k >= len as i32 || spread == SPREAD_NONE {
        return;
    }
    let factor = SPREAD_FACTOR[(spread - 1) as usize];
    let gain = len as f32 / (len as i32 + factor * k) as f32;
    let theta = 0.5 * gain * gain;
    let c = cos_half_pi(theta);
    let s = cos_half_pi(1.0 - theta);
    let mut stride2 = 0usize;
    if len >= 8 * stride {
        stride2 = 1;
        while (stride2 * stride2 + stride2) * stride + (stride >> 2) < len {
            stride2 += 1;
        }
    }
    let len = len / stride;
    for i in 0..stride {
        let xs = &mut x[i * len..];
        if dir < 0 {
            if stride2 != 0 {
                exp_rotation1(xs, len, stride2, s, c);
            }
            exp_rotation1(xs, len, 1, c, s);
        } else {
            exp_rotation1(xs, len, 1, c, -s);
            if stride2 != 0 {
                exp_rotation1(xs, len, stride2, s, -c);
            }
        }
    }
}

/// Pulse search: finds the integer vector y with sum|y| = K that maximises <x,y>/|y|.
fn alg_quant(x: &mut [f32], n: usize, k: i32, spread: i32, b: usize, ec: &mut Ec) {
    let mut y = [0f32; MAX_BAND_N];
    let mut absx = [0f32; MAX_BAND_N];
    let mut iy = [0i32; MAX_BAND_N];
    let mut sum = 0f32;
    let mut xy = 0f32;
    let mut yy = 0f32;

    exp_rotation(x, n, 1, b, k, spread);
    for j in 0..n {
        absx[j] = fabsf(x[j]);
        iy[j] = 0;
        y[j] = 0.0;
    }
    let mut pulses_left = k;
    // Pre-search by projecting on the pyramid
    if k > (n as i32 >> 1) {
        for j in 0..n {
            sum += absx[j];
        }
        if !(sum > 1e-15 && sum < 64.0) {
            absx[0] = 1.0;
            for v in absx.iter_mut().take(n).skip(1) {
                *v = 0.0;
            }
            sum = 1.0;
        }
        let rcp = (k - 1) as f32 / sum;
        for j in 0..n {
            iy[j] = f2i(floorf(rcp * absx[j]));
            y[j] = iy[j] as f32;
            yy += y[j] * y[j];
            xy += absx[j] * y[j];
            y[j] *= 2.0;
            pulses_left -= iy[j];
        }
    }
    if pulses_left > n as i32 + 3 {
        let tmp = pulses_left as f32;
        yy += tmp * tmp + tmp * y[0];
        iy[0] += pulses_left;
        pulses_left = 0;
    }
    for _ in 0..pulses_left {
        let mut best_id = 0usize;
        let mut best_num = -1e10f32;
        let mut best_den = 0f32;
        yy += 1.0;
        for j in 0..n {
            let mut rxy = xy + absx[j];
            let ryy = yy + y[j];
            rxy = rxy * rxy;
            if best_den * rxy > ryy * best_num {
                best_den = ryy;
                best_num = rxy;
                best_id = j;
            }
        }
        xy += absx[best_id];
        yy += y[best_id];
        y[best_id] += 2.0;
        iy[best_id] += 1;
    }
    for j in 0..n {
        if x[j] < 0.0 {
            iy[j] = -iy[j];
        }
    }
    encode_pulses(&iy[..n], n as i32, k, ec);
}

fn stereo_itheta(x: &[f32], y: &[f32], stereo: bool, n: usize) -> i32 {
    let mut emid = 1e-15f32;
    let mut eside = 1e-15f32;
    if stereo {
        for i in 0..n {
            let m = 0.5 * (x[i] + y[i]);
            let s = 0.5 * (x[i] - y[i]);
            emid += m * m;
            eside += s * s;
        }
    } else {
        for i in 0..n {
            emid += x[i] * x[i];
            eside += y[i] * y[i];
        }
    }
    // C: floor(.5f + 16384 * 0.63662f * atan2(...)): the constant product is a float, the rest is double.
    let k = 16384.0f32 * 0.63662f32;
    d2i(floor(0.5f32 as f64 + k as f64 * atan2(sqrtf(eside) as f64, sqrtf(emid) as f64)))
}

// ---- bands.c helpers ----
pub(crate) fn haar1(x: &mut [f32], n0: usize, stride: usize) {
    let n0 = n0 >> 1;
    for i in 0..stride {
        for j in 0..n0 {
            let t1 = 0.70710678 * x[stride * 2 * j + i];
            let t2 = 0.70710678 * x[stride * (2 * j + 1) + i];
            x[stride * 2 * j + i] = t1 + t2;
            x[stride * (2 * j + 1) + i] = t1 - t2;
        }
    }
}

const ORDERY_TABLE: [usize; 30] =
    [1, 0, 3, 0, 2, 1, 7, 0, 4, 3, 6, 1, 5, 2, 15, 0, 8, 7, 12, 3, 11, 4, 14, 1, 9, 6, 13, 2, 10, 5];

fn deinterleave_hadamard(x: &mut [f32], n0: usize, stride: usize, hadamard: bool) {
    let mut tmp = [0f32; MAX_BAND_N];
    let n = n0 * stride;
    if hadamard {
        let ordery = &ORDERY_TABLE[stride - 2..];
        for i in 0..stride {
            for j in 0..n0 {
                tmp[ordery[i] * n0 + j] = x[j * stride + i];
            }
        }
    } else {
        for i in 0..stride {
            for j in 0..n0 {
                tmp[i * n0 + j] = x[j * stride + i];
            }
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

fn intensity_stereo(x: &mut [f32], y: &[f32], left: f32, right: f32, n: usize) {
    let norm = 1e-15 + sqrtf(1e-15 + left * left + right * right);
    let a1 = left / norm;
    let a2 = right / norm;
    for j in 0..n {
        x[j] = a1 * x[j] + a2 * y[j];
    }
}

fn stereo_split(x: &mut [f32], y: &mut [f32], n: usize) {
    for j in 0..n {
        let l = 0.70710678 * x[j];
        let r = 0.70710678 * y[j];
        x[j] = l + r;
        y[j] = r - l;
    }
}

// ---- band coding context ----
struct BandCtx<'a, 'b> {
    m: &'a ModeTables,
    i: usize,
    intensity: i32,
    spread: i32,
    tf_change: i32,
    ec: &'a mut Ec<'b>,
    remaining_bits: i32,
    band_e: &'a [f32; C * NB],
}

struct SplitCtx {
    delta: i32,
    itheta: i32,
    qalloc: i32,
}

fn compute_theta(
    ctx: &mut BandCtx,
    x: &mut [f32],
    y: &mut [f32],
    n: usize,
    b: &mut i32,
    b0: usize,
    lm: i32,
    stereo: bool,
) -> SplitCtx {
    let m = ctx.m;
    let i = ctx.i;
    let ni = n as i32;
    let pulse_cap = m.log_n[i] as i32 + lm * (1 << BITRES);
    let offset = (pulse_cap >> 1) - QTHETA_OFFSET;
    let mut qn = compute_qn(ni, *b, offset, pulse_cap);
    if stereo && i as i32 >= ctx.intensity {
        qn = 1;
    }
    let mut itheta = stereo_itheta(x, y, stereo, n);
    let tell = ctx.ec.tell_frac() as i32;
    if qn != 1 {
        itheta = itheta.wrapping_mul(qn).wrapping_add(8192) >> 14; // wraps like the C int math for NaN input
        if stereo && n > 2 {
            // step pdf: probability 3 up to qn/2, then 1
            let p0 = 3;
            let xx = itheta;
            let x0 = qn / 2;
            let ft = p0 * (x0 + 1) + x0;
            let fl = if xx <= x0 { p0 * xx } else { (xx - 1 - x0) + (x0 + 1) * p0 };
            let fh = if xx <= x0 { p0 * (xx + 1) } else { (xx - x0) + (x0 + 1) * p0 };
            ctx.ec.encode(fl as u32, fh as u32, ft as u32);
        } else if b0 > 1 || stereo {
            ctx.ec.uint(itheta as u32, qn as u32 + 1); // uniform
        } else {
            // triangular
            let ft = ((qn >> 1) + 1) * ((qn >> 1) + 1);
            let fs = if itheta <= (qn >> 1) { itheta + 1 } else { qn + 1 - itheta };
            let fl = if itheta <= (qn >> 1) {
                (itheta * (itheta + 1)) >> 1
            } else {
                ft - (((qn + 1 - itheta) * (qn + 2 - itheta)) >> 1)
            };
            ctx.ec.encode(fl as u32, (fl + fs) as u32, ft as u32);
        }
        itheta = itheta * 16384 / qn;
        if stereo {
            if itheta == 0 {
                intensity_stereo(x, y, ctx.band_e[i], ctx.band_e[i + NB], n);
            } else {
                stereo_split(x, y, n);
            }
        }
    } else if stereo {
        let mut inv = itheta > 8192;
        if inv {
            for v in y.iter_mut().take(n) {
                *v = -*v;
            }
        }
        intensity_stereo(x, y, ctx.band_e[i], ctx.band_e[i + NB], n);
        if *b > 2 << BITRES && ctx.remaining_bits > 2 << BITRES {
            ctx.ec.bit_logp(inv as i32, 2);
        } else {
            inv = false;
        }
        let _ = inv;
        itheta = 0;
    } else {
        itheta = 0; // qn == 1 in a time/frequency split: nothing is coded
    }
    let qalloc = ctx.ec.tell_frac() as i32 - tell;
    *b -= qalloc;

    let delta = if itheta == 0 {
        -16384
    } else if itheta == 16384 {
        16384
    } else {
        let imid = bitexact_cos(itheta);
        let iside = bitexact_cos(16384 - itheta);
        frac_mul16((ni - 1) << 7, bitexact_log2tan(iside, imid))
    };
    SplitCtx { delta, itheta, qalloc }
}

fn quant_partition(ctx: &mut BandCtx, x: &mut [f32], mut n: usize, mut b: i32, mut bb: usize, mut lm: i32) {
    let m = ctx.m;
    let i = ctx.i;
    let b0 = bb;
    let cache = band_cache(m, i, lm);

    if lm != -1 && b > cache[cache[0] as usize] as i32 + 12 && n > 2 {
        n >>= 1;
        let (xh, yh) = x.split_at_mut(n);
        lm -= 1;
        bb = (bb + 1) >> 1;
        let sctx = compute_theta(ctx, xh, yh, n, &mut b, b0, lm, false);
        let mut delta = sctx.delta;
        let itheta = sctx.itheta;
        let qalloc = sctx.qalloc;
        if b0 > 1 && (itheta & 0x3fff) != 0 {
            if itheta > 8192 {
                delta -= delta >> (4 - lm);
            } else {
                delta = 0.min(delta + ((n as i32) << BITRES >> (5 - lm)));
            }
        }
        let mut mbits = 0.max(b.min((b - delta) / 2));
        let mut sbits = b - mbits;
        ctx.remaining_bits -= qalloc;
        let mut rebalance = ctx.remaining_bits;
        if mbits >= sbits {
            quant_partition(ctx, xh, n, mbits, bb, lm);
            rebalance = mbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 0 {
                sbits += rebalance - (3 << BITRES);
            }
            quant_partition(ctx, yh, n, sbits, bb, lm);
        } else {
            quant_partition(ctx, yh, n, sbits, bb, lm);
            rebalance = sbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 16384 {
                mbits += rebalance - (3 << BITRES);
            }
            quant_partition(ctx, xh, n, mbits, bb, lm);
        }
    } else {
        let mut q = bits2pulses(m, i, lm, b);
        let mut curr_bits = pulses2bits(m, i, lm, q);
        ctx.remaining_bits -= curr_bits;
        while ctx.remaining_bits < 0 && q > 0 {
            ctx.remaining_bits += curr_bits;
            q -= 1;
            curr_bits = pulses2bits(m, i, lm, q);
            ctx.remaining_bits -= curr_bits;
        }
        if q != 0 {
            alg_quant(x, n, get_pulses(q), ctx.spread, bb, ctx.ec);
        }
    }
}

fn quant_band(ctx: &mut BandCtx, x: &mut [f32], n: usize, b: i32, mut bb: usize, lm: i32) {
    let mut n_b = n / bb;
    let long_blocks = bb == 1;
    let mut tf_change = ctx.tf_change;
    let recombine = if tf_change > 0 { tf_change } else { 0 };
    for k in 0..recombine {
        haar1(x, n >> k, 1 << k);
    }
    bb >>= recombine;
    n_b <<= recombine;
    while (n_b & 1) == 0 && tf_change < 0 {
        haar1(x, n_b, bb);
        bb <<= 1;
        n_b >>= 1;
        tf_change += 1;
    }
    let b0 = bb;
    if b0 > 1 {
        deinterleave_hadamard(x, n_b >> recombine, b0 << recombine, long_blocks);
    }
    quant_partition(ctx, x, n, b, bb, lm);
}

fn quant_band_stereo(ctx: &mut BandCtx, x: &mut [f32], y: &mut [f32], n: usize, mut b: i32, bb: usize, lm: i32) {
    let sctx = compute_theta(ctx, x, y, n, &mut b, bb, lm, true);
    let delta = sctx.delta;
    let itheta = sctx.itheta;
    let qalloc = sctx.qalloc;
    let mut mbits = 0.max(b.min((b - delta) / 2));
    let mut sbits = b - mbits;
    ctx.remaining_bits -= qalloc;
    let mut rebalance = ctx.remaining_bits;
    if mbits >= sbits {
        quant_band(ctx, x, n, mbits, bb, lm);
        rebalance = mbits - (rebalance - ctx.remaining_bits);
        if rebalance > 3 << BITRES && itheta != 0 {
            sbits += rebalance - (3 << BITRES);
        }
        quant_band(ctx, y, n, sbits, bb, lm);
    } else {
        quant_band(ctx, y, n, sbits, bb, lm);
        rebalance = sbits - (rebalance - ctx.remaining_bits);
        if rebalance > 3 << BITRES && itheta != 16384 {
            mbits += rebalance - (3 << BITRES);
        }
        quant_band(ctx, x, n, mbits, bb, lm);
    }
}

/// xs, ys: normalised spectra of the two channels (modified in place). band_e: linear band amplitudes [c*22+i].
pub(crate) fn quant_all_bands(
    m: &ModeTables,
    end: usize,
    xs: &mut [f32],
    ys: &mut [f32],
    band_e: &[f32; C * NB],
    pulses: &[i32; NB],
    short_blocks: bool,
    spread: i32,
    mut dual_stereo: bool,
    intensity: i32,
    tf_res: &[i32; NB],
    total_bits: i32,
    mut balance: i32,
    ec: &mut Ec,
    coded_bands: i32,
) {
    let bb = if short_blocks { M as usize } else { 1 };
    let mut ctx = BandCtx { m, i: 0, intensity, spread, tf_change: 0, ec, remaining_bits: 0, band_e };
    for i in 0..end {
        let lo = (M * m.e_bands[i] as i32) as usize;
        let n = (M * m.e_bands[i + 1] as i32 - M * m.e_bands[i] as i32) as usize;
        ctx.i = i;
        let tell = ctx.ec.tell_frac() as i32;
        if i != 0 {
            balance -= tell;
        }
        let remaining_bits = total_bits - tell - 1;
        ctx.remaining_bits = remaining_bits;
        let b = if (i as i32) < coded_bands {
            let curr_balance = balance / 3.min(coded_bands - i as i32);
            0.max(16383.min((remaining_bits + 1).min(pulses[i] + curr_balance)))
        } else {
            0
        };
        ctx.tf_change = tf_res[i];
        if dual_stereo && i as i32 == intensity {
            dual_stereo = false;
        }
        let x = &mut xs[lo..lo + n];
        let y = &mut ys[lo..lo + n];
        if dual_stereo {
            quant_band(&mut ctx, x, n, b / 2, bb, LM);
            quant_band(&mut ctx, y, n, b / 2, bb, LM);
        } else {
            quant_band_stereo(&mut ctx, x, y, n, b, bb, LM);
        }
        balance += pulses[i] + tell;
    }
}
