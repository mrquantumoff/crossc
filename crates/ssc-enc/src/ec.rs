// SSC encoder: range encoder.
//
// Ported from samsung-ssc-encoder src/ssc_enc_ec.c, Copyright (c) 2026 William Ho (WillyBilly06),
// BSD-3-Clause, itself adapted from libopus 1.1 celt/entenc.c, celt/entcode.c and celt/laplace.c
// (Copyright 2001-2011 Timothy B. Terriberry, Xiph.Org Foundation, BSD-3-Clause). See NOTICE.
//
// The only change from opus is the coder width: SSC uses EC_CODE_BITS = 24 instead of 32, so the range is kept
// in [2^15, 2^23]. Raw bits (`bits`) are packed from the end of the buffer as in opus; the buffer is the whole
// range-coded payload.

use crate::consts::BITRES;

const EC_SYM_BITS: u32 = 8;
const EC_CODE_BITS: i32 = 24;
const EC_SYM_MAX: u32 = (1 << EC_SYM_BITS) - 1;
const EC_CODE_SHIFT: u32 = EC_CODE_BITS as u32 - EC_SYM_BITS - 1;
const EC_CODE_TOP: u32 = 1 << (EC_CODE_BITS - 1);
const EC_CODE_BOT: u32 = EC_CODE_TOP >> EC_SYM_BITS;
const EC_WINDOW_SIZE: i32 = 32;
const EC_UINT_BITS: i32 = 8;

#[inline]
pub(crate) fn ilog(v: u32) -> i32 {
    32 - v.leading_zeros() as i32
}

/// The coder state without the buffer, so it can be saved and restored (two-pass coarse energy).
#[derive(Clone, Copy)]
pub(crate) struct EcState {
    pub storage: u32,
    pub end_offs: u32,
    pub end_window: u32,
    pub nend_bits: i32,
    pub nbits_total: i32,
    pub offs: u32,
    pub rng: u32,
    pub val: u32,
    pub ext: u32,
    pub rem: i32,
    pub error: i32,
}

pub(crate) struct Ec<'a> {
    pub buf: &'a mut [u8],
    pub s: EcState,
}

impl<'a> Ec<'a> {
    /// `buf` is the whole range-coded payload; its length is the storage size.
    pub fn new(buf: &'a mut [u8]) -> Self {
        let storage = buf.len() as u32;
        Ec {
            buf,
            s: EcState {
                storage,
                end_offs: 0,
                end_window: 0,
                nend_bits: 0,
                nbits_total: EC_CODE_BITS + 1,
                offs: 0,
                rng: EC_CODE_TOP,
                val: 0,
                ext: 0,
                rem: -1,
                error: 0,
            },
        }
    }

    #[inline]
    pub fn tell(&self) -> i32 {
        self.s.nbits_total - ilog(self.s.rng)
    }

    pub fn tell_frac(&self) -> u32 {
        let nbits = (self.s.nbits_total as u32) << BITRES;
        let mut l = ilog(self.s.rng);
        let mut r = self.s.rng >> (l - 16);
        for _ in 0..BITRES {
            r = r.wrapping_mul(r) >> 15;
            let b = (r >> 16) as i32;
            l = (l << 1) | b;
            r >>= b;
        }
        nbits.wrapping_sub(l as u32)
    }

    fn write_byte(&mut self, v: u32) -> i32 {
        if self.s.offs + self.s.end_offs >= self.s.storage {
            return -1;
        }
        self.buf[self.s.offs as usize] = v as u8;
        self.s.offs += 1;
        0
    }

    fn write_byte_at_end(&mut self, v: u32) -> i32 {
        if self.s.offs + self.s.end_offs >= self.s.storage {
            return -1;
        }
        self.s.end_offs += 1;
        self.buf[(self.s.storage - self.s.end_offs) as usize] = v as u8;
        0
    }

