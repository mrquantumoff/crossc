//! Fixed codec parameters.
// Ported from samsung-ssc-encoder src/ssc_enc_internal.h, Copyright (c) 2026 William Ho (WillyBilly06),
// BSD-3-Clause; parts adapted from libopus 1.1 (CELT), Copyright Xiph.Org Foundation and contributors,
// BSD-3-Clause. See NOTICE.

/// Samples per channel per frame = long MDCT coefficient count.
pub(crate) const N: usize = 864;
/// Window overlap, also the short MDCT size.
pub(crate) const OVERLAP: usize = 108;
pub(crate) const SHORT: usize = 108;
/// Short blocks per frame (1 << LM).
pub(crate) const M: i32 = 8;
pub(crate) const LM: i32 = 3;
/// The bitstream always codes two channels.
pub(crate) const C: usize = 2;
/// Bands.
pub(crate) const NB: usize = 22;
pub(crate) const BITRES: i32 = 3;
pub(crate) const DB_SHIFT: i32 = 10;
pub(crate) const MAX_FINE: i32 = 8;
/// Widest band: 18 * 8 bins.
pub(crate) const MAX_BAND_N: usize = 144;

pub(crate) const SPREAD_NONE: i32 = 0;
pub(crate) const SPREAD_LIGHT: i32 = 1;
pub(crate) const SPREAD_NORMAL: i32 = 2;
pub(crate) const SPREAD_AGGRESSIVE: i32 = 3;

pub(crate) const PF_MINPERIOD: i32 = 15;
pub(crate) const PF_MAXPERIOD: usize = 1024;
