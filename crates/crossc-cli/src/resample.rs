//! Sample-rate conversion for `play --wav` when the file's rate is not the
//! codec mode's rate (e.g. a 44.1 kHz file into HiFi 48 kHz or UHQ 96 kHz).
//!
//! The whole file is converted up front with [`crossc_core::resample`], in
//! f64, and handed on as 32-bit float so the 24-bit path keeps its precision.

pub use crossc_core::resample::{ZERO_CROSSINGS, resample};

use crate::wav::{SampleFormat, Wav};

/// All samples of `wav` as f64, full scale 1.0, interleaved.
fn to_f64(wav: &Wav) -> Vec<f64> {
    let d = &wav.data;
    match wav.format {
        SampleFormat::Int { bits: 16, .. } => d
            .as_chunks::<2>()
            .0
            .iter()
            .map(|s| f64::from(i16::from_le_bytes(*s)) / 32_768.0)
            .collect(),
        SampleFormat::Int { bits: 24, .. } => d
            .as_chunks::<3>()
            .0
            .iter()
            .map(|s| f64::from(i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) / 8_388_608.0)
            .collect(),
        SampleFormat::Int { .. } => d
            .as_chunks::<4>()
            .0
            .iter()
            .map(|s| f64::from(i32::from_le_bytes(*s)) / 2_147_483_648.0)
            .collect(),
        SampleFormat::Float32 => d
            .as_chunks::<4>()
            .0
            .iter()
            .map(|s| f64::from(f32::from_le_bytes(*s)))
            .collect(),
    }
}

/// `wav` converted to `rate` as 32-bit float (unchanged if already at `rate`).
pub fn resample_wav(wav: Wav, rate: u32) -> Wav {
    if wav.sample_rate == rate {
        return wav;
    }
    let ch = usize::from(wav.channels);
    let out = resample(&to_f64(&wav), ch, wav.sample_rate, rate);
    Wav {
        sample_rate: rate,
        channels: wav.channels,
        format: SampleFormat::Float32,
        data: out.iter().flat_map(|&v| (v as f32).to_le_bytes()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_is_converted_to_float_at_the_new_rate() {
        let raw: Vec<u8> = (0..4410)
            .flat_map(|i| ((i % 100) as i16 * 100).to_le_bytes())
            .collect();
        let wav = Wav::parse(&crate::wav::tests::build(1, 1, 44100, 16, None, &raw)).unwrap();
        let out = resample_wav(wav.clone(), 96000);
        assert_eq!(
            (out.sample_rate, out.channels, out.format),
            (96000, 1, SampleFormat::Float32)
        );
        assert_eq!(out.frames(), 9600);
        assert_eq!(resample_wav(wav.clone(), 44100), wav);
    }
}
