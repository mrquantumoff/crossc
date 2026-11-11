//! Minimal RIFF/WAVE reader: PCM 16/24/32-bit integer and 32-bit float,
//! mono or stereo, plain `fmt ` or WAVE_FORMAT_EXTENSIBLE. The whole data
//! chunk is loaded into memory.

use std::fmt;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// Tail of KSDATAFORMAT_SUBTYPE_PCM / _IEEE_FLOAT after the 2-byte format tag.
const SUBTYPE_TAIL: [u8; 14] = [
    0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// Sample encoding of the data chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    /// Signed little-endian integer in a `bits`-bit container (16, 24 or 32).
    /// `valid_bits` (from WAVE_FORMAT_EXTENSIBLE) is informational: the
    /// samples are left-justified in the container, so full scale is always
    /// the container's full scale.
    Int { bits: u16, valid_bits: u16 },
    /// IEEE float, 32-bit.
    Float32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Wav {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
    /// Raw interleaved sample bytes (whole frames only).
    pub data: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct WavError(pub String);

impl fmt::Display for WavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for WavError {}

fn err<T>(s: impl Into<String>) -> Result<T, WavError> {
    Err(WavError(s.into()))
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl Wav {
    pub fn parse(b: &[u8]) -> Result<Self, WavError> {
        if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
            return err("not a RIFF/WAVE file");
        }
        let mut fmt: Option<(u32, u16, SampleFormat, u16)> = None;
        let mut data: Option<&[u8]> = None;
        let mut o = 12;
        while o + 8 <= b.len() {
            let id = &b[o..o + 4];
            let len = u32le(b, o + 4) as usize;
            let body_start = o + 8;
            // A truncated last chunk (common for data written by a crashed
            // recorder, or a 0xFFFFFFFF streaming length) is cut to the file end.
            let body = &b[body_start..b.len().min(body_start.saturating_add(len))];
            match id {
                b"fmt " => fmt = Some(Self::parse_fmt(body)?),
                b"data" => data = Some(body),
                _ => {}
            }
            o = body_start.saturating_add(len).saturating_add(len & 1);
        }
        let Some((sample_rate, channels, format, block_align)) = fmt else {
            return err("no fmt chunk");
        };
        let Some(data) = data else {
            return err("no data chunk");
        };
        let whole = data.len() / usize::from(block_align) * usize::from(block_align);
        Ok(Self {
            sample_rate,
            channels,
            format,
            data: data[..whole].to_vec(),
        })
    }

    fn parse_fmt(f: &[u8]) -> Result<(u32, u16, SampleFormat, u16), WavError> {
        if f.len() < 16 {
            return err("fmt chunk too short");
        }
        let mut tag = u16le(f, 0);
        let channels = u16le(f, 2);
        let rate = u32le(f, 4);
        let block_align = u16le(f, 12);
        let bits = u16le(f, 14);
        let mut valid_bits = bits;
        if tag == WAVE_FORMAT_EXTENSIBLE {
            if f.len() < 40 {
                return err("WAVE_FORMAT_EXTENSIBLE fmt chunk too short");
            }
            let vb = u16le(f, 18);
            if vb != 0 {
                valid_bits = vb;
            }
            if f[26..40] != SUBTYPE_TAIL {
                return err("WAVE_FORMAT_EXTENSIBLE with an unknown sub-format GUID");
            }
            tag = u16le(f, 24);
        }
        if !(1..=2).contains(&channels) {
            return err(format!(
                "{channels} channels: only mono and stereo are supported"
            ));
        }
        if rate == 0 {
            return err("sample rate 0");
        }
        let format = match (tag, bits) {
            (WAVE_FORMAT_PCM, 16 | 24 | 32) => SampleFormat::Int { bits, valid_bits },
            (WAVE_FORMAT_IEEE_FLOAT, 32) => SampleFormat::Float32,
            (WAVE_FORMAT_PCM, b) => return err(format!("{b}-bit integer PCM is not supported")),
            (WAVE_FORMAT_IEEE_FLOAT, b) => return err(format!("{b}-bit float is not supported")),
            (t, _) => return err(format!("format tag {t:#06x} is not PCM or IEEE float")),
        };
        if usize::from(block_align) != usize::from(channels) * usize::from(bits / 8) {
            return err(format!(
                "block align {block_align} does not match {channels} x {bits}-bit"
            ));
        }
        Ok((rate, channels, format, block_align))
    }

    pub fn bytes_per_sample(&self) -> usize {
        match self.format {
            SampleFormat::Int { bits, .. } => usize::from(bits / 8),
            SampleFormat::Float32 => 4,
        }
    }

    /// Frames (samples per channel).
    pub fn frames(&self) -> usize {
        self.data.len() / self.bytes_per_sample() / usize::from(self.channels)
    }

    pub fn describe(&self) -> String {
        let f = match self.format {
            SampleFormat::Int { bits, valid_bits } if valid_bits != bits => {
                format!("{bits}-bit int ({valid_bits} valid)")
            }
            SampleFormat::Int { bits, .. } => format!("{bits}-bit int"),
            SampleFormat::Float32 => "32-bit float".into(),
        };
        format!(
            "{} Hz, {}, {}, {:.2} s",
            self.sample_rate,
            if self.channels == 1 { "mono" } else { "stereo" },
            f,
            self.frames() as f64 / f64::from(self.sample_rate)
        )
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Build a WAV file. `samples` are the raw little-endian sample bytes.
    pub fn build(
        tag: u16,
        channels: u16,
        rate: u32,
        bits: u16,
        extensible: Option<(u16, u16)>, // (valid bits, sub-format tag)
        samples: &[u8],
    ) -> Vec<u8> {
        let block = channels * bits / 8;
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&(if extensible.is_some() { 0xFFFE } else { tag }).to_le_bytes());
        fmt.extend_from_slice(&channels.to_le_bytes());
        fmt.extend_from_slice(&rate.to_le_bytes());
        fmt.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
        fmt.extend_from_slice(&block.to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());
        if let Some((valid, sub)) = extensible {
            fmt.extend_from_slice(&22u16.to_le_bytes());
            fmt.extend_from_slice(&valid.to_le_bytes());
            fmt.extend_from_slice(&3u32.to_le_bytes()); // channel mask FL|FR
            fmt.extend_from_slice(&sub.to_le_bytes());
            fmt.extend_from_slice(&SUBTYPE_TAIL);
        }
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        v.extend_from_slice(&fmt);
        // An odd-sized chunk we must skip (with its pad byte).
        v.extend_from_slice(b"LIST");
        v.extend_from_slice(&3u32.to_le_bytes());
        v.extend_from_slice(&[1, 2, 3, 0]);
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        v.extend_from_slice(samples);
        let riff = (v.len() - 8) as u32;
        v[4..8].copy_from_slice(&riff.to_le_bytes());
        v
    }

    #[test]
    fn parses_formats() {
        let w = Wav::parse(&build(1, 2, 48000, 16, None, &[0; 8])).unwrap();
        assert_eq!(
            (w.sample_rate, w.channels, w.format, w.frames()),
            (
                48000,
                2,
                SampleFormat::Int {
                    bits: 16,
                    valid_bits: 16
                },
                2
            )
        );
        let w = Wav::parse(&build(1, 1, 96000, 24, None, &[0; 9])).unwrap();
        assert_eq!((w.channels, w.frames()), (1, 3));
        let w = Wav::parse(&build(3, 2, 44100, 32, None, &[0; 16])).unwrap();
        assert_eq!(w.format, SampleFormat::Float32);
        let w = Wav::parse(&build(0, 2, 48000, 32, Some((24, 1)), &[0; 16])).unwrap();
        assert_eq!(
            w.format,
            SampleFormat::Int {
                bits: 32,
                valid_bits: 24
            }
        );
        assert!(
            w.describe().contains("32-bit int (24 valid)"),
            "{}",
            w.describe()
        );
        let w = Wav::parse(&build(0, 2, 48000, 32, Some((32, 3)), &[0; 16])).unwrap();
        assert_eq!(w.format, SampleFormat::Float32);
        // A partial trailing frame is dropped.
        let w = Wav::parse(&build(1, 2, 48000, 24, None, &[0; 13])).unwrap();
        assert_eq!(w.frames(), 2);
    }

    #[test]
    fn rejects_unsupported() {
        let e = |b: Vec<u8>| Wav::parse(&b).unwrap_err().0;
        assert!(e(b"RIFX0000WAVE".to_vec()).contains("RIFF/WAVE"));
        assert!(e(build(1, 6, 48000, 16, None, &[0; 24])).contains("6 channels"));
        assert!(e(build(1, 2, 48000, 8, None, &[0; 4])).contains("8-bit"));
        assert!(e(build(3, 2, 48000, 64, None, &[0; 32])).contains("64-bit float"));
        assert!(e(build(2, 2, 48000, 16, None, &[0; 4])).contains("0x0002"));
        let mut b = build(1, 2, 48000, 16, None, &[0; 4]);
        let i = b.windows(4).position(|w| w == b"data").unwrap();
        b[i..i + 4].copy_from_slice(b"junk");
        assert!(e(b).contains("no data"));
    }
}
