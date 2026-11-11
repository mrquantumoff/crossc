// SSC encoder: frame header, rate rules and the per-frame encode flow.
//
// Ported from samsung-ssc-encoder src/ssc_encoder.c, Copyright (c) 2026 William Ho (WillyBilly06),
// BSD-3-Clause. The frame flow follows libopus 1.1 celt_encode_with_ec (Copyright 2007-2010 Xiph.Org Foundation,
// 2008 Gregory Maxwell, 2007-2008 CSIRO, BSD-3-Clause), with the SSC bitstream rules:
//
//   - frame = 4-byte header + range-coded payload; LM = 3 (864 samples, 8 short blocks of 108), 22 bands, always
//     two coded channels, start band 0, no pre-emphasis;
//   - total_bits = 8 * min(payload, 1275) for every budget check except TF, which uses the full payload;
//   - the pitch postfilter has no tapset symbol (tapset 0 only);
//   - "hq" rates (E-family, 256k, 328k) code no TF symbols (tf_res = 0);
//   - the coded end band comes from the header (22, 21..19 for the E-family, 18 for 152k);
//   - dynalloc quanta/caps for C=2, LM=3, then the bands 18..21 boost propagation;
//   - allocation vector table selected by bitrate > 192000;
//   - bands at or above the end band are reset to -28 after the frame.
//
// Sample scale: the codec works in 16-bit PCM units. 24-bit input is divided by 256.
// See NOTICE.

use crate::alloc::compute_allocation;
use crate::analysis::{
    alloc_trim_analysis, dynalloc_analysis, patch_transient_decision, spreading_decision, stereo_analysis, tf_analysis,
    tf_select_table, transient_analysis,
};
use crate::bands::quant_all_bands;
use crate::consts::*;
use crate::ec::{Ec, ilog};
use crate::energy::{quant_coarse_energy, quant_energy_finalise, quant_fine_energy};
use crate::math::{exp2i, f2i, fabsf, floorf, log2f, sqrtf};
use crate::mdct::Mdct;
use crate::pitch::{comb_filter, pitch_downsample, pitch_search, remove_doubling};
use crate::tables::{E_MEANS, MODE_44K, MODE_96K, ModeTables};
use crate::{Bitrate, Error, Flags, FrameInfo, Mode, PcmFrame, frame_size, payload_bytes};

const FRAME: usize = N;
const INLEN: usize = N + OVERLAP;
const NEG28: i16 = (-28 * (1 << DB_SHIFT)) as i16;

const SPREAD_ICDF: [u8; 4] = [25, 23, 2, 0];
const TRIM_ICDF: [u8; 11] = [126, 124, 119, 109, 87, 41, 19, 9, 4, 2, 0];

/// High-frequency spectral shaping ("headroom"): a fixed gain per MDCT bin, at most -1.7 dB and only above ~6-16
/// kHz depending on the rate class. Samsung's encoder applies it to every frame; this encoder does the same by
/// default (`Flags::FLAT_RESPONSE` turns it off). Values are Q15.
const HEADROOM_TAB: [i16; 240] = [
    31308, 31235, 31162, 31089, 31016, 30943, 30870, 30797, 30744, 30691, 30638, 30585, 30532, 30479, 30426, 30373,
    30320, 30267, 30234, 30201, 30168, 30135, 30102, 30069, 30036, 30003, 29970, 29937, 29904, 29871, 29838, 29805,
    29786, 29767, 29748, 29729, 29710, 29691, 29672, 29653, 29634, 29615, 29596, 29577, 29558, 29539, 29520, 29501,
    29482, 29469, 29456, 29443, 29430, 29419, 29412, 29405, 29398, 29391, 29384, 29377, 29370, 29363, 29356, 29349,
    29342, 29335, 29328, 29321, 29314, 29307, 29300, 29293, 29286, 29279, 29272, 29265, 29258, 29251, 29244, 29237,
    29230, 29223, 29216, 29209, 29202, 29195, 29188, 29181, 29174, 29167, 29160, 29153, 29146, 29139, 29132, 29125,
    29110, 29095, 29081, 29066, 29051, 29036, 29021, 29007, 28992, 28977, 28962, 28947, 28933, 28918, 28903, 28888,
    28873, 28859, 28844, 28829, 28814, 28799, 28785, 28770, 28755, 28740, 28725, 28711, 28696, 28681, 28666, 28651,
    28637, 28622, 28607, 28592, 28577, 28563, 28548, 28533, 28518, 28503, 28489, 28474, 28459, 28444, 28429, 28415,
    28400, 28385, 28370, 28355, 28341, 28326, 28311, 28296, 28281, 28267, 28252, 28237, 28222, 28207, 28193, 28178,
    28163, 28148, 28133, 28119, 28104, 28089, 28074, 28059, 28045, 28030, 28015, 28000, 27985, 27971, 27956, 27941,
    27926, 27911, 27897, 27882, 27867, 27852, 27837, 27823, 27808, 27793, 27778, 27763, 27749, 27734, 27719, 27704,
    27689, 27675, 27660, 27645, 27630, 27615, 27601, 27586, 27571, 27556, 27541, 27527, 27512, 27497, 27482, 27467,
    27453, 27438, 27423, 27408, 27393, 27379, 27364, 27349, 27334, 27319, 27305, 27290, 27275, 27260, 27245, 27231,
    27216, 27201, 27186, 27171, 27157, 27142, 27127, 27112, 27097, 27083, 27068, 27053, 27038, 27023, 27009, 26990,
];

