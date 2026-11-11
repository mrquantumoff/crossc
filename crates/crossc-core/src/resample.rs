//! Sample-rate conversion: [`resample`] converts a whole signal at once (a
//! WAV file whose rate is not the codec mode's), [`StreamResampler`] converts
//! a live stream block by block with the same filter and the same arithmetic,
//! so both produce the same samples.
//!
//! Band-limited interpolation with a Kaiser-windowed sinc, evaluated from a
//! finely sampled prototype with linear interpolation between table entries,
//! so any ratio works (no rational-ratio restriction). All in f64.
//!
//! Filter: [`ZERO_CROSSINGS`] sinc lobes each side at the lower of the two
//! Nyquist frequencies (128 input taps when upsampling), Kaiser beta 12.26
//! (about -120 dB stopband). The -6 dB point sits at that Nyquist; the
//! passband is flat to about 0.94 of it (20.7 kHz for a 44.1 kHz file) and the
//! stopband starts at about 1.06 of it. Measured in the tests: a 1 kHz tone
//! keeps its level within 0.001 dB with the residual below -110 dB, and a
//! 30 kHz tone is rejected by more than 110 dB when going 96 kHz -> 48 kHz.

/// Sinc zero crossings on each side of the centre.
pub const ZERO_CROSSINGS: usize = 64;
/// Table entries per zero crossing (linear interpolation between them).
const TABLE_RES: usize = 512;
const KAISER_BETA: f64 = 12.26;

/// Modified Bessel function of the first kind, order 0.
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    let q = x * x / 4.0;
    while term > sum * 1e-17 {
        term *= q / (k * k);
        sum += term;
        k += 1.0;
    }
    sum
}

/// One-sided prototype: `table[i]` = sinc(u) * kaiser(u / Z) at
/// u = i / TABLE_RES zero crossings, for u in 0..=Z (plus one guard entry).
fn prototype() -> Vec<f64> {
    let n = ZERO_CROSSINGS * TABLE_RES;
    let i0b = bessel_i0(KAISER_BETA);
    let mut t = Vec::with_capacity(n + 2);
    for i in 0..=n {
        let u = i as f64 / TABLE_RES as f64;
        let sinc = if i == 0 {
            1.0
        } else {
            let x = std::f64::consts::PI * u;
            x.sin() / x
        };
        let r = u / ZERO_CROSSINGS as f64;
        let w = bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) / i0b;
        t.push(sinc * w);
    }
    t.push(0.0);
    t
}

/// The filter for one rate pair.
struct Kernel {
    table: Vec<f64>,
    /// Cutoff as a fraction of the input Nyquist: 1 when upsampling, out/in
    /// when downsampling (the filter then widens in input samples).
    fc: f64,
    /// Taps each side, in input samples.
    half: i64,
    /// Table index per input sample of distance.
    step: f64,
    last: f64,
    inr: u64,
    outr: u64,
}

impl Kernel {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        let fc = (f64::from(out_rate) / f64::from(in_rate)).min(1.0);
        Kernel {
            table: prototype(),
            fc,
            half: (ZERO_CROSSINGS as f64 / fc).ceil() as i64,
            step: fc * TABLE_RES as f64,
            last: (ZERO_CROSSINGS * TABLE_RES) as f64,
            inr: u64::from(in_rate),
            outr: u64::from(out_rate),
        }
    }

    /// Input frame at (or just before) output frame `n`: floor(n * in / out).
    fn centre(&self, n: u64) -> i64 {
        (n * self.inr / self.outr) as i64
    }

    /// Output frame `n` into `acc` (one value per channel). `buf` holds the
    /// input frames `base..`, of which those up to `end - 1` exist; frames
    /// before 0 and from `end` on count as silence.
    fn output(&self, n: u64, buf: &[f64], base: i64, end: i64, channels: usize, acc: &mut [f64]) {
        // Exact input position of output sample n: n * in / out.
        let num = n * self.inr;
        let centre = (num / self.outr) as i64;
        let frac = (num % self.outr) as f64 / self.outr as f64;
        let lo = (centre - self.half + 1).max(0).max(base);
        let hi = (centre + self.half).min(end - 1);
        acc.fill(0.0);
        for k in lo..=hi {
            let d = ((centre - k) as f64 + frac).abs() * self.step;
            if d >= self.last {
                continue;
            }
            let i = d as usize;
            let f = d - i as f64;
            let h = self.table[i] + (self.table[i + 1] - self.table[i]) * f;
            let o = (k - base) as usize * channels;
            let x = &buf[o..o + channels];
            for (a, s) in acc.iter_mut().zip(x) {
                *a += h * s;
            }
        }
        for a in acc.iter_mut() {
            *a *= self.fc; // unity passband gain
        }
    }
}

