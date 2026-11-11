// SSC encoder: signal analysis (transients, TF resolution, spreading, dynamic allocation, trim, stereo).
//
// Ported from samsung-ssc-encoder src/ssc_enc_analysis.c, Copyright (c) 2026 William Ho (WillyBilly06),
// BSD-3-Clause, itself adapted from libopus 1.1 celt/celt_encoder.c and celt/bands.c, float build (Copyright
// 2007-2010 Xiph.Org Foundation, 2008 Gregory Maxwell, 2007-2008 CSIRO, BSD-3-Clause). See NOTICE.
//
// These are encoder decisions only: none of them has to match anything, they just pick legal values for the
// symbols the bitstream carries. Energies here are float log2 amplitudes relative to eMeans.

use crate::bands::haar1;
use crate::consts::*;
use crate::math::{f2i, fabsf, floorf, log2f, sqrtf};
use crate::tables::{E_MEANS, ModeTables};

/// CELT LM = 3 row.
pub(crate) const TF_SELECT_TABLE: [i8; 8] = [0, -2, 0, -3, 3, 0, 1, -1];

#[inline]
pub(crate) fn tf_select_table(i: i32) -> i32 {
    TF_SELECT_TABLE[i as usize] as i32
}

pub(crate) struct Transient {
    pub is_transient: bool,
    pub tf_estimate: f32,
    pub tf_chan: usize,
}

/// Time-domain transient detector over both channels' `len` input samples.
pub(crate) fn transient_analysis(input: &[[f32; N + OVERLAP]; C], len: usize) -> Transient {
    const INV_TABLE: [u8; 128] = [
        255, 255, 156, 110, 86, 70, 59, 51, 45, 40, 37, 33, 31, 28, 26, 25, 23, 22, 21, 20, 19, 18, 17, 16, 16, 15, 15,
        14, 13, 13, 12, 12, 12, 12, 11, 11, 11, 10, 10, 10, 9, 9, 9, 9, 9, 9, 8, 8, 8, 8, 8, 7, 7, 7, 7, 7, 7, 6, 6, 6,
        6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
        4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 2,
    ];
    let mut tmp = [0f32; N + OVERLAP];
    let len2 = len / 2;
    let mut mask_metric = 0i32;
    let mut tf_chan = 0usize;
    for c in 0..C {
        let mut mean = 0f32;
        let mut mem0 = 0f32;
        let mut mem1 = 0f32;
        let mut max_e = 0f32;
        let mut unmask = 0i32;
        let x = &input[c];
        // high-pass filter (1 - 2z^-1 + z^-2) / (1 - z^-1 + .5 z^-2)
        for i in 0..len {
            let y = mem0 + x[i];
            mem0 = mem1 + y - 2.0 * x[i];
            mem1 = x[i] - 0.5 * y;
            tmp[i] = y;
        }
        for v in tmp.iter_mut().take(12) {
            *v = 0.0;
        }
        mem0 = 0.0;
        // forward pass: post-echo threshold
        for i in 0..len2 {
            let x2 = tmp[2 * i] * tmp[2 * i] + tmp[2 * i + 1] * tmp[2 * i + 1];
            mean += x2;
            tmp[i] = mem0 + 0.0625 * (x2 - mem0);
            mem0 = tmp[i];
        }
        mem0 = 0.0;
        // backward pass: pre-echo threshold
        for i in (0..len2).rev() {
            tmp[i] = mem0 + 0.125 * (tmp[i] - mem0);
            mem0 = tmp[i];
            if mem0 > max_e {
                max_e = mem0;
            }
        }
        mean = sqrtf(mean * max_e * 0.5 * len2 as f32);
        let norm = len2 as f32 / (1e-15 + mean);
        let mut i = 12;
        while i < len2 - 5 {
            let mut id = f2i(floorf(64.0 * norm * tmp[i]));
            if id < 0 {
                id = 0;
            }
            if id > 127 {
                id = 127;
            }
            unmask += INV_TABLE[id as usize] as i32;
            i += 4;
        }
        unmask = 64 * unmask * 4 / (6 * (len2 as i32 - 17));
        if unmask > mask_metric {
            tf_chan = c;
            mask_metric = unmask;
        }
    }
    let mut tf_max = sqrtf(27.0 * mask_metric as f32) - 42.0;
    if tf_max < 0.0 {
        tf_max = 0.0;
    }
    let t = 0.0069 * (if tf_max < 163.0 { tf_max } else { 163.0 }) - 0.139;
    let tf_estimate = sqrtf(if t > 0.0 { t } else { 0.0 });
    Transient { is_transient: mask_metric > 200, tf_estimate, tf_chan }
}