    /// Outputs a symbol, with a carry bit. Runs of 0xFF are buffered until the carry is known.
    fn carry_out(&mut self, c: i32) {
        if c != EC_SYM_MAX as i32 {
            let carry = c >> EC_SYM_BITS;
            if self.s.rem >= 0 {
                let r = self.write_byte((self.s.rem + carry) as u32);
                self.s.error |= r;
            }
            if self.s.ext > 0 {
                let sym = (EC_SYM_MAX.wrapping_add(carry as u32)) & EC_SYM_MAX;
                loop {
                    let r = self.write_byte(sym);
                    self.s.error |= r;
                    self.s.ext -= 1;
                    if self.s.ext == 0 {
                        break;
                    }
                }
            }
            self.s.rem = c & EC_SYM_MAX as i32;
        } else {
            self.s.ext += 1;
        }
    }

    fn normalize(&mut self) {
        while self.s.rng <= EC_CODE_BOT {
            self.carry_out((self.s.val >> EC_CODE_SHIFT) as i32);
            self.s.val = (self.s.val << EC_SYM_BITS) & (EC_CODE_TOP - 1);
            self.s.rng <<= EC_SYM_BITS;
            self.s.nbits_total += EC_SYM_BITS as i32;
        }
    }

    pub fn encode(&mut self, fl: u32, fh: u32, ft: u32) {
        let r = self.s.rng / ft;
        if fl > 0 {
            self.s.val = self.s.val.wrapping_add(self.s.rng.wrapping_sub(r.wrapping_mul(ft.wrapping_sub(fl))));
            self.s.rng = r.wrapping_mul(fh.wrapping_sub(fl));
        } else {
            self.s.rng = self.s.rng.wrapping_sub(r.wrapping_mul(ft.wrapping_sub(fh)));
        }
        self.normalize();
    }

    pub fn encode_bin(&mut self, fl: u32, fh: u32, bits: u32) {
        let r = self.s.rng >> bits;
        if fl > 0 {
            self.s.val =
                self.s.val.wrapping_add(self.s.rng.wrapping_sub(r.wrapping_mul((1u32 << bits).wrapping_sub(fl))));
            self.s.rng = r.wrapping_mul(fh.wrapping_sub(fl));
        } else {
            self.s.rng = self.s.rng.wrapping_sub(r.wrapping_mul((1u32 << bits).wrapping_sub(fh)));
        }
        self.normalize();
    }

    pub fn bit_logp(&mut self, val: i32, logp: u32) {
        let mut r = self.s.rng;
        let l = self.s.val;
        let s = r >> logp;
        r -= s;
        if val != 0 {
            self.s.val = l.wrapping_add(r);
        }
        self.s.rng = if val != 0 { s } else { r };
        self.normalize();
    }

    pub fn icdf(&mut self, s: i32, icdf: &[u8], ftb: u32) {
        let r = self.s.rng >> ftb;
        if s > 0 {
            let a = icdf[(s - 1) as usize] as u32;
            let b = icdf[s as usize] as u32;
            self.s.val = self.s.val.wrapping_add(self.s.rng.wrapping_sub(r.wrapping_mul(a)));
            self.s.rng = r.wrapping_mul(a.wrapping_sub(b));
        } else {
            self.s.rng = self.s.rng.wrapping_sub(r.wrapping_mul(icdf[s as usize] as u32));
        }
        self.normalize();
    }

    pub fn bits(&mut self, fl: u32, bits: u32) {
        let mut window = self.s.end_window;
        let mut used = self.s.nend_bits;
        if used + bits as i32 > EC_WINDOW_SIZE {
            loop {
                let r = self.write_byte_at_end(window & EC_SYM_MAX);
                self.s.error |= r;
                window >>= EC_SYM_BITS;
                used -= EC_SYM_BITS as i32;
                if used < EC_SYM_BITS as i32 {
                    break;
                }
            }
        }
        window |= fl.wrapping_shl(used as u32);
        used += bits as i32;
        self.s.end_window = window;
        self.s.nend_bits = used;
        self.s.nbits_total += bits as i32;
    }

