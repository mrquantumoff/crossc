// SSC encoder: band energy quantisation (coarse, fine, finalise).
//
// Ported from samsung-ssc-encoder src/ssc_enc_energy.c, Copyright (c) 2026 William Ho (WillyBilly06),
// BSD-3-Clause, itself adapted from libopus 1.1 celt/quant_bands.c, fixed-point arithmetic (Copyright 2007-2009
// Xiph.Org Foundation, 2008-2009 Gregory Maxwell, BSD-3-Clause). See NOTICE.
//
// All values are i16 Q10 log2 amplitudes, layout [c*22 + band]. The prediction state old_e is updated with the
// same integer arithmetic as the decoder, so encoder and decoder stay in lock-step. SSC always codes two
// channels, LM = 3 (prediction 0.5, beta 0.2; intra beta 0.15) and start band 0.

use crate::consts::{C, DB_SHIFT, MAX_FINE, NB};
use crate::ec::Ec;
use crate::tables::E_PROB_MODEL;

const PRED_COEF: i32 = 16384; // 0.5 Q15
const BETA_COEF: i32 = 6554; // 0.2 Q15
const BETA_INTRA: i32 = 4915; // 0.15 Q15
const fn q10(x: i32) -> i32 {
    x * (1 << DB_SHIFT)
}

const SMALL_ENERGY_ICDF: [u8; 3] = [2, 1, 0];

pub(crate) type BandI16 = [i16; C * NB];

fn coarse_pass(
    end: usize,
    x: &BandI16,
    old_e: &mut BandI16,
    budget: i32,
    intra: bool,
    max_decay: i32,
    error: &mut BandI16,
    enc: &mut Ec,
) -> i32 {
    let prob = &E_PROB_MODEL[intra as usize];
    let mut prev = [0i32; 2];
    let coef = if intra { 0 } else { PRED_COEF };
    let beta = if intra { BETA_INTRA } else { BETA_COEF };
    let mut badness = 0;

    if enc.tell() + 3 <= budget {
        enc.bit_logp(intra as i32, 3);
    }
    for i in 0..end {
        for c in 0..C {
            let k = i + c * NB;
            let old = old_e[k] as i32;
            let oldc = if old < -q10(9) { -q10(9) } else { old };
            let f = ((x[k] as i32) << 7) - ((coef * oldc + 128) >> 8) - prev[c];
            let mut qi = (f + (1 << (DB_SHIFT + 6))) >> (DB_SHIFT + 7); // round to nearest
            let mut decay_bound = old - max_decay;
            if decay_bound < -q10(28) {
                decay_bound = -q10(28);
            }
            // do not let the energy drop faster than max_decay per frame
            if qi < 0 && (x[k] as i32) < decay_bound {
                qi += (decay_bound - x[k] as i32) >> DB_SHIFT;
                if qi > 0 {
                    qi = 0;
                }
            }
            let qi0 = qi;
            let tell = enc.tell();
            let bits_left = budget - tell - 3 * C as i32 * (end - i) as i32;
            if i != 0 && bits_left < 30 {
                if bits_left < 24 {
                    qi = qi.min(1);
                }
                if bits_left < 16 {
                    qi = qi.max(-1);
                }
            }
            if budget - tell >= 15 {
                let pi = 2 * i.min(20);
                enc.laplace_encode(&mut qi, (prob[pi] as u32) << 7, (prob[pi + 1] as i32) << 6);
            } else if budget - tell >= 2 {
                qi = (-1).max(qi.min(1));
                enc.icdf((2 * qi) ^ -((qi < 0) as i32), &SMALL_ENERGY_ICDF, 2);
            } else if budget - tell >= 1 {
                qi = qi.min(0);
                enc.bit_logp(-qi, 1);
            } else {
                qi = -1;
            }
            error[k] = (((f + 64) >> 7) - (qi << DB_SHIFT)) as i16;
            badness += (qi0 - qi).abs();
            let q = qi.wrapping_shl(DB_SHIFT as u32);
            let mut tmp = ((coef * oldc + 128) >> 8) + prev[c] + q.wrapping_shl(7);
            if tmp < -(28 << (DB_SHIFT + 7)) {
                tmp = -(28 << (DB_SHIFT + 7));
            }
            old_e[k] = ((tmp + 64) >> 7) as i16;
            prev[c] = prev[c] + q.wrapping_shl(7) - beta * ((q + 128) >> 8);
        }
    }
    badness
}