/// Sudden energy increase that the time-domain detector missed. new_e/old_e: [c*22+i] float log2.
pub(crate) fn patch_transient_decision(new_e: &[f32; C * NB], old_e: &[f32; C * NB], end: usize) -> bool {
    let mut mean_diff = 0f32;
    let mut spread_old = [0f32; NB];
    spread_old[0] = if old_e[0] > old_e[NB] { old_e[0] } else { old_e[NB] };
    for i in 1..end {
        let a = spread_old[i - 1] - 1.0;
        let b = if old_e[i] > old_e[i + NB] { old_e[i] } else { old_e[i + NB] };
        spread_old[i] = if a > b { a } else { b };
    }
    for i in (0..end - 1).rev() {
        if spread_old[i + 1] - 1.0 > spread_old[i] {
            spread_old[i] = spread_old[i + 1] - 1.0;
        }
    }
    for c in 0..C {
        for i in 2..end - 1 {
            let x1 = if new_e[i + c * NB] > 0.0 { new_e[i + c * NB] } else { 0.0 };
            let x2 = if spread_old[i] > 0.0 { spread_old[i] } else { 0.0 };
            if x1 > x2 {
                mean_diff += x1 - x2;
            }
        }
    }
    mean_diff /= (C * (end - 3)) as f32;
    mean_diff > 1.0
}

fn l1_metric(tmp: &[f32], n: usize, lm: i32, bias: f32) -> f32 {
    let mut l1 = 0f32;
    for &v in &tmp[..n] {
        l1 += fabsf(v);
    }
    l1 + lm as f32 * bias * l1
}