fn headroom_gain(mode: &ModeTables, is_96k: bool, bitrate: u32, hq: bool, j: i32) -> f32 {
    let eb = |i: usize| mode.e_bands[i] as i32;
    let mut q: i32 = 32768;
    if !hq {
        if bitrate >= 96000 {
            let b0 = eb(20) * 8;
            let lo = b0 + 68;
            let hi = eb(22) * 8;
            if j >= lo && j < hi && j - b0 < 240 {
                q = HEADROOM_TAB[(j - b0) as usize] as i32;
            }
        } else {
            let i0 = eb(17) * 8;
            let i1 = eb(20) * 8 - 15;
            let i2 = eb(22) * 8;
            if j >= i0 && j < i1 {
                q = 32767 - 3 * (j - i0);
            } else if j >= i1 && j < i2 {
                q = 0x7767 - 3 * (i1 - i0) - 5 * (j - i1);
            }
        }
    } else if !is_96k {
        if bitrate < 270000 {
            if (350..384).contains(&j) {
                q = 30620;
            } else if (384..409).contains(&j) {
                q = 28893;
            } else if j >= 409 {
                q = 30620;
            }
        } else if (354..384).contains(&j) {
            q = 30720;
        } else if (384..400).contains(&j) {
            q = 28988;
        } else if (400..409).contains(&j) {
            q = 28893;
        } else if j >= 409 {
            q = 30620;
        }
    } else if bitrate < 270000 {
        if (288..313).contains(&j) {
            q = 30920;
        } else if (313..350).contains(&j) {
            q = 30750;
        } else if j >= 350 {
            q = 30620;
        }
    } else if (288..313).contains(&j) {
        q = 30920;
    } else if (313..354).contains(&j) {
        q = 30820;
    } else if (354..400).contains(&j) {
        q = 30720;
    } else if j >= 400 {
        q = 30620;
    }
    q as f32 * (1.0f32 / 32768.0f32)
}

fn check_channels(channels: usize) -> Result<(), Error> {
    if channels != 1 && channels != 2 {
        return Err(Error::InvalidChannels(channels));
    }
    Ok(())
}

/// SSC encoder state. One instance per stream; instances are independent (no global state). Not `Sync`-shared
/// by design: encoding needs `&mut self`.
#[cfg_attr(feature = "alloc", derive(bytemuck::Zeroable))]
pub struct Encoder {
    mode: Mode,
    channels: usize,
    is_96k: bool,
    flags: Flags,
    mdct: Mdct,
    // work buffers (the C work_t)
    input: [[f32; INLEN]; C], // [108 history | 864 new] per channel
    freq: [f32; C * FRAME],   // MDCT, channel-major
    x: [f32; C * FRAME],      // normalised spectrum
    band_e: [f32; C * NB],
    band_log_e: [f32; C * NB],
    band_log_e2: [f32; C * NB],
    pre: [[f32; PF_MAXPERIOD + FRAME]; C], // unfiltered input with pitch history
    pitch_buf: [f32; (PF_MAXPERIOD + FRAME) / 2],
    stage: [f32; C * FRAME], // interleaved float staging buffer for the front ends
    // persistent state
    in_mem: [[f32; OVERLAP]; C],             // prefiltered input history
    prefilter_mem: [[f32; PF_MAXPERIOD]; C], // unfiltered input history
    prefilter_period: i32,
    prefilter_gain: f32,
    overlap_max: f32,
    old_band_e: [i16; C * NB],
    seq: i32,
    spread_decision: i32,
    tonal_average: i32,
    consec_transient: i32,
    last_coded_bands: i32,
    stereo_saving: f32,
    info: FrameInfo,
    has_info: bool,
}

impl core::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Encoder")
            .field("mode", &self.mode)
            .field("channels", &self.channels)
            .field("flags", &self.flags)
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

impl Encoder {
    /// Creates an encoder for a codec mode. `channels` is 2 (interleaved stereo) or 1 (mono, duplicated to both
    /// coded channels; the SSC bitstream always codes two channels).
    pub fn new(mode: Mode, channels: usize) -> Result<Encoder, Error> {
        check_channels(channels)?;
        let mut e = Encoder::BLANK;
        e.init(mode, channels);
        Ok(e)
    }

