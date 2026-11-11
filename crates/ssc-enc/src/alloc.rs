// SSC encoder: bit allocation (encoder side of compute_allocation / interp_bits2pulses).
//
// Ported from samsung-ssc-encoder src/ssc_enc_alloc.c, Copyright (c) 2026 William Ho (WillyBilly06),
// BSD-3-Clause, itself adapted from libopus 1.1 celt/rate.c (Copyright 2007-2009 Xiph.Org Foundation,
// BSD-3-Clause). See NOTICE.
//
// SSC specifics: C = 2 and LM = 3 always, start = 0, per-band caps from the mode table, and the allocation vector
// table is chosen by bitrate (> 192 kb/s uses the second table). Everything here is integer and must follow the
// decoder exactly; the only encoder choices are the band-skip flags, the intensity band and the dual-stereo flag.

use crate::consts::{BITRES, LM, MAX_FINE, NB};
use crate::ec::Ec;
use crate::tables::{LOG2_FRAC, ModeTables};

const ALLOC_STEPS: i32 = 6;
const FINE_OFFSET: i32 = 21;
const NB_VECTORS: i32 = 11;
const CC: i32 = 2;

pub(crate) struct AllocOut {
    pub coded_bands: usize,
    pub balance: i32,
}

fn interp_bits2pulses(
    m: &ModeTables,
    end: usize,
    skip_start: usize,
    bits1: &[i32; NB],
    bits2: &[i32; NB],
    thresh: &[i32; NB],
    cap: &[i32; 22],
    mut total: i32,
    balance_out: &mut i32,
    skip_rsv: i32,
    intensity: &mut i32,
    mut intensity_rsv: i32,
    dual_stereo: &mut i32,
    mut dual_stereo_rsv: i32,
    bits: &mut [i32; NB],
    ebits: &mut [i32; NB],
    fine_priority: &mut [i32; NB],
    prev: i32,
    signal_bandwidth: i32,
    ec: &mut Ec,
) -> usize {
    let alloc_floor = CC << BITRES;
    let log_m = LM << BITRES;
    let e_bands = m.e_bands;
    let eb = |i: usize| e_bands[i] as i32;
    let mut lo = 0i32;
    let mut hi = 1i32 << ALLOC_STEPS;

    for _ in 0..ALLOC_STEPS {
        let mid = (lo + hi) >> 1;
        let mut psum = 0i32;
        let mut done = false;
        for j in (0..end).rev() {
            let tmp = bits1[j] + ((mid * bits2[j]) >> ALLOC_STEPS);
            if tmp >= thresh[j] || done {
                done = true;
                psum += tmp.min(cap[j]);
            } else if tmp >= alloc_floor {
                psum += alloc_floor;
            }
        }
        if psum > total {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let mut psum = 0i32;
    let mut done = false;
    for j in (0..end).rev() {
        let mut tmp = bits1[j] + ((lo * bits2[j]) >> ALLOC_STEPS);
        if tmp < thresh[j] && !done {
            tmp = if tmp >= alloc_floor { alloc_floor } else { 0 };
        } else {
            done = true;
        }
        tmp = tmp.min(cap[j]);
        bits[j] = tmp;
        psum += tmp;
    }

    // Decide which bands to skip, working backwards from the end.
    let mut coded_bands = end;
    loop {
        let j = coded_bands - 1;
        if j <= skip_start {
            total += skip_rsv; // give the reserved skip bit back
            break;
        }
        let mut left = total - psum;
        let percoeff = left / (eb(coded_bands) - eb(0));
        left -= (eb(coded_bands) - eb(0)) * percoeff;
        let rem = (left - (eb(j) - eb(0))).max(0);
        let band_width = eb(coded_bands) - eb(j);
        let mut band_bits = bits[j] + percoeff * band_width + rem;
        if band_bits >= thresh[j].max(alloc_floor + (1 << BITRES)) {
            // The only non-normative part of the allocation: keep the band (hysteresis against the previous
            // frame's coded band count) or signal a skip.
            if coded_bands <= 2
                || (band_bits > ((((if (j as i32) < prev { 7 } else { 9 }) * band_width) << LM << BITRES) >> 4)
                    && j as i32 <= signal_bandwidth)
            {
                ec.bit_logp(1, 1);
                break;
            }
            ec.bit_logp(0, 1);
            psum += 1 << BITRES;
            band_bits -= 1 << BITRES;
        }
        // Reclaim the bits originally allocated to this band.
        psum -= bits[j] + intensity_rsv;
        if intensity_rsv > 0 {
            intensity_rsv = LOG2_FRAC[j] as i32;
        }
        psum += intensity_rsv;
        if band_bits >= alloc_floor {
            psum += alloc_floor;
            bits[j] = alloc_floor;
        } else {
            bits[j] = 0;
        }
        coded_bands -= 1;
    }

    // Intensity and dual stereo parameters.
    if intensity_rsv > 0 {
        *intensity = (*intensity).min(coded_bands as i32);
        ec.uint(*intensity as u32, coded_bands as u32 + 1);
    } else {
        *intensity = 0;
    }
    if *intensity <= 0 {
        total += dual_stereo_rsv;
        dual_stereo_rsv = 0;
    }
    if dual_stereo_rsv > 0 {
        ec.bit_logp(*dual_stereo, 1);
    } else {
        *dual_stereo = 0;
    }

    // Allocate the remaining bits
    let mut left = total - psum;
    let percoeff = left / (eb(coded_bands) - eb(0));
    left -= (eb(coded_bands) - eb(0)) * percoeff;
    for j in 0..coded_bands {
        bits[j] += percoeff * (eb(j + 1) - eb(j));
    }
    for j in 0..coded_bands {
        let tmp = left.min(eb(j + 1) - eb(j));
        bits[j] += tmp;
        left -= tmp;
    }

    let mut balance = 0i32;
    let mut j = 0;
    while j < coded_bands {
        let n0 = eb(j + 1) - eb(j);
        let bit = bits[j] + balance;
        let mut excess = bit - cap[j];
        if excess < 0 {
            excess = 0;
        }
        bits[j] = bit - excess;
        let den = CC * (n0 << LM) + if *dual_stereo == 0 && (j as i32) < *intensity { 1 } else { 0 };
        let nclogn = den * (m.log_n[j] as i32 + log_m);
        let mut offset = (nclogn >> 1) - den * FINE_OFFSET;
        if bits[j] + offset < (den * 2) << BITRES {
            offset += nclogn >> 2;
        } else if bits[j] + offset < (den * 3) << BITRES {
            offset += nclogn >> 3;
        }
        ebits[j] = 0.max((bits[j] + offset + (den << (BITRES - 1))) / (den << BITRES));
        if CC * ebits[j] > (bits[j] >> BITRES) {
            ebits[j] = bits[j] >> 1 >> BITRES;
        }
        ebits[j] = ebits[j].min(MAX_FINE);
        fine_priority[j] = (ebits[j] * (den << BITRES) >= bits[j] + offset) as i32;
        bits[j] -= (CC * ebits[j]) << BITRES;
        if excess > 0 {
            let extra_fine = (excess >> (1 + BITRES)).min(MAX_FINE - ebits[j]);
            ebits[j] += extra_fine;
            let extra_bits = (extra_fine * CC) << BITRES;
            fine_priority[j] = (extra_bits >= excess - balance) as i32;
            excess -= extra_bits;
        }
        balance = excess;
        j += 1;
    }
    *balance_out = balance;
    while j < end {
        ebits[j] = bits[j] >> 1 >> BITRES;
        bits[j] = 0;
        fine_priority[j] = (ebits[j] < 1) as i32;
        j += 1;
    }
    coded_bands
}

pub(crate) fn compute_allocation(
    m: &ModeTables,
    end: usize,
    offsets: &[i32; NB],
    alloc_trim: i32,
    hi_rate: bool,
    mut total: i32,
    pulses: &mut [i32; NB],
    ebits: &mut [i32; NB],
    fine_priority: &mut [i32; NB],
    intensity: &mut i32,
    dual_stereo: &mut i32,
    prev_coded: i32,
    signal_bw: i32,
    ec: &mut Ec,
) -> AllocOut {
    let len = NB;
    let av = m.alloc_vectors[hi_rate as usize];
    let cap = m.cap;
    let mut skip_start = 0usize;
    let mut bits1 = [0i32; NB];
    let mut bits2 = [0i32; NB];
    let mut thresh = [0i32; NB];
    let mut trim_offset = [0i32; NB];
    let n0 = |j: usize| (m.e_bands[j + 1] - m.e_bands[j]) as i32;

    if total < 0 {
        total = 0;
    }
    let skip_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
    total -= skip_rsv;
    let mut intensity_rsv = LOG2_FRAC[end] as i32;
    let mut dual_stereo_rsv = 0;
    if intensity_rsv > total {
        intensity_rsv = 0;
    } else {
        total -= intensity_rsv;
        dual_stereo_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
        total -= dual_stereo_rsv;
    }
    for j in 0..end {
        thresh[j] = (CC << BITRES).max(((3 * n0(j)) << LM << BITRES) >> 4);
        trim_offset[j] = CC * n0(j) * (alloc_trim - 5 - LM) * (end as i32 - j as i32 - 1);
    }
    let mut lo = 1i32;
    let mut hi = NB_VECTORS - 1;
    loop {
        let mut done = false;
        let mid = (lo + hi) >> 1;
        let mut psum = 0i32;
        for j in (0..end).rev() {
            let mut bitsj = (CC * n0(j) * av[mid as usize * len + j] as i32) << LM >> 2;
            if bitsj > 0 {
                bitsj = 0.max(bitsj + trim_offset[j]);
            }
            bitsj += offsets[j];
            if bitsj >= thresh[j] || done {
                done = true;
                psum += bitsj.min(cap[j]);
            } else if bitsj >= CC << BITRES {
                psum += CC << BITRES;
            }
        }
        if psum > total {
            hi = mid - 1;
        } else {
            lo = mid + 1;
        }
        if lo > hi {
            break;
        }
    }
    hi = lo;
    lo -= 1;
    for j in 0..end {
        let mut bits1j = (CC * n0(j) * av[lo as usize * len + j] as i32) << LM >> 2;
        let mut bits2j =
            if hi >= NB_VECTORS { cap[j] } else { (CC * n0(j) * av[hi as usize * len + j] as i32) << LM >> 2 };
        if bits1j > 0 {
            bits1j = 0.max(bits1j + trim_offset[j]);
        }
        if bits2j > 0 {
            bits2j = 0.max(bits2j + trim_offset[j]);
        }
        if lo > 0 {
            bits1j += offsets[j];
        }
        bits2j += offsets[j];
        if offsets[j] > 0 {
            skip_start = j;
        }
        bits2j = 0.max(bits2j - bits1j);
        bits1[j] = bits1j;
        bits2[j] = bits2j;
    }
    let mut balance = 0;
    let coded_bands = interp_bits2pulses(
        m,
        end,
        skip_start,
        &bits1,
        &bits2,
        &thresh,
        cap,
        total,
        &mut balance,
        skip_rsv,
        intensity,
        intensity_rsv,
        dual_stereo,
        dual_stereo_rsv,
        pulses,
        ebits,
        fine_priority,
        prev_coded,
        signal_bw,
        ec,
    );
    AllocOut { coded_bands, balance }
}
