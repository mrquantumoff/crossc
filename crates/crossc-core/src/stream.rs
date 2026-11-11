//! The audio pipeline of a running stream: endpoint PCM chunks in, media
//! packets out. Converts each chunk to the encoder's input format (resampling
//! first if the plan says so), assembles 864-sample frames, encodes them and
//! wraps them in RTP. A packet goes out as soon as its frames are complete,
//! so the endpoint's own clock paces the stream.

use crossc_ioctl::audio::PcmFormat;
use ssc_enc::{Encoder, FRAME_SAMPLES, MAX_FRAME_BYTES};

use crate::channel::ChanError;
use crate::pcm::{self, Dither, FrameBuf};
use crate::plan::Plan;
use crate::resample::StreamResampler;
use crate::rtp::{Packetizer, TsMode};
use crate::volume::Gain;

/// Interleaved stereo samples per SSC frame (mono input is duplicated).
const FRAME_LEN: usize = FRAME_SAMPLES * 2;

#[derive(Debug)]
pub enum PipeError {
    Encoder(String),
    /// Sending a packet failed.
    Send(ChanError),
}

impl std::fmt::Display for PipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipeError::Encoder(e) => write!(f, "encoder: {e}"),
            PipeError::Send(e) => write!(f, "media write failed: {e}"),
        }
    }
}

impl std::error::Error for PipeError {}

pub struct Pipeline {
    fmt: PcmFormat,
    plan: Plan,
    dither: Dither,
    resampler: Option<StreamResampler>,
    rs_in: Vec<f64>,
    rs_out: Vec<f64>,
    /// Converted samples not yet encoded (less than one frame between calls).
    acc: FrameBuf,
    enc: Box<Encoder>,
    frame: Vec<u8>,
    /// Encoded frames of the packet being filled.
    payload: Vec<u8>,
    payload_frames: usize,
    pk: Packetizer,
    pkt: Vec<u8>,
    frames: u64,
    packets: u64,
    /// Software volume (unity, and then bypassed, unless the session sets one).
    gain: Gain,
    gained: Vec<u8>,
}

fn resampler_for(fmt: PcmFormat, plan: &Plan) -> Option<StreamResampler> {
    plan.resample_to
        .map(|to| StreamResampler::new(2, fmt.sample_rate, to))
}

impl Pipeline {
    /// A pipeline for endpoint format `fmt` streamed as `plan`. RTP sequence
    /// and timestamp start at 0 (the timestamp advances by 1 per packet, as
    /// Samsung's stack does).
    pub fn new(fmt: PcmFormat, plan: &Plan) -> Result<Self, String> {
        let enc = Encoder::new_boxed(plan.encoder, 2).map_err(|e| format!("encoder: {e}"))?;
        Ok(Pipeline {
            fmt,
            plan: *plan,
            dither: Dither::default(),
            resampler: resampler_for(fmt, plan),
            rs_in: Vec::new(),
            rs_out: Vec::new(),
            acc: plan.input.new_buf(),
            enc,
            frame: vec![0; MAX_FRAME_BYTES],
            payload: Vec::new(),
            payload_frames: 0,
            pk: Packetizer::new(TsMode::Packets, plan.frames_per_packet),
            pkt: Vec::new(),
            frames: 0,
            packets: 0,
            gain: Gain::unity(),
            gained: Vec::new(),
        })
    }

    /// Per-channel linear gain from now on, ramped (see [`crate::volume`])
    /// or `immediate`. Unity with no ramp running leaves the PCM untouched.
    pub fn set_gain(&mut self, gain: [f64; 2], immediate: bool) {
        self.gain.set(gain, self.fmt.sample_rate, immediate);
    }

    pub fn gain(&self) -> &Gain {
        &self.gain
    }

    /// The endpoint format changed but the stream configuration did not
    /// (`plan.same_stream`): keep the encoder and the RTP sequence, convert
    /// the input differently from now on.
    pub fn retarget(&mut self, fmt: PcmFormat, plan: &Plan) {
        self.fmt = fmt;
        self.plan = *plan;
        self.resampler = resampler_for(fmt, plan);
        self.acc = plan.input.new_buf();
        self.payload.clear();
        self.payload_frames = 0;
    }

    pub fn format(&self) -> PcmFormat {
        self.fmt
    }
    /// SSC frames encoded so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }
    /// Packets handed to `send` so far.
    pub fn packets(&self) -> u64 {
        self.packets
    }
    /// Converted samples waiting for the rest of their frame (per channel).
    pub fn pending(&self) -> usize {
        self.acc.len() / 2
    }

    /// The audio before and after this point is not contiguous: drop the
    /// partial frame, the partial packet and the resampler's history.
    pub fn flush(&mut self) {
        self.acc.clear();
        self.payload.clear();
        self.payload_frames = 0;
        if let Some(r) = &mut self.resampler {
            r.reset();
        }
    }