    /// Like [`Encoder::new`], but builds the encoder directly on the heap (feature `alloc`, on with `std`).
    ///
    /// `Box::new(Encoder::new(..))` first builds the ~75 KB state on the stack; this does not, which matters on
    /// small-stack threads, in debug and release builds. It allocates pre-zeroed heap memory (an all-zero encoder is
    /// [`Encoder::BLANK`]) and initialises it in place. The result is the same encoder as [`Encoder::new`] gives.
    #[cfg(feature = "alloc")]
    pub fn new_boxed(mode: Mode, channels: usize) -> Result<alloc_crate::boxed::Box<Encoder>, Error> {
        check_channels(channels)?;
        // zeroed_box allocates already-zeroed heap memory, so nothing ~75 KB ever lives on the stack (in debug
        // builds too, unlike Box::new(Encoder::BLANK)). An all-zero Encoder equals Encoder::BLANK.
        let mut e: alloc_crate::boxed::Box<Encoder> = bytemuck::zeroed_box();
        e.init(mode, channels);
        Ok(e)
    }

    /// An all-zero encoder; [`Encoder::init`] turns it into a working one.
    const BLANK: Encoder = Encoder {
        mode: Mode::Ssc44k1,
        channels: 2,
        is_96k: false,
        flags: Flags::NONE,
        mdct: Mdct::ZERO,
        input: [[0.0; INLEN]; C],
        freq: [0.0; C * FRAME],
        x: [0.0; C * FRAME],
        band_e: [0.0; C * NB],
        band_log_e: [0.0; C * NB],
        band_log_e2: [0.0; C * NB],
        pre: [[0.0; PF_MAXPERIOD + FRAME]; C],
        pitch_buf: [0.0; (PF_MAXPERIOD + FRAME) / 2],
        stage: [0.0; C * FRAME],
        in_mem: [[0.0; OVERLAP]; C],
        prefilter_mem: [[0.0; PF_MAXPERIOD]; C],
        prefilter_period: 0,
        prefilter_gain: 0.0,
        overlap_max: 0.0,
        old_band_e: [0; C * NB],
        seq: 0,
        spread_decision: 0,
        tonal_average: 0,
        consec_transient: 0,
        last_coded_bands: 0,
        stereo_saving: 0.0,
        info: FrameInfo::ZERO,
        has_info: false,
    };