/// Chooses the per-band time-frequency resolution (Viterbi over a L1-norm sparsity metric).
pub(crate) fn tf_analysis(
    m: &ModeTables,
    len: usize,
    is_transient: bool,
    tf_res: &mut [i32; NB],
    lambda: i32,
    x: &[f32],
    tf_estimate: f32,
    tf_chan: usize,
) -> i32 {
    let mut metric = [0i32; NB];
    let mut path0 = [0i32; NB];
    let mut path1 = [0i32; NB];
    let mut selcost = [0i32; 2];
    let mut tmp = [0f32; MAX_BAND_N];
    let mut tmp_1 = [0f32; MAX_BAND_N];
    let mut tf_select = 0;
    let it = is_transient as i32;
    let bias = 0.04 * (if 0.5 - tf_estimate > -0.25 { 0.5 - tf_estimate } else { -0.25 });

    for i in 0..len {
        let width = (m.e_bands[i + 1] - m.e_bands[i]) as usize;
        let n = width << LM;
        let mut best_level = 0;
        let narrow = width == 1;
        let start = tf_chan * N + ((m.e_bands[i] as usize) << LM);
        tmp[..n].copy_from_slice(&x[start..start + n]);
        let mut l1 = l1_metric(&tmp, n, if is_transient { LM } else { 0 }, bias);
        let mut best_l1 = l1;
        if is_transient && !narrow {
            tmp_1[..n].copy_from_slice(&tmp[..n]);
            haar1(&mut tmp_1, n >> LM, 1 << LM);
            l1 = l1_metric(&tmp_1, n, LM + 1, bias);
            if l1 < best_l1 {
                best_l1 = l1;
                best_level = -1;
            }
        }
        let kmax = LM + !(is_transient || narrow) as i32;
        for k in 0..kmax {
            let b = if is_transient { LM - k - 1 } else { k + 1 };
            haar1(&mut tmp, n >> k, 1 << k);
            l1 = l1_metric(&tmp, n, b, bias);
            if l1 < best_l1 {
                best_l1 = l1;
                best_level = k + 1;
            }
        }
        metric[i] = if is_transient { 2 * best_level } else { -2 * best_level };
        if narrow && (metric[i] == 0 || metric[i] == -2 * LM) {
            metric[i] -= 1;
        }
    }
    for sel in 0..2 {
        let mut cost0 = 0;
        let mut cost1 = if is_transient { 0 } else { lambda };
        for i in 1..len {
            let curr0 = cost0.min(cost1 + lambda);
            let curr1 = (cost0 + lambda).min(cost1);
            let d0 = metric[i] - 2 * tf_select_table(4 * it + 2 * sel);
            let d1 = metric[i] - 2 * tf_select_table(4 * it + 2 * sel + 1);
            cost0 = curr0 + if d0 < 0 { -d0 } else { d0 };
            cost1 = curr1 + if d1 < 0 { -d1 } else { d1 };
        }
        selcost[sel as usize] = cost0.min(cost1);
    }
    if selcost[1] < selcost[0] && is_transient {
        tf_select = 1;
    }
    let mut cost0 = 0;
    let mut cost1 = if is_transient { 0 } else { lambda };
    for i in 1..len {
        let curr0;
        let curr1;
        let from0 = cost0;
        let from1 = cost1 + lambda;
        if from0 < from1 {
            curr0 = from0;
            path0[i] = 0;
        } else {
            curr0 = from1;
            path0[i] = 1;
        }
        let from0 = cost0 + lambda;
        let from1 = cost1;
        if from0 < from1 {
            curr1 = from0;
            path1[i] = 0;
        } else {
            curr1 = from1;
            path1[i] = 1;
        }
        let d0 = metric[i] - 2 * tf_select_table(4 * it + 2 * tf_select);
        let d1 = metric[i] - 2 * tf_select_table(4 * it + 2 * tf_select + 1);
        cost0 = curr0 + if d0 < 0 { -d0 } else { d0 };
        cost1 = curr1 + if d1 < 0 { -d1 } else { d1 };
    }
    tf_res[len - 1] = if cost0 < cost1 { 0 } else { 1 };
    for i in (0..len - 1).rev() {
        tf_res[i] = if tf_res[i + 1] == 1 { path1[i + 1] } else { path0[i + 1] };
    }
    tf_select
}

/// Spreading (rotation) decision from the peakiness of the normalised spectrum.
pub(crate) fn spreading_decision(m: &ModeTables, x: &[f32], average: &mut i32, last_decision: i32, end: usize) -> i32 {
    let eb = |i: usize| m.e_bands[i] as i32;
    let mut sum = 0i32;
    let mut nb_bands = 0i32;
    if M * (eb(end) - eb(end - 1)) <= 8 {
        return SPREAD_NONE;
    }
    for c in 0..C {
        for i in 0..end {
            let n = M * (eb(i + 1) - eb(i));
            let mut tcount = [0i32; 3];
            if n <= 8 {
                continue;
            }
            let base = (M * eb(i)) as usize + c * N;
            for j in 0..n as usize {
                let v = x[base + j];
                let x2n = v * v * n as f32;
                if x2n < 0.25 {
                    tcount[0] += 1;
                }
                if x2n < 0.0625 {
                    tcount[1] += 1;
                }
                if x2n < 0.015625 {
                    tcount[2] += 1;
                }
            }
            let tmp = (2 * tcount[2] >= n) as i32 + (2 * tcount[1] >= n) as i32 + (2 * tcount[0] >= n) as i32;
            sum += tmp * 256;
            nb_bands += 1;
        }
    }
    sum /= nb_bands;
    sum = (sum + *average) >> 1;
    *average = sum;
    sum = (3 * sum + (((3 - last_decision) << 7) + 64) + 2) >> 2;
    if sum < 80 {
        SPREAD_AGGRESSIVE
    } else if sum < 256 {
        SPREAD_NORMAL
    } else if sum < 384 {
        SPREAD_LIGHT
    } else {
        SPREAD_NONE
    }
}