/// Two-pass coarse quantisation: tries intra and inter prediction and keeps the cheaper one (fewer clamped
/// deltas first, then fewer bits). Returns the intra flag that was coded.
pub(crate) fn quant_coarse_energy(
    end: usize,
    x: &BandI16,
    old_e: &mut BandI16,
    budget: i32,
    nb_available_bytes: i32,
    error: &mut BandI16,
    enc: &mut Ec,
) -> bool {
    let mut max_decay = q10(16);
    let mut two_pass = true;
    if enc.tell() + 3 > budget {
        two_pass = false;
    }
    if end > 10 && (nb_available_bytes << (DB_SHIFT - 3)) < max_decay {
        max_decay = nb_available_bytes << (DB_SHIFT - 3);
    }
    if !two_pass {
        coarse_pass(end, x, old_e, budget, false, max_decay, error, enc);
        return false;
    }
    let start = enc.s;
    let mut old_intra = *old_e;
    let mut err_intra = [0i16; C * NB];
    let badness1 = coarse_pass(end, x, &mut old_intra, budget, true, max_decay, &mut err_intra, enc);
    let tell_intra = enc.tell_frac();
    let after_intra = enc.s;
    // The range coder only writes bytes forward from offs; keep the ones the intra pass produced.
    let mut intra_bytes = [0u8; 1300];
    let nstart = start.offs as usize;
    let mut nintra = after_intra.offs as usize;
    if nintra - nstart > intra_bytes.len() {
        nintra = nstart + intra_bytes.len(); // not reachable
    }
    intra_bytes[..nintra - nstart].copy_from_slice(&enc.buf[nstart..nintra]);
    enc.s = start;
    let badness2 = coarse_pass(end, x, old_e, budget, false, max_decay, error, enc);
    if badness1 < badness2 || (badness1 == badness2 && enc.tell_frac() > tell_intra) {
        enc.s = after_intra;
        enc.buf[nstart..nintra].copy_from_slice(&intra_bytes[..nintra - nstart]);
        *old_e = old_intra;
        *error = err_intra;
        return true;
    }
    false
}

pub(crate) fn quant_fine_energy(
    end: usize,
    old_e: &mut BandI16,
    error: &mut BandI16,
    fine_quant: &[i32; NB],
    enc: &mut Ec,
) {
    for i in 0..end {
        let fq = fine_quant[i];
        if fq <= 0 {
            continue;
        }
        let frac = 1 << fq;
        for c in 0..C {
            let k = i + c * NB;
            let mut q2 = (error[k] as i32 + (1 << (DB_SHIFT - 1))) >> (DB_SHIFT - fq);
            if q2 > frac - 1 {
                q2 = frac - 1;
            }
            if q2 < 0 {
                q2 = 0;
            }
            enc.bits(q2 as u32, fq as u32);
            let offset = ((((q2 << DB_SHIFT) + (1 << (DB_SHIFT - 1))) >> fq) - (1 << (DB_SHIFT - 1))) as i16;
            old_e[k] = (old_e[k] as i32 + offset as i32) as i16;
            error[k] = (error[k] as i32 - offset as i32) as i16;
        }
    }
}

pub(crate) fn quant_energy_finalise(
    end: usize,
    old_e: &mut BandI16,
    error: &mut BandI16,
    fine_quant: &[i32; NB],
    fine_priority: &[i32; NB],
    mut bits_left: i32,
    enc: &mut Ec,
) {
    for prio in 0..2 {
        let mut i = 0;
        while i < end && bits_left >= C as i32 {
            if fine_quant[i] >= MAX_FINE || fine_priority[i] != prio {
                i += 1;
                continue;
            }
            for c in 0..C {
                let k = i + c * NB;
                let q2: i32 = if error[k] < 0 { 0 } else { 1 };
                enc.bits(q2 as u32, 1);
                let offset = (((q2 << DB_SHIFT) - (1 << (DB_SHIFT - 1))) >> (fine_quant[i] + 1)) as i16;
                old_e[k] = (old_e[k] as i32 + offset as i32) as i16;
                error[k] = (error[k] as i32 - offset as i32) as i16;
                bits_left -= 1;
            }
            i += 1;
        }
    }
}