/// Resample interleaved `input` (`channels` channels, full scale 1.0) from
/// `in_rate` to `out_rate`. Output length is ceil(frames * out / in) frames.
pub fn resample(input: &[f64], channels: usize, in_rate: u32, out_rate: u32) -> Vec<f64> {
    assert!(channels > 0 && in_rate > 0 && out_rate > 0);
    let frames = input.len() / channels;
    if in_rate == out_rate || frames == 0 {
        return input[..frames * channels].to_vec();
    }
    let k = Kernel::new(in_rate, out_rate);
    let out_frames = (frames as u64 * u64::from(out_rate)).div_ceil(u64::from(in_rate)) as usize;
    let mut out = vec![0.0; out_frames * channels];
    for (n, acc) in out.chunks_exact_mut(channels).enumerate() {
        k.output(n as u64, input, 0, frames as i64, channels, acc);
    }
    out
}

/// Block-in, block-out resampler for a live stream. State is kept across
/// calls, so the output does not depend on how the input is cut into blocks,
/// and equals what [`resample`] gives for the same signal.
///
/// An output frame is produced once every input frame its filter reaches is
/// there, so the output lags the input by [`Self::delay_frames`] input
/// frames; [`Self::finish`] produces the tail when the input ends.
pub struct StreamResampler {
    k: Option<Kernel>,
    channels: usize,
    /// Input frames `base..total`, interleaved.
    buf: Vec<f64>,
    base: u64,
    /// Input frames seen.
    total: u64,
    /// Next output frame.
    n: u64,
}

impl StreamResampler {
    /// `channels`, `in_rate` and `out_rate` must be non-zero (a zero is
    /// treated as 1). Equal rates pass the input through.
    pub fn new(channels: usize, in_rate: u32, out_rate: u32) -> Self {
        let (in_rate, out_rate) = (in_rate.max(1), out_rate.max(1));
        StreamResampler {
            k: (in_rate != out_rate).then(|| Kernel::new(in_rate, out_rate)),
            channels: channels.max(1),
            buf: Vec::new(),
            base: 0,
            total: 0,
            n: 0,
        }
    }

    /// Input frames the output lags behind (0 when the rates are equal).
    pub fn delay_frames(&self) -> usize {
        self.k.as_ref().map_or(0, |k| k.half as usize)
    }

    /// Output frames produced so far.
    pub fn frames_out(&self) -> u64 {
        self.n
    }

    /// Feed interleaved `input` (whole frames; a trailing partial frame is
    /// ignored) and append every output frame that is now complete to `out`.
    pub fn process(&mut self, input: &[f64], out: &mut Vec<f64>) {
        let ch = self.channels;
        let frames = input.len() / ch;
        let input = &input[..frames * ch];
        let Some(k) = &self.k else {
            out.extend_from_slice(input);
            self.total += frames as u64;
            self.n = self.total;
            return;
        };
        self.buf.extend_from_slice(input);
        self.total += frames as u64;
        // Output n needs input up to centre(n) + half.
        while k.centre(self.n) + k.half < self.total as i64 {
            let at = out.len();
            out.resize(at + ch, 0.0);
            k.output(
                self.n,
                &self.buf,
                self.base as i64,
                self.total as i64,
                ch,
                &mut out[at..],
            );
            self.n += 1;
        }
        // Drop what no later output reaches back to.
        let keep_from = (k.centre(self.n) - k.half + 1).max(self.base as i64) as u64;
        let drop_frames = (keep_from - self.base).min(self.total - self.base) as usize;
        if drop_frames > 0 {
            self.buf.drain(..drop_frames * ch);
            self.base += drop_frames as u64;
        }
    }

    /// The input has ended: append the remaining output frames (up to
    /// ceil(input frames * out / in) in total), then start over.
    pub fn finish(&mut self, out: &mut Vec<f64>) {
        if let Some(k) = &self.k {
            let ch = self.channels;
            let out_frames = (self.total * k.outr).div_ceil(k.inr);
            while self.n < out_frames {
                let at = out.len();
                out.resize(at + ch, 0.0);
                k.output(
                    self.n,
                    &self.buf,
                    self.base as i64,
                    self.total as i64,
                    ch,
                    &mut out[at..],
                );
                self.n += 1;
            }
        }
        self.reset();
    }

    /// Forget all history (after a gap in the input).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.base = 0;
        self.total = 0;
        self.n = 0;
    }
}