    pub fn uint(&mut self, fl: u32, ft: u32) {
        let ft = ft - 1;
        let mut ftb = ilog(ft);
        if ftb > EC_UINT_BITS {
            ftb -= EC_UINT_BITS;
            let ft1 = (ft >> ftb) + 1;
            let fl1 = fl >> ftb;
            self.encode(fl1, fl1 + 1, ft1);
            self.bits(fl & ((1u32 << ftb) - 1), ftb as u32);
        } else {
            self.encode(fl, fl + 1, ft + 1);
        }
    }

    pub fn done(&mut self) {
        // Output the minimum number of bits that ensures that the symbols encoded so far are decoded correctly
        // whatever follows.
        let mut l = EC_CODE_BITS - ilog(self.s.rng);
        let mut msk = (EC_CODE_TOP - 1) >> l;
        let mut end = self.s.val.wrapping_add(msk) & !msk;
        if (end | msk) >= self.s.val.wrapping_add(self.s.rng) {
            l += 1;
            msk >>= 1;
            end = self.s.val.wrapping_add(msk) & !msk;
        }
        while l > 0 {
            self.carry_out((end >> EC_CODE_SHIFT) as i32);
            end = (end << EC_SYM_BITS) & (EC_CODE_TOP - 1);
            l -= EC_SYM_BITS as i32;
        }
        if self.s.rem >= 0 || self.s.ext > 0 {
            self.carry_out(0);
        }
        let mut window = self.s.end_window;
        let mut used = self.s.nend_bits;
        while used >= EC_SYM_BITS as i32 {
            let r = self.write_byte_at_end(window & EC_SYM_MAX);
            self.s.error |= r;
            window >>= EC_SYM_BITS;
            used -= EC_SYM_BITS as i32;
        }
        if self.s.error == 0 {
            let (offs, end_offs, storage) = (self.s.offs as usize, self.s.end_offs as usize, self.s.storage as usize);
            self.buf[offs..storage - end_offs].fill(0);
            if used > 0 {
                if self.s.end_offs >= self.s.storage {
                    self.s.error = -1;
                } else {
                    l = -l;
                    if self.s.offs + self.s.end_offs >= self.s.storage && l < used {
                        window &= (1u32 << l) - 1;
                        self.s.error = -1;
                    }
                    self.buf[storage - end_offs - 1] |= window as u8;
                }
            }
        }
    }

    /// Laplace-like energy deltas (laplace.c, LAPLACE_MINP = 1, LAPLACE_NMIN = 16).
    pub fn laplace_encode(&mut self, value: &mut i32, fs_in: u32, decay: i32) {
        let mut fl: u32 = 0;
        let mut fs = fs_in;
        let mut val = *value;
        if val != 0 {
            let s: i32 = -((val < 0) as i32);
            val = (val + s) ^ s;
            fl = fs;
            fs = laplace_get_freq1(fs, decay);
            let mut i = 1;
            while fs > 0 && i < val {
                fs = fs.wrapping_mul(2);
                fl = fl.wrapping_add(fs + 2);
                fs = fs.wrapping_mul(decay as u32) >> 15;
                i += 1;
            }
            if fs == 0 {
                let mut ndi_max = 32768u32.wrapping_sub(fl) as i32;
                ndi_max = (ndi_max - s) >> 1;
                let di = (val - i).min(ndi_max - 1);
                fl = fl.wrapping_add((2 * di + 1 + s) as u32);
                fs = 1.min(32768u32.wrapping_sub(fl) as i32) as u32;
                *value = (i + di + s) ^ s;
            } else {
                fs += 1;
                fl = fl.wrapping_add(fs & !(s as u32));
            }
        }
        self.encode_bin(fl, fl.wrapping_add(fs), 15);
    }
}

fn laplace_get_freq1(fs0: u32, decay: i32) -> u32 {
    let ft = 32768u32.wrapping_sub(2 * 16).wrapping_sub(fs0);
    ft.wrapping_mul((16384 - decay) as u32) >> 15
}