/// Dynamic allocation: boosts bands that stick out of a smoothed spectral envelope. offsets receives the number of
/// boost quanta wanted per band (the caller codes them). CBR rules: non-transient frames get half the boost, and
/// the total is limited to a quarter of the frame.
pub(crate) fn dynalloc_analysis(
    m: &ModeTables,
    band_log_e: &[f32; C * NB],
    band_log_e2: &[f32; C * NB],
    end: usize,
    offsets: &mut [i32; NB],
    lsb_depth: i32,
    is_transient: bool,
    effective_bytes: i32,
) {
    let eb = |i: usize| m.e_bands[i] as i32;
    let mut follower = [0f32; C * NB];
    let mut noise_floor = [0f32; NB];
    let mut tot_boost = 0i32;
    offsets.fill(0);
    if effective_bytes <= 50 {
        return;
    }
    for i in 0..end {
        noise_floor[i] = 0.0625 * m.log_n[i] as f32 + 0.5 + (9 - lsb_depth) as f32 - E_MEANS[i]
            + 0.0062 * (i + 5) as f32 * (i + 5) as f32;
    }
    for c in 0..C {
        let e2 = &band_log_e2[c * NB..c * NB + NB];
        let f = &mut follower[c * NB..c * NB + NB];
        let mut last = 0usize;
        f[0] = e2[0];
        for i in 1..end {
            if e2[i] > e2[i - 1] + 0.5 {
                last = i;
            }
            f[i] = if f[i - 1] + 1.5 < e2[i] { f[i - 1] + 1.5 } else { e2[i] };
        }
        for i in (0..last).rev() {
            let a = if f[i + 1] + 2.0 < e2[i] { f[i + 1] + 2.0 } else { e2[i] };
            if a < f[i] {
                f[i] = a;
            }
        }
        for i in 0..end {
            if f[i] < noise_floor[i] {
                f[i] = noise_floor[i];
            }
        }
    }
    for i in 0..end {
        let (i0, i1) = (i, NB + i);
        if follower[i0] - 4.0 > follower[i1] {
            follower[i1] = follower[i0] - 4.0;
        }
        if follower[i1] - 4.0 > follower[i0] {
            follower[i0] = follower[i1] - 4.0;
        }
        let d0 = band_log_e[i] - follower[i0];
        let d1 = band_log_e[NB + i] - follower[i1];
        follower[i] = 0.5 * ((if d0 > 0.0 { d0 } else { 0.0 }) + (if d1 > 0.0 { d1 } else { 0.0 }));
    }
    if !is_transient {
        for v in follower.iter_mut().take(end) {
            *v *= 0.5;
        }
    }
    for i in 0..end {
        let width = (C as i32 * (eb(i + 1) - eb(i))) << LM;
        let quanta = (width << BITRES).min((6 << BITRES).max(width));
        if i < 8 {
            follower[i] *= 2.0;
        }
        if i >= 12 {
            follower[i] *= 0.5;
        }
        if follower[i] > 4.0 {
            follower[i] = 4.0;
        }
        let mut boost;
        let mut boost_bits;
        if width > 48 {
            boost = f2i(follower[i] * 8.0);
            boost_bits = ((boost * width) << BITRES) / 8;
        } else {
            boost = f2i(follower[i] * width as f32 / 6.0);
            boost_bits = (boost * 6) << BITRES;
        }
        // boost is in units of the band's coding quantum
        boost = boost_bits / quanta;
        boost_bits = boost * quanta;
        if (tot_boost + boost_bits) >> BITRES >> 3 > effective_bytes / 4 {
            let cap = (effective_bytes / 4) << BITRES << 3;
            offsets[i] = (cap - tot_boost) / quanta;
            break;
        }
        offsets[i] = boost;
        tot_boost += boost_bits;
    }
}