/// Output frames a [`StreamResampler`] has produced after `frames` input
/// frames (without [`StreamResampler::finish`]).
pub fn stream_frames_out(frames: u64, in_rate: u32, out_rate: u32) -> u64 {
    if in_rate == out_rate || in_rate == 0 || out_rate == 0 {
        return frames;
    }
    let fc = (f64::from(out_rate) / f64::from(in_rate)).min(1.0);
    let half = (ZERO_CROSSINGS as f64 / fc).ceil() as u64;
    if frames <= half {
        return 0;
    }
    // Largest n with floor(n * in / out) + half < frames, plus one.
    let c = frames - half; // need floor(n * in / out) < c, i.e. n * in < c * out
    (c * u64::from(out_rate)).div_ceil(u64::from(in_rate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn tone(rate: u32, secs: f64, freq: f64, amp: f64) -> Vec<f64> {
        (0..(f64::from(rate) * secs) as usize)
            .map(|i| amp * (TAU * freq * i as f64 / f64::from(rate)).sin())
            .collect()
    }

    /// Least-squares fit of a sine at `freq` over the middle of `x`:
    /// returns (amplitude, rms of the residual), both linear.
    fn fit(x: &[f64], rate: u32, freq: f64) -> (f64, f64) {
        let x = &x[x.len() / 4..x.len() * 3 / 4];
        let w = TAU * freq / f64::from(rate);
        let (mut ss, mut cc, mut sc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (i, &v) in x.iter().enumerate() {
            let (s, c) = (w * i as f64).sin_cos();
            ss += s * s;
            cc += c * c;
            sc += s * c;
            xs += v * s;
            xc += v * c;
        }
        let det = ss * cc - sc * sc;
        let a = (xs * cc - xc * sc) / det;
        let b = (xc * ss - xs * sc) / det;
        let res = x
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let (s, c) = (w * i as f64).sin_cos();
                (v - a * s - b * c).powi(2)
            })
            .sum::<f64>()
            / x.len() as f64;
        ((a * a + b * b).sqrt(), res.sqrt())
    }

    fn db(x: f64) -> f64 {
        20.0 * x.max(1e-300).log10()
    }

    #[test]
    fn tone_survives_every_common_ratio() {
        for (i, o) in [
            (44100, 48000),
            (44100, 96000),
            (48000, 96000),
            (96000, 48000),
            (88200, 96000),
            (192000, 96000),
            (48000, 44100),
            (32000, 48000),
        ] {
            for f in [1000.0, 10000.0] {
                let y = resample(&tone(i, 0.5, f, 0.5), 1, i, o);
                let (amp, res) = fit(&y, o, f);
                let gain = db(amp / 0.5);
                assert!(gain.abs() < 0.001, "{i}->{o} {f} Hz: gain {gain} dB");
                assert!(
                    db(res / amp) < -110.0,
                    "{i}->{o} {f} Hz: residual {} dB",
                    db(res / amp)
                );
            }
        }
    }

    #[test]
    fn passband_reaches_20k_from_44k1() {
        let y = resample(&tone(44100, 0.5, 20000.0, 0.5), 1, 44100, 96000);
        let (amp, _) = fit(&y, 96000, 20000.0);
        assert!(
            db(amp / 0.5).abs() < 0.01,
            "20 kHz gain {} dB",
            db(amp / 0.5)
        );
    }

    #[test]
    fn downsampling_rejects_out_of_band_content() {
        // 30 kHz cannot exist at 48 kHz; it would alias to 18 kHz.
        let y = resample(&tone(96000, 0.5, 30000.0, 0.5), 1, 96000, 48000);
        let rms = (y[y.len() / 4..y.len() * 3 / 4]
            .iter()
            .map(|v| v * v)
            .sum::<f64>()
            / (y.len() / 2) as f64)
            .sqrt();
        assert!(db(rms / 0.5) < -110.0, "alias at {} dB", db(rms / 0.5));
    }

    #[test]
    fn length_dc_and_channels() {
        // Stereo: L = +0.25 DC, R = -0.5 DC; channels must not mix.
        let x: Vec<f64> = (0..44100).flat_map(|_| [0.25, -0.5]).collect();
        let y = resample(&x, 2, 44100, 48000);
        assert_eq!(y.len(), 48000 * 2);
        let mid = &y[48000..48010];
        for p in mid.chunks(2) {
            assert!(
                (p[0] - 0.25).abs() < 1e-6 && (p[1] + 0.5).abs() < 1e-6,
                "{p:?}"
            );
        }
        // Odd lengths round up; empty and same-rate inputs pass through.
        assert_eq!(resample(&[0.0; 3], 1, 44100, 48000).len(), 4);
        assert!(resample(&[], 2, 44100, 48000).is_empty());
        assert_eq!(resample(&[0.1, 0.2], 1, 48000, 48000), vec![0.1, 0.2]);
    }

    /// Deterministic wide-band test signal: two tones plus noise, stereo
    /// with different content per channel.
    fn signal(rate: u32, frames: usize) -> Vec<f64> {
        let mut seed = 0x1234_5678u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            f64::from(seed >> 8) / f64::from(1u32 << 24) - 0.5
        };
        (0..frames)
            .flat_map(|i| {
                let t = i as f64 / f64::from(rate);
                let l = 0.4 * (TAU * 997.0 * t).sin() + 0.1 * noise();
                let r = 0.3 * (TAU * 7919.0 * t).cos() - 0.1 * noise();
                [l, r]
            })
            .collect()
    }

    /// Feed `x` in blocks of the given sizes (cycled), collect the output.
    fn stream(x: &[f64], ch: usize, i: u32, o: u32, sizes: &[usize]) -> (Vec<f64>, Vec<f64>) {
        let mut r = StreamResampler::new(ch, i, o);
        let mut out = Vec::new();
        let mut pos = 0;
        let mut k = 0;
        let frames = x.len() / ch;
        while pos < frames {
            let n = sizes[k % sizes.len()].min(frames - pos);
            k += 1;
            let before = out.len();
            r.process(&x[pos * ch..(pos + n) * ch], &mut out);
            pos += n;
            assert_eq!((out.len() - before) % ch, 0);
            assert_eq!(
                r.frames_out(),
                stream_frames_out(pos as u64, i, o),
                "after {pos} frames"
            );
            assert_eq!(r.frames_out() as usize * ch, out.len());
        }
        let live = out.clone();
        r.finish(&mut out);
        (live, out)
    }

    #[test]
    fn streaming_equals_offline_for_irregular_blocks() {
        for (i, o) in [(96000u32, 48000u32), (48000, 44100), (44100, 48000)] {
            let frames = 6000;
            let x = signal(i, frames);
            let want = resample(&x, 2, i, o);
            for sizes in [
                &[1usize, 7, 480, 13, 1000, 2, 333][..],
                &[frames][..],
                &[1][..],
                &[441, 479, 3][..],
            ] {
                let (live, all) = stream(&x, 2, i, o, sizes);
                assert_eq!(all.len(), want.len(), "{i}->{o} {sizes:?}");
                let worst = all
                    .iter()
                    .zip(&want)
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0, f64::max);
                assert!(worst <= 1e-9, "{i}->{o} {sizes:?}: differs by {worst}");
                // Before the end, the output is a prefix that lags by the
                // filter's reach and nothing else.
                assert_eq!(live[..], all[..live.len()]);
                let lag = want.len() - live.len();
                let reach = (64.0 * f64::from(i.max(o)) / f64::from(i)).ceil() as usize;
                assert!(lag > 0 && lag <= (reach + 2) * 2, "{i}->{o}: lag {lag}");
            }
        }
    }

    #[test]
    fn stream_state_carries_over_and_resets() {
        let x = signal(96000, 3000);
        let mut r = StreamResampler::new(2, 96000, 48000);
        assert_eq!(r.delay_frames(), 128);
        let mut a = Vec::new();
        r.process(&x, &mut a);
        assert_eq!(a.len() / 2, stream_frames_out(3000, 96000, 48000) as usize);
        assert_eq!(a.len() / 2, (3000 - 128) / 2);
        // After a reset the same input gives the same output again.
        r.reset();
        let mut b = Vec::new();
        r.process(&x, &mut b);
        assert_eq!(a, b);
        // Mono, partial trailing frame ignored, equal rates pass through.
        let mut p = StreamResampler::new(2, 48000, 48000);
        let mut c = Vec::new();
        p.process(&[0.1, 0.2, 0.3], &mut c);
        assert_eq!(c, vec![0.1, 0.2]);
        assert_eq!((p.delay_frames(), p.frames_out()), (0, 1));
        assert_eq!(stream_frames_out(100, 48000, 48000), 100);
        assert_eq!(stream_frames_out(10, 96000, 48000), 0);
    }

    #[test]
    fn long_streams_keep_a_bounded_history() {
        let mut r = StreamResampler::new(1, 48000, 44100);
        let block = vec![0.25; 480];
        let mut out = Vec::new();
        for _ in 0..200 {
            out.clear();
            r.process(&block, &mut out);
            assert!(r.buf.len() <= 480 + 2 * 70 + 2, "{}", r.buf.len());
        }
        // Settled DC comes through at unity gain.
        assert!(out.iter().all(|v| (v - 0.25).abs() < 1e-6));
    }
}