    /// Feed one chunk of endpoint PCM (`data` in the pipeline's format) and
    /// pass every packet that becomes complete to `send`. Returns how many
    /// packets were sent.
    pub fn push(
        &mut self,
        data: &[u8],
        send: &mut dyn FnMut(&[u8]) -> Result<(), ChanError>,
    ) -> Result<usize, PipeError> {
        // Software volume first, at 32-bit precision; skipped entirely at
        // unity so that path stays bit-exact.
        let (data, fmt) = if self.gain.is_unity() {
            (data, self.fmt)
        } else {
            let f = self.gain.apply(data, self.fmt, &mut self.gained);
            (&self.gained[..], f)
        };
        match &mut self.resampler {
            Some(r) => {
                self.rs_in.clear();
                pcm::append_chunk_f64(data, fmt, &mut self.rs_in);
                self.rs_out.clear();
                r.process(&self.rs_in, &mut self.rs_out);
                pcm::append_f64(&self.rs_out, &mut self.dither, &mut self.acc);
            }
            None => pcm::append_chunk(data, fmt, &mut self.dither, &mut self.acc),
        }
        let mut sent = 0;
        while let Some(frame) = self.acc.head(FRAME_LEN) {
            let n = self
                .enc
                .encode_frame(frame, self.plan.bitrate, &mut self.frame)
                .map_err(|e| PipeError::Encoder(e.to_string()))?;
            self.acc.discard(FRAME_LEN);
            self.frames += 1;
            self.payload.extend_from_slice(&self.frame[..n]);
            self.payload_frames += 1;
            if self.payload_frames >= self.plan.frames_per_packet {
                self.pk.build(&self.payload, &mut self.pkt);
                self.payload.clear();
                self.payload_frames = 0;
                send(&self.pkt).map_err(PipeError::Send)?;
                self.packets += 1;
                sent += 1;
            }
        }
        Ok(sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::select_plan;
    use crate::resample::stream_frames_out;
    use crate::settings::Settings;
    use avdtp::codec::SscFlags;
    use avdtp::rtp::RtpHeader;
    use ssc_enc::Bitrate;

    fn fmt(rate: u32, container: u8, valid: u8, channels: u8) -> PcmFormat {
        PcmFormat {
            sample_rate: rate,
            channels,
            container_bits: container,
            valid_bits: valid,
        }
    }

    /// `frames` frames of a 1 kHz tone at -12 dBFS in `f`.
    fn tone(f: PcmFormat, start: usize, frames: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for i in start..start + frames {
            let v =
                0.25 * (std::f64::consts::TAU * 1000.0 * i as f64 / f64::from(f.sample_rate)).sin();
            for _ in 0..f.channels {
                if f.container_bits == 16 {
                    out.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes());
                } else {
                    out.extend_from_slice(&(((v * 8_388_607.0) as i32) << 8).to_le_bytes());
                }
            }
        }
        out
    }

    /// Push `total` frames in `block`-frame chunks; collect the packets.
    fn run(f: PcmFormat, caps: u8, total: usize, block: usize) -> (Pipeline, Vec<Vec<u8>>) {
        let plan = select_plan(f, SscFlags(caps), &Settings::default()).unwrap();
        let mut p = Pipeline::new(f, &plan).unwrap();
        let mut packets = Vec::new();
        let mut pos = 0;
        while pos < total {
            let n = block.min(total - pos);
            p.push(&tone(f, pos, n), &mut |pkt| {
                packets.push(pkt.to_vec());
                Ok(())
            })
            .unwrap();
            pos += n;
        }
        (p, packets)
    }

    fn check_packets(packets: &[Vec<u8>], frame_bytes: usize, code: u8) {
        for (k, p) in packets.iter().enumerate() {
            assert_eq!(p.len(), 12 + frame_bytes, "packet {k}");
            let (h, off) = RtpHeader::decode(p).unwrap();
            assert_eq!(off, 12);
            assert_eq!(
                (h.sequence as usize, h.timestamp as usize, h.payload_type),
                (k, k, 96)
            );
            assert_eq!(p[12], 0xFF, "SSC sync byte right after the RTP header");
            // E-family rates keep side information in the top 3 bits.
            let mask = if code & 0xF0 == 0xE0 { 0x1F } else { 0xFF };
            assert_eq!(p[15] & mask, code & mask, "rate code");
        }
    }

    #[test]
    fn hifi_48k_one_packet_per_864_frames() {
        let f = fmt(48000, 32, 24, 2);
        // 10 ms chunks, 0.2 s: 9600 frames = 11 SSC frames + 96 left over.
        let (p, packets) = run(f, 0x3E, 9600, 480);
        assert_eq!(packets.len(), 11);
        assert_eq!((p.frames(), p.packets(), p.pending()), (11, 11, 96));
        check_packets(&packets, 484, Bitrate::K192.code());
    }

    #[test]
    fn uhq_96k_packets_are_672_bytes() {
        let f = fmt(96000, 32, 24, 2);
        let (p, packets) = run(f, 0x3E, 9600, 960);
        assert_eq!(packets.len(), 11);
        assert_eq!(p.pending(), 9600 - 11 * 864);
        check_packets(&packets, 660, Bitrate::K584.code());
        assert_eq!(packets[0].len(), 672);
    }

    #[test]
    fn chunking_does_not_change_the_stream() {
        let f = fmt(48000, 32, 24, 2);
        let (_, a) = run(f, 0x3E, 5000, 480);
        let (_, b) = run(f, 0x3E, 5000, 37);
        let (_, c) = run(f, 0x3E, 5000, 5000);
        assert_eq!(a.len(), 5);
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn sixteen_bit_and_mono_inputs() {
        // 16-bit into HiFi (widened), mono duplicated: same packet count,
        // and mono content equals the same signal sent as stereo.
        let (_, st16) = run(fmt(48000, 16, 16, 2), 0x3E, 4000, 480);
        let (_, mono16) = run(fmt(48000, 16, 16, 1), 0x3E, 4000, 480);
        assert_eq!(st16.len(), 4);
        assert_eq!(st16, mono16);
        check_packets(&st16, 484, Bitrate::K192.code());
        // Plain SSC from 24-bit input (dithered down) and from 16-bit input.
        let (_, a) = run(fmt(48000, 32, 24, 2), 0x30, 4000, 480);
        let (_, b) = run(fmt(48000, 16, 16, 2), 0x30, 4000, 480);
        assert_eq!((a.len(), b.len()), (4, 4));
        check_packets(&a, 576, Bitrate::K229.code());
        check_packets(&b, 576, Bitrate::K229.code());
    }

    #[test]
    fn resampling_paths_give_the_expected_frame_counts() {
        // (endpoint rate, caps, mode rate): frames out of the streaming
        // resampler, then whole SSC frames of those.
        for (rate, caps, to, frame_bytes) in [
            (96000u32, 0x3Cu8, 48000u32, 484usize), // no UHQ: 96k -> HiFi 48k
            (96000, 0x30, 48000, 576),              // 96k -> SSC 48k
            (48000, 0x20, 44100, 576),              // 48k -> SSC 44.1k
            (96000, 0x20, 44100, 576),              // 96k -> SSC 44.1k
        ] {
            let f = fmt(rate, 32, 24, 2);
            let total = rate as usize / 5; // 0.2 s
            let (p, packets) = run(f, caps, total, rate as usize / 100);
            let out = stream_frames_out(total as u64, rate, to) as usize;
            assert_eq!(packets.len(), out / 864, "{rate} -> {to}");
            assert_eq!(p.pending(), out % 864, "{rate} -> {to}");
            assert_eq!(packets[0].len(), 12 + frame_bytes);
        }
        // The numbers themselves, for 0.2 s: 9536 and 8756 frames.
        assert_eq!(stream_frames_out(19200, 96000, 48000), 9536);
        assert_eq!(stream_frames_out(9600, 48000, 44100), 8756);
    }

    #[test]
    fn flush_drops_the_partial_frame_only() {
        let f = fmt(48000, 32, 24, 2);
        let plan = select_plan(f, SscFlags(0x3E), &Settings::default()).unwrap();
        let mut p = Pipeline::new(f, &plan).unwrap();
        let mut n = 0;
        let mut send = |_: &[u8]| {
            n += 1;
            Ok(())
        };
        p.push(&tone(f, 0, 1000), &mut send).unwrap();
        assert_eq!((p.packets(), p.pending()), (1, 136));
        p.flush();
        assert_eq!(p.pending(), 0);
        // The next frame needs a full 864 again; the RTP sequence goes on.
        p.push(&tone(f, 2000, 863), &mut send).unwrap();
        assert_eq!(p.packets(), 1);
        p.push(&tone(f, 2863, 1), &mut send).unwrap();
        assert_eq!((p.packets(), p.pending()), (2, 0));
        assert_eq!(n, 2);
    }

    #[test]
    fn send_errors_surface() {
        let f = fmt(48000, 32, 24, 2);
        let plan = select_plan(f, SscFlags(0x3E), &Settings::default()).unwrap();
        let mut p = Pipeline::new(f, &plan).unwrap();
        let r = p.push(&tone(f, 0, 864), &mut |_| Err(ChanError::Disconnected));
        assert!(matches!(r, Err(PipeError::Send(ChanError::Disconnected))));
        assert_eq!(p.packets(), 0);
    }

    #[test]
    fn retarget_keeps_the_rtp_sequence() {
        let f24 = fmt(48000, 32, 24, 2);
        let f16 = fmt(48000, 16, 16, 2);
        let s = Settings::default();
        let plan = select_plan(f24, SscFlags(0x3E), &s).unwrap();
        let mut p = Pipeline::new(f24, &plan).unwrap();
        let mut seqs = Vec::new();
        let mut send = |pkt: &[u8]| {
            seqs.push(RtpHeader::decode(pkt).unwrap().0.sequence);
            Ok(())
        };
        p.push(&tone(f24, 0, 900), &mut send).unwrap();
        let plan16 = select_plan(f16, SscFlags(0x3E), &s).unwrap();
        assert!(plan.same_stream(&plan16));
        p.retarget(f16, &plan16);
        assert_eq!((p.format(), p.pending()), (f16, 0));
        p.push(&tone(f16, 0, 864), &mut send).unwrap();
        assert_eq!(seqs, vec![0, 1]);
    }
}