    /// The mode's static tables.
    fn tables(&self) -> &'static ModeTables {
        if self.is_96k { &MODE_96K } else { &MODE_44K }
    }

    /// Configures a [`Encoder::BLANK`] encoder in place.
    fn init(&mut self, mode: Mode, channels: usize) {
        let is_96k = mode.is_96k();
        self.mode = mode;
        self.channels = channels;
        self.is_96k = is_96k;
        self.flags = Flags::NONE;
        self.mdct.init();
        self.reset();
    }

    /// Clears all history and restarts the sequence counter at 0 (keeps the mode, channels and flags).
    pub fn reset(&mut self) {
        self.in_mem = [[0.0; OVERLAP]; C];
        self.prefilter_mem = [[0.0; PF_MAXPERIOD]; C];
        self.prefilter_period = PF_MINPERIOD;
        self.prefilter_gain = 0.0;
        self.overlap_max = 0.0;
        self.old_band_e = [0; C * NB];
        self.seq = 0;
        self.spread_decision = SPREAD_NORMAL;
        self.tonal_average = 256;
        self.consec_transient = 0;
        self.last_coded_bands = 0;
        self.stereo_saving = 0.0;
        self.has_info = false;
    }

    /// The codec mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Input channels (1 or 2).
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Current options.
    pub fn flags(&self) -> Flags {
        self.flags
    }

    /// Sets the options used by the following frames.
    pub fn set_flags(&mut self, flags: Flags) {
        self.flags = flags;
    }

    /// Details of the last successfully encoded frame, if any.
    pub fn last_frame_info(&self) -> Option<FrameInfo> {
        self.has_info.then_some(self.info)
    }

    /// Encodes one frame of `FRAME_SAMPLES` samples per channel (interleaved) at `bitrate` into `out` and
    /// returns the number of bytes written (always [`frame_size`]`(mode, bitrate)`). The bitrate may change from
    /// frame to frame.
    pub fn encode_frame(&mut self, pcm: PcmFrame<'_>, bitrate: Bitrate, out: &mut [u8]) -> Result<usize, Error> {
        let expected = FRAME * self.channels;
        if pcm.len() != expected {
            return Err(Error::InvalidSampleCount { expected, got: pcm.len() });
        }
        let needed = frame_size(self.mode, bitrate);
        if out.len() < needed {
            return Err(Error::BufferTooSmall { needed, got: out.len() });
        }
        let mono = self.channels == 1;
        // Staging exactly as the C front ends: (float)sample, then encode_frame() multiplies by `scale`.
        let (scale, lsb_depth) = match pcm {
            PcmFrame::I16(s) => {
                self.stage_from(|i| s[i] as f32, mono);
                let hq16 = self.flags.contains(Flags::SAMSUNG_HQ16_LEVEL) && bitrate.is_hq();
                (if hq16 { 1.0f32 / 256.0f32 } else { 1.0f32 }, 16)
            }
            PcmFrame::I24(s) => {
                self.stage_from(|i| s[i] as f32, mono);
                (exp2i(16 - 24), 24)
            }
            PcmFrame::S32 { samples, bits } => {
                if !(16..=32).contains(&bits) {
                    return Err(Error::InvalidBitDepth(bits));
                }
                self.stage_from(|i| samples[i] as f32, mono);
                (exp2i(16 - bits as i32), if bits > 24 { 24 } else { bits as i32 })
            }
            PcmFrame::F32(s) => {
                self.stage_from(|i| s[i], mono);
                (32768.0f32, 24)
            }
        };
        self.encode_staged(scale, lsb_depth, bitrate, out)
    }

    fn stage_from(&mut self, f: impl Fn(usize) -> f32, mono: bool) {
        if mono {
            for i in 0..FRAME {
                let v = f(i);
                self.stage[2 * i] = v;
                self.stage[2 * i + 1] = v;
            }
        } else {
            for i in 0..C * FRAME {
                self.stage[i] = f(i);
            }
        }
    }

    // ---- analysis helpers ----
    fn compute_mdcts(&mut self, short_blocks: bool) {
        for c in 0..C {
            self.mdct.frame(&self.input[c], short_blocks, &mut self.freq[c * FRAME..(c + 1) * FRAME]);
        }
    }

    fn bin_gain(&self, r: Bitrate, j: i32) -> f32 {
        headroom_gain(self.tables(), self.is_96k, r.bps(), r.is_hq(), j)
    }

    /// Spectrum shaping after each MDCT: the headroom gain (unless flat), and for bitrates below 192 kb/s nothing
    /// is coded above eBands[21] (band 21 itself is still coded, empty). Same bin index for long and interleaved
    /// short spectra.
    fn shape_spectrum(&mut self, r: Bitrate, short_blocks: bool, flat: bool) {
        let start = self.tables().e_bands[21] as usize * 8;
        if !flat {
            for j in 0..FRAME {
                let g = self.bin_gain(r, j as i32);
                if g != 1.0 {
                    for c in 0..C {
                        self.freq[c * FRAME + j] *= g;
                    }
                }
            }
        }
        if r.bps() >= 192000 || (short_blocks && r.is_hq()) {
            return;
        }
        for c in 0..C {
            self.freq[c * FRAME + start..(c + 1) * FRAME].fill(0.0);
        }
    }

    fn compute_band_energies(&mut self) {
        let eb = self.tables().e_bands;
        for c in 0..C {
            for i in 0..NB {
                let x = &self.freq[c * FRAME..];
                let mut sum = 1e-27f32;
                for j in eb[i] as usize * 8..eb[i + 1] as usize * 8 {
                    sum += x[j] * x[j];
                }
                let mut e = sqrtf(sum);
                self.band_e[c * NB + i] = e;
                e = log2f(e) - E_MEANS[i];
                self.band_log_e[c * NB + i] = if e < -28.0 { -28.0 } else { e };
            }
        }
    }

    fn normalise_bands(&mut self) {
        let eb = self.tables().e_bands;
        self.x = [0.0; C * FRAME];
        for c in 0..C {
            for i in 0..NB {
                let g = 1.0f32 / (1e-15f32 + self.band_e[c * NB + i]);
                for j in eb[i] as usize * 8..eb[i + 1] as usize * 8 {
                    self.x[c * FRAME + j] = self.freq[c * FRAME + j] * g;
                }
            }
        }
    }

    /// L2 norm of bin ranges [prev, parts[k]) of both channels, in the scale of Samsung's fixed-point spectrum
    /// (16-bit PCM unit = 4096), as used for the end-band side information. e[c*n + k]. The spectrum is measured
    /// with the headroom gain applied (done here if the coded spectrum is flat).
    fn part_energies(&self, r: Bitrate, flat: bool, start: i32, parts: &[i16], e: &mut [f32; 16]) {
        let n = parts.len();
        for c in 0..C {
            let mut prev = start;
            for k in 0..n {
                let mut s = 0f32;
                for j in prev..parts[k] as i32 {
                    let v = self.freq[c * FRAME + j as usize] * if flat { self.bin_gain(r, j) } else { 1.0 };
                    s += v * v;
                }
                e[c * n + k] = 4096.0 * sqrtf(s) + 1.0;
                if parts[k] as i32 > prev {
                    prev = parts[k] as i32;
                }
            }
        }
    }

    /// Header side information. Returns `red` (number of uncoded top bands), the 2-bit field for byte 2 and the
    /// top 3 bits of byte 3.
    fn end_band_info(&self, r: Bitrate, flat: bool) -> (usize, i32, i32) {
        const PARTS_NONHQ: [i16; 4] = [708, 744, 780, 816];
        const PARTS_E: [[i16; 8]; 4] = [
            [738, 756, 774, 792, 810, 828, 846, 864],
            [594, 612, 630, 648, 666, 684, 702, 720],
            [450, 468, 486, 504, 522, 540, 558, 576],
            [306, 324, 342, 360, 378, 396, 414, 432],
        ];
        let mut e = [0f32; 16];
        if !r.is_hq() {
            // last-band activity of band 21 (side information only, nothing in the payload depends on it)
            let mut lb = 0;
            self.part_energies(r, flat, self.tables().e_bands[21] as i32 * 8, &PARTS_NONHQ, &mut e);
            if e[3] + e[7] >= 30002.0 {
                lb = 3;
            } else if e[2] + e[6] >= 30002.0 {
                lb = 2;
            } else if e[1] + e[5] >= 30002.0 {
                lb = 1;
            }
            return (0, lb, 0);
        }
        if r == Bitrate::K152 {
            return (4, 0, 0); // 152k: 18 bands, nothing signalled
        }
        if r.is_e_family() {
            for red in 0..4 {
                self.part_energies(r, flat, self.tables().e_bands[21 - red] as i32 * 8, &PARTS_E[red], &mut e);
                for k in (0..8).rev() {
                    if e[k] + e[8 + k] > 12001.0 {
                        // (sub - 1) << 5 with sub = k + 1
                        return (red, red as i32, (k as i32) << 5);
                    }
                }
            }
            return (3, 3, 0);
        }
        (0, 0, 0) // 256k, 328k
    }

    /// Pitch prefilter: finds a pitch period and gain, and filters the new input samples in place with the comb
    /// filter x - g*comb(x) (the decoder's postfilter is the inverse IIR). The filter crossfades from the previous
    /// frame's parameters over the first 108 samples. Returns (postfilter on, pitch, gain index).
    fn run_prefilter(&mut self, enabled: bool, nb_avail: i32) -> (bool, i32, i32) {
        let mut pitch_index = PF_MINPERIOD;
        let mut qg = 0;
        let mut gain1 = 0f32;
        let pf_on;
        for c in 0..C {
            self.pre[c][..PF_MAXPERIOD].copy_from_slice(&self.prefilter_mem[c]);
            self.pre[c][PF_MAXPERIOD..].copy_from_slice(&self.input[c][OVERLAP..]);
        }
        if enabled {
            pitch_downsample(&self.pre[0], &self.pre[1], &mut self.pitch_buf, PF_MAXPERIOD + FRAME);
            // the last 1.5 octaves of the range are skipped (false positives from short-term correlation)
            pitch_index = pitch_search(
                &self.pitch_buf[PF_MAXPERIOD >> 1..],
                &self.pitch_buf,
                FRAME,
                PF_MAXPERIOD - 3 * PF_MINPERIOD as usize,
            );
            pitch_index = PF_MAXPERIOD as i32 - pitch_index;
            gain1 = remove_doubling(
                &self.pitch_buf,
                PF_MAXPERIOD as i32,
                PF_MINPERIOD,
                FRAME as i32,
                &mut pitch_index,
                self.prefilter_period,
                self.prefilter_gain,
            );
            if pitch_index > PF_MAXPERIOD as i32 - 2 {
                pitch_index = PF_MAXPERIOD as i32 - 2;
            }
            gain1 *= 0.7;
        }
        // gain threshold, adjusted for continuity and rate
        let mut pf_threshold = 0.2f32;
        if (pitch_index - self.prefilter_period).abs() * 10 > pitch_index {
            pf_threshold += 0.2;
        }
        if nb_avail < 25 {
            pf_threshold += 0.1;
        }
        if nb_avail < 35 {
            pf_threshold += 0.1;
        }
        if self.prefilter_gain > 0.4 {
            pf_threshold -= 0.1;
        }
        if self.prefilter_gain > 0.55 {
            pf_threshold -= 0.1;
        }
        if pf_threshold < 0.2 {
            pf_threshold = 0.2;
        }
        if gain1 < pf_threshold {
            gain1 = 0.0;
            pf_on = false;
        } else {
            if fabsf(gain1 - self.prefilter_gain) < 0.1 {
                gain1 = self.prefilter_gain;
            }
            qg = f2i(floorf(0.5 + gain1 * 32.0 / 3.0)).wrapping_sub(1); // C int arithmetic (NaN gain -> INT_MIN - 1)
            qg = 0.max(7.min(qg));
            gain1 = 0.09375 * (qg + 1) as f32;
            pf_on = true;
        }
        if self.prefilter_period < PF_MINPERIOD {
            self.prefilter_period = PF_MINPERIOD;
        }
        for c in 0..C {
            comb_filter(
                &mut self.input[c][OVERLAP..],
                &self.pre[c],
                PF_MAXPERIOD,
                self.prefilter_period,
                pitch_index,
                FRAME,
                -self.prefilter_gain,
                -gain1,
                OVERLAP,
            );
            self.prefilter_mem[c].copy_from_slice(&self.pre[c][FRAME..FRAME + PF_MAXPERIOD]);
        }
        self.prefilter_period = pitch_index;
        self.prefilter_gain = gain1;
        (pf_on, pitch_index, qg)
    }

    // ---- one frame ----
    fn encode_staged(&mut self, scale: f32, lsb_depth: i32, r: Bitrate, out: &mut [u8]) -> Result<usize, Error> {
        let flags = self.flags;
        let mode = self.tables();
        let e_bands = mode.e_bands;
        let flat = flags.contains(Flags::FLAT_RESPONSE);
        let no_transients = flags.contains(Flags::NO_TRANSIENTS);
        let payload = payload_bytes(self.is_96k, r);
        let frame_len = payload + 4;
        let nb_avail = if payload < 1275 { payload as i32 } else { 1275 };
        let total_bits = 8 * nb_avail;
        let mut tf_res = [0i32; NB];
        let mut offsets = [0i32; NB];
        let mut req = [0i32; NB];
        let mut pulses = [0i32; NB];
        let mut fine_quant = [0i32; NB];
        let mut fine_priority = [0i32; NB];
        let mut x16 = [0i16; C * NB];
        let mut error = [0i16; C * NB];
        let mut tf_estimate = 0f32;
        let mut tf_chan = 0usize;
        let mut tf_select = 0i32;
        let mut transient_got_disabled = false;

        // input: [previous 108 | new 864]
        let mut sample_max = self.overlap_max;
        for c in 0..C {
            self.input[c][..OVERLAP].copy_from_slice(&self.in_mem[c]);
            for i in 0..FRAME {
                self.input[c][OVERLAP + i] = self.stage[2 * i + c] * scale;
            }
        }
        self.overlap_max = 0.0;
        for c in 0..C {
            for i in 0..FRAME {
                let a = fabsf(self.input[c][OVERLAP + i]);
                if i >= FRAME - OVERLAP {
                    if a > self.overlap_max {
                        self.overlap_max = a;
                    }
                } else if a > sample_max {
                    sample_max = a;
                }
            }
        }
        if self.overlap_max > sample_max {
            sample_max = self.overlap_max;
        }
        let silence = sample_max == 0.0;

        let (head, rest) = out.split_at_mut(4);
        let mut enc = Ec::new(&mut rest[..payload]);

        // silence
        enc.bit_logp(silence as i32, 15);
        if silence {
            enc.s.nbits_total += total_bits - enc.tell();
        }

        // pitch prefilter / postfilter parameters (no tapset symbol in SSC)
        let (pf_on, pf_pitch, pf_qg) =
            self.run_prefilter(!silence && nb_avail >= 25 && !flags.contains(Flags::NO_POSTFILTER), nb_avail);
        if !pf_on {
            if enc.tell() + 16 <= total_bits {
                enc.bit_logp(0, 1);
            }
        } else {
            let p1 = pf_pitch + 1;
            enc.bit_logp(1, 1);
            let octave = ilog(p1 as u32) - 5;
            enc.uint(octave as u32, 6);
            enc.bits((p1 - (16 << octave)) as u32, (4 + octave) as u32);
            enc.bits(pf_qg as u32, 3);
        }

        // transient decision and MDCT
        let mut is_transient = false;
        if !silence && !no_transients {
            let t = transient_analysis(&self.input, INLEN);
            is_transient = t.is_transient;
            tf_estimate = t.tf_estimate;
            tf_chan = t.tf_chan;
        }
        if enc.tell() + 3 > total_bits {
            is_transient = false;
            transient_got_disabled = true;
        }
        let mut short_blocks = is_transient;
        self.compute_mdcts(false);
        self.shape_spectrum(r, false, flat);
        self.compute_band_energies();
        self.band_log_e2 = self.band_log_e;
        if !is_transient && !transient_got_disabled && !silence && !no_transients {
            let mut oldf = [0f32; C * NB];
            for i in 0..C * NB {
                oldf[i] = self.old_band_e[i] as f32 * (1.0f32 / (1 << DB_SHIFT) as f32);
            }
            if patch_transient_decision(&self.band_log_e, &oldf, NB) {
                is_transient = true;
                tf_estimate = 0.2;
            }
        }
        if is_transient {
            // band_log_e2 keeps the long-block energies (+1.5 for the short/long scale) for dynalloc
            for v in self.band_log_e2.iter_mut() {
                *v += 0.5 * LM as f32;
            }
            short_blocks = true;
            self.compute_mdcts(true);
            self.shape_spectrum(r, true, flat);
            self.compute_band_energies();
        }
        if enc.tell() + 3 <= total_bits {
            enc.bit_logp(is_transient as i32, 3);
        }

        // header
        let (red, b2_field, b3_top) = self.end_band_info(r, flat);
        let end = NB - red;
        let seq = self.seq;
        head[0] = 0xFF;
        head[1] = if r.is_e_family() { (0xE0 | ((seq >> 4) & 0xF)) as u8 } else { 0xEE };
        head[2] = (((seq & 0xF) << 4) | (b2_field << 2) | 1) as u8;
        // E-family (except 152k): the top 3 bits of byte 3 carry sub-1 of the last active part (0 if none)
        head[3] =
            if r.is_e_family() && r != Bitrate::K152 { ((r.code() as i32 & 0x1F) | b3_top) as u8 } else { r.code() };

        self.normalise_bands();

        // TF resolution (not coded for hq rates)
        let it = is_transient as i32;
        for v in tf_res.iter_mut() {
            *v = it;
        }
        if !r.is_hq() && nb_avail >= 15 * C as i32 {
            let lambda = if nb_avail < 40 {
                12
            } else if nb_avail < 60 {
                6
            } else if nb_avail < 100 {
                4
            } else {
                3
            };
            tf_select = tf_analysis(mode, NB, is_transient, &mut tf_res, 2 * lambda, &self.x, tf_estimate, tf_chan);
        }

        // coarse energy
        for i in 0..C * NB {
            let v = self.band_log_e[i] * (1 << DB_SHIFT) as f32;
            x16[i] = f2i(floorf(v + 0.5)) as i16;
        }
        quant_coarse_energy(end, &x16, &mut self.old_band_e, total_bits, nb_avail, &mut error, &mut enc);

        // TF symbols: budget is the full payload
        if !r.is_hq() {
            let mut budget = enc.s.storage * 8;
            let mut t = enc.tell() as u32;
            let mut logp: u32 = if is_transient { 2 } else { 4 };
            let mut curr = 0i32;
            let mut tf_changed = 0i32;
            let tf_select_rsv = t + logp < budget; // C: t + logp + 1 <= budget
            budget -= tf_select_rsv as u32;
            for i in 0..NB {
                if t + logp <= budget {
                    enc.bit_logp(tf_res[i] ^ curr, logp);
                    t = enc.tell() as u32;
                    curr = tf_res[i];
                    tf_changed |= curr;
                } else {
                    tf_res[i] = curr;
                }
                logp = if is_transient { 4 } else { 5 };
            }
            if tf_select_rsv && tf_select_table(4 * it + tf_changed) != tf_select_table(4 * it + 2 + tf_changed) {
                enc.bit_logp(tf_select, 1);
            } else {
                tf_select = 0;
            }
            for v in tf_res.iter_mut() {
                *v = tf_select_table(4 * it + 2 * tf_select + *v);
            }
        } else {
            tf_res = [0; NB];
        }

        // spreading
        let mut spread = SPREAD_NORMAL;
        if enc.tell() + 4 <= total_bits {
            if !short_blocks && nb_avail >= 10 * C as i32 {
                spread = spreading_decision(mode, &self.x, &mut self.tonal_average, self.spread_decision, end);
            } else {
                spread = SPREAD_NORMAL;
            }
            self.spread_decision = spread;
            enc.icdf(spread, &SPREAD_ICDF, 5);
        }

        // dynamic allocation
        dynalloc_analysis(mode, &self.band_log_e, &self.band_log_e2, end, &mut req, lsb_depth, is_transient, nb_avail);
        let mut dynalloc_logp = 6;
        let total_bits8 = total_bits << BITRES;
        let mut total_boost = 0i32;
        let mut tell = enc.tell_frac() as i32;
        for i in 0..end {
            let quanta = 48.max(((e_bands[i + 1] - e_bands[i]) as i32) << 4);
            let mut loop_logp = dynalloc_logp;
            let mut boost = 0;
            let mut j = 0;
            while tell + (loop_logp << BITRES) < total_bits8 - total_boost && boost < mode.cap[i] {
                let flag = j < req[i];
                enc.bit_logp(flag as i32, loop_logp as u32);
                tell = enc.tell_frac() as i32;
                if !flag {
                    break;
                }
                boost += quanta;
                total_boost += quanta;
                loop_logp = 1;
                j += 1;
            }
            if boost > 0 {
                dynalloc_logp = 2.max(dynalloc_logp - 1);
            }
            offsets[i] = boost;
        }
        // SSC rule: a large boost in band 18/19/20 also applies to the next band, depending on channel 0's coarse
        // energies (applied identically by the decoder, nothing is coded).
        {
            let e = &self.old_band_e;
            if offsets[18] > 1000 && offsets[19] == 0 {
                if e[19] > 7000 || (e[18] >> 1) < e[19] {
                    offsets[19] = offsets[18];
                }
            } else if offsets[19] > 1000 && offsets[20] == 0 {
                if e[20] > 7000 || (e[19] >> 1) < e[20] {
                    offsets[20] = offsets[19];
                }
            } else if offsets[20] > 1000 && offsets[21] == 0 {
                if e[21] > 7000 || (e[20] >> 1) < e[21] {
                    offsets[21] = offsets[20];
                }
            }
        }

        // stereo decisions: mid/side for every band (no intensity), dual stereo from an L1 estimate
        let mut dual_stereo = stereo_analysis(mode, &self.x) as i32;
        let mut intensity = end as i32;

        // allocation trim
        let mut alloc_trim = 5;
        if tell + (6 << BITRES) <= total_bits8 - total_boost {
            alloc_trim = alloc_trim_analysis(
                mode,
                &self.x,
                &self.band_log_e,
                end,
                &mut self.stereo_saving,
                tf_estimate,
                intensity as usize,
            );
            enc.icdf(alloc_trim, &TRIM_ICDF, 7);
        }

        // bit allocation
        let mut bits = total_bits8 - enc.tell_frac() as i32 - 1;
        let anti_collapse_rsv = if is_transient && bits >= (LM + 2) << BITRES { 1 << BITRES } else { 0 };
        bits -= anti_collapse_rsv;
        let a = compute_allocation(
            mode,
            end,
            &offsets,
            alloc_trim,
            r.bps() > 192000,
            bits,
            &mut pulses,
            &mut fine_quant,
            &mut fine_priority,
            &mut intensity,
            &mut dual_stereo,
            self.last_coded_bands,
            end as i32 - 1,
            &mut enc,
        );
        let coded_bands = a.coded_bands as i32;
        if self.last_coded_bands != 0 {
            self.last_coded_bands = (self.last_coded_bands + 1).min((self.last_coded_bands - 1).max(coded_bands));
        } else {
            self.last_coded_bands = coded_bands;
        }

        quant_fine_energy(end, &mut self.old_band_e, &mut error, &fine_quant, &mut enc);
        {
            let (xs, ys) = self.x.split_at_mut(FRAME);
            quant_all_bands(
                mode,
                end,
                xs,
                ys,
                &self.band_e,
                &pulses,
                short_blocks,
                spread,
                dual_stereo != 0,
                intensity,
                &tf_res,
                total_bits8 - anti_collapse_rsv,
                a.balance,
                &mut enc,
                coded_bands,
            );
        }
        if anti_collapse_rsv > 0 {
            enc.bits((self.consec_transient < 2) as u32, 1);
        }
        let bits_left = total_bits - enc.tell();
        quant_energy_finalise(end, &mut self.old_band_e, &mut error, &fine_quant, &fine_priority, bits_left, &mut enc);

        // end-of-frame state, as in the decoder
        if silence {
            self.old_band_e = [NEG28; C * NB];
        }
        for c in 0..C {
            for i in end..NB {
                self.old_band_e[c * NB + i] = NEG28;
            }
        }
        if is_transient || transient_got_disabled {
            self.consec_transient += 1;
        } else {
            self.consec_transient = 0;
        }

        let tell_bits = enc.tell();
        enc.done();
        if enc.s.error != 0 {
            return Err(Error::Internal);
        }

        for c in 0..C {
            self.in_mem[c].copy_from_slice(&self.input[c][FRAME..FRAME + OVERLAP]);
        }

        self.has_info = true;
        self.info = FrameInfo {
            bytes: frame_len,
            bitrate: r.bps(),
            hq: r.is_hq(),
            seq: seq as u8,
            end_band: end as u8,
            transient: is_transient,
            pf_on,
            pf_period: if pf_on { pf_pitch as u16 } else { 0 },
            pf_gain_q: if pf_on { pf_qg as u8 } else { 0 },
            silence,
            coded_bands: coded_bands as u8,
            tell_bits: tell_bits as u32,
            total_bits: total_bits as u32,
        };
        self.seq = (self.seq + 1) & 0xFF;
        Ok(frame_len)
    }
}