/// Allocation trim: spectral tilt, inter-channel correlation and transient-ness.
pub(crate) fn alloc_trim_analysis(
    m: &ModeTables,
    x: &[f32],
    band_log_e: &[f32; C * NB],
    end: usize,
    stereo_saving: &mut f32,
    tf_estimate: f32,
    intensity: usize,
) -> i32 {
    let mut diff = 0f32;
    let mut trim = 5f32;
    let mut sum = 0f32;
    let lo = |i: usize| (m.e_bands[i] as usize) << LM;
    for i in 0..8 {
        let mut partial = 0f32;
        for j in lo(i)..lo(i + 1) {
            partial += x[j] * x[N + j];
        }
        sum += partial;
    }
    sum *= 1.0 / 8.0;
    sum = if fabsf(sum) < 1.0 { fabsf(sum) } else { 1.0 };
    let mut min_xc = sum;
    for i in 8..intensity {
        let mut partial = 0f32;
        for j in lo(i)..lo(i + 1) {
            partial += x[j] * x[N + j];
        }
        if fabsf(partial) < min_xc {
            min_xc = fabsf(partial);
        }
    }
    if min_xc > 1.0 {
        min_xc = 1.0;
    }
    let log_xc = log2f(1.001 - sum * sum);
    let mut log_xc2 = log2f(1.001 - min_xc * min_xc);
    if 0.5 * log_xc > log_xc2 {
        log_xc2 = 0.5 * log_xc;
    }
    trim += if 0.75 * log_xc > -4.0 { 0.75 * log_xc } else { -4.0 };
    if *stereo_saving + 0.25 < -0.5 * log_xc2 {
        *stereo_saving += 0.25;
    } else {
        *stereo_saving = -0.5 * log_xc2;
    }

    for c in 0..C {
        for i in 0..end - 1 {
            diff += band_log_e[i + c * NB] * (2 + 2 * i as i32 - end as i32) as f32;
        }
    }
    diff /= (C * (end - 1)) as f32;
    let mut t = (diff + 1.0) / 6.0;
    if t > 2.0 {
        t = 2.0;
    }
    if t < -2.0 {
        t = -2.0;
    }
    trim -= t;
    trim -= 2.0 * tf_estimate;
    let mut trim_index = f2i(floorf(0.5 + trim));
    if trim_index < 0 {
        trim_index = 0;
    }
    if trim_index > 10 {
        trim_index = 10;
    }
    trim_index
}

/// Dual (L/R) stereo vs mid/side, from an L1 entropy estimate over the 13 lowest bands.
pub(crate) fn stereo_analysis(m: &ModeTables, x: &[f32]) -> bool {
    let mut sum_lr = 1e-15f32;
    let mut sum_ms = 1e-15f32;
    for i in 0..13 {
        for j in ((m.e_bands[i] as usize) << LM)..((m.e_bands[i + 1] as usize) << LM) {
            let l = x[j];
            let r = x[N + j];
            let mi = l + r;
            let s = l - r;
            sum_lr += fabsf(l) + fabsf(r);
            sum_ms += fabsf(mi) + fabsf(s);
        }
    }
    sum_ms *= 0.707107;
    let e13 = m.e_bands[13] as i32;
    (((e13 << (LM + 1)) + 13) as f32 * sum_ms) > ((e13 << (LM + 1)) as f32 * sum_lr)
}
