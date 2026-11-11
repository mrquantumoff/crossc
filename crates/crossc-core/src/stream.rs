//! The audio pipeline of a running stream: endpoint PCM chunks in, media
//! packets out. Converts each chunk to the encoder's input format (resampling
//! first if the plan says so), assembles 864-sample frames, encodes them and
//! wraps them in RTP. A packet goes out as soon as its frames are complete,
//! so the endpoint's own clock paces the stream.
//!
//! The bitrate and the frames per packet are the plan's until
//! [`Pipeline::set_rate`] says otherwise (adaptive bitrate, [`crate::abr`]):
//! the SSC frame header carries the rate, so it can change between any two
//! packets while the encoder, the RTP sequence and the AVDTP configuration
//! stay as they are.

use crossc_ioctl::audio::PcmFormat;
use ssc_enc::{Encoder, FRAME_SAMPLES, MAX_FRAME_BYTES};

use crate::abr::Rate;
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
    /// What the packet being filled is built with.
    rate: Rate,
    /// What the next packet is built with, if not `rate`.
    next_rate: Option<Rate>,
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
            // Room for the largest packet of any rate, so that a rate
            // change never allocates on the packet path.
            payload: Vec::with_capacity(PAYLOAD_CAPACITY),
            payload_frames: 0,
            rate: Rate {
                bitrate: plan.bitrate,
                frames_per_packet: plan.frames_per_packet,
            },
            next_rate: None,
            pk: Packetizer::new(TsMode::Packets, plan.frames_per_packet),
            pkt: Vec::with_capacity(PAYLOAD_CAPACITY + 12),
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

    /// The bitrate and frames per packet from the next packet on. A packet
    /// that is partly filled is finished at the old rate first, so every
    /// packet holds frames of one rate.
    pub fn set_rate(&mut self, rate: Rate) {
        let rate = Rate {
            frames_per_packet: rate.frames_per_packet.max(1),
            ..rate
        };
        if self.payload_frames == 0 {
            self.rate = rate;
            self.next_rate = None;
        } else {
            self.next_rate = (rate != self.rate).then_some(rate);
        }
    }

    /// What packets are being built with (a rate set while a packet was
    /// partly filled shows once that packet is complete).
    pub fn rate(&self) -> Rate {
        self.rate
    }

    /// The endpoint format changed but the stream configuration did not
    /// (`plan.same_stream`): keep the encoder, the RTP sequence and the
    /// current rate, convert the input differently from now on.
    pub fn retarget(&mut self, fmt: PcmFormat, plan: &Plan) {
        self.fmt = fmt;
        self.plan = *plan;
        self.resampler = resampler_for(fmt, plan);
        self.acc = plan.input.new_buf();
        self.drop_partial_packet();
    }

    fn drop_partial_packet(&mut self) {
        self.payload.clear();
        self.payload_frames = 0;
        if let Some(r) = self.next_rate.take() {
            self.rate = r;
        }
    }

    pub fn format(&self) -> PcmFormat {
        self.fmt
    }
    /// SSC frames encoded so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }
    /// Packets sent ([`Self::push`]) or handed out ([`Self::next_packet`])
    /// so far.
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
        self.drop_partial_packet();
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
        self.feed(data);
        let mut sent = 0;
        while self.build_packet()? {
            send(&self.pkt).map_err(PipeError::Send)?;
            self.packets += 1;
            sent += 1;
        }
        Ok(sent)
    }

    /// The next complete packet of what was fed, or `None` when less than
    /// a packet is left. Between two calls the caller may [`Self::set_rate`]
    /// (which is how the rate follows each packet's fate).
    pub fn next_packet(&mut self) -> Result<Option<&[u8]>, PipeError> {
        if !self.build_packet()? {
            return Ok(None);
        }
        self.packets += 1;
        Ok(Some(&self.pkt))
    }

    /// Take in one chunk of endpoint PCM (`data` in the pipeline's format):
    /// volume, conversion to the encoder's input format, resampling.
    pub fn feed(&mut self, data: &[u8]) {
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
    }

    /// Encode frames until a packet is complete (then in `self.pkt`) or the
    /// accumulated PCM runs out.
    fn build_packet(&mut self) -> Result<bool, PipeError> {
        while let Some(frame) = self.acc.head(FRAME_LEN) {
            let n = self
                .enc
                .encode_frame(frame, self.rate.bitrate, &mut self.frame)
                .map_err(|e| PipeError::Encoder(e.to_string()))?;
            self.acc.discard(FRAME_LEN);
            self.frames += 1;
            self.payload.extend_from_slice(&self.frame[..n]);
            self.payload_frames += 1;
            if self.payload_frames >= self.rate.frames_per_packet {
                self.pk.build(&self.payload, &mut self.pkt);
                self.drop_partial_packet();
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Bytes the payload buffer starts with: two frames of the largest rate the
/// buds decode (584 kbps UHQ, 660 bytes), rounded up.
const PAYLOAD_CAPACITY: usize = 2048;

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
        check_packets(&packets, 576, Bitrate::K229.code());
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
        check_packets(&st16, 576, Bitrate::K229.code());
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
            (96000u32, 0x3Cu8, 48000u32, 576usize), // no UHQ: 96k -> HiFi 48k
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

    /// (sequence, timestamp, payload length, rate code of every frame).
    fn dissect(pkt: &[u8], frame_bytes: usize) -> (u16, u32, usize, Vec<u8>) {
        let (h, off) = RtpHeader::decode(pkt).unwrap();
        let payload = &pkt[off..];
        assert_eq!(payload.len() % frame_bytes, 0, "whole frames");
        let codes = payload
            .chunks(frame_bytes)
            .map(|f| {
                assert_eq!(f[0], 0xFF, "SSC sync byte at every frame start");
                f[3]
            })
            .collect();
        (h.sequence, h.timestamp, payload.len(), codes)
    }

    #[test]
    fn every_rung_of_every_ladder_gives_the_right_packets() {
        use crate::abr::Ladder;
        for (f, caps) in [
            (fmt(48000, 32, 24, 2), 0x3Eu8), // HiFi
            (fmt(96000, 32, 24, 2), 0x3E),   // UHQ
            (fmt(48000, 32, 24, 2), 0x30),   // SSC 48 kHz
            (fmt(48000, 32, 24, 2), 0x20),   // SSC 44.1 kHz (resampled)
        ] {
            let plan = select_plan(f, SscFlags(caps), &Settings::default()).unwrap();
            let top = Rate {
                bitrate: plan.bitrate,
                frames_per_packet: plan.frames_per_packet,
            };
            let ladder = Ladder::new(plan.encoder, top, 1005, true, true);
            assert!(ladder.is_adaptive(), "{}", plan.mode.name());
            let mut p = Pipeline::new(f, &plan).unwrap();
            let cap = (p.payload.capacity(), p.pkt.capacity());
            let mut pos = 0;
            let mut seq = 0u16;
            // Down the ladder and up again, eight packets at every rung.
            let down = ladder.rungs().iter();
            let up = ladder.rungs().iter().rev().skip(1);
            for rate in down.chain(up) {
                p.set_rate(*rate);
                assert_eq!(p.rate(), *rate);
                let fb = ssc_enc::frame_size(plan.encoder, rate.bitrate);
                let mut got = 0;
                while got < 8 {
                    p.feed(&tone(f, pos, 480));
                    pos += 480;
                    while let Some(pkt) = p.next_packet().unwrap() {
                        assert_eq!(pkt.len(), rate.packet_bytes(plan.encoder));
                        let (s, ts, len, codes) = dissect(pkt, fb);
                        assert_eq!((s, ts), (seq, u32::from(seq)), "RTP goes on");
                        assert_eq!(len, rate.frames_per_packet * fb);
                        let mask = if rate.bitrate.is_e_family() {
                            0x1F
                        } else {
                            0xFF
                        };
                        for c in codes {
                            assert_eq!(c & mask, rate.bitrate.code() & mask);
                        }
                        seq += 1;
                        got += 1;
                    }
                }
            }
            assert_eq!(
                (p.payload.capacity(), p.pkt.capacity()),
                cap,
                "no rate needed a larger buffer"
            );
            assert_eq!(p.packets(), u64::from(seq));
        }
    }

    #[test]
    fn a_rate_change_waits_for_the_packet_boundary() {
        // UHQ at 291 kbps with two frames per packet, then back to 584 / 1
        // while the first frame of a packet is already encoded.
        let f = fmt(96000, 32, 24, 2);
        let plan = select_plan(f, SscFlags(0x3E), &Settings::default()).unwrap();
        let mut p = Pipeline::new(f, &plan).unwrap();
        let two = Rate {
            bitrate: Bitrate::K291,
            frames_per_packet: 2,
        };
        let one = Rate {
            bitrate: Bitrate::K584,
            frames_per_packet: 1,
        };
        p.set_rate(two);
        p.feed(&tone(f, 0, 864));
        assert!(p.next_packet().unwrap().is_none(), "half a packet");
        p.set_rate(one);
        assert_eq!(p.rate(), two, "not before the packet is complete");
        p.feed(&tone(f, 864, 2 * 864));
        let fb291 = ssc_enc::frame_size(plan.encoder, Bitrate::K291);
        let a = p.next_packet().unwrap().unwrap().to_vec();
        assert_eq!(a.len(), 12 + 2 * fb291, "both frames at the old rate");
        assert_eq!(dissect(&a, fb291).3.len(), 2);
        assert_eq!(p.rate(), one);
        let b = p.next_packet().unwrap().unwrap().to_vec();
        assert_eq!(b.len(), 672);
        assert_eq!((dissect(&a, fb291).0, dissect(&b, 660).0), (0, 1));
        assert_eq!((p.frames(), p.packets()), (3, 2));
        // A flush drops the half packet and takes the pending rate.
        p.set_rate(two);
        p.feed(&tone(f, 0, 864));
        assert!(p.next_packet().unwrap().is_none());
        p.set_rate(one);
        p.flush();
        assert_eq!(p.rate(), one);
        p.feed(&tone(f, 0, 864));
        assert_eq!(p.next_packet().unwrap().unwrap().len(), 672);
        // Setting the rate that is already in use is nothing.
        p.set_rate(one);
        assert_eq!(p.rate(), one);
    }

    #[test]
    fn push_and_next_packet_give_the_same_stream() {
        let f = fmt(48000, 32, 24, 2);
        let (_, a) = run(f, 0x3E, 5000, 480);
        let plan = select_plan(f, SscFlags(0x3E), &Settings::default()).unwrap();
        let mut p = Pipeline::new(f, &plan).unwrap();
        let mut b = Vec::new();
        let mut pos = 0;
        while pos < 5000 {
            let n = 480.min(5000 - pos);
            p.feed(&tone(f, pos, n));
            while let Some(pkt) = p.next_packet().unwrap() {
                b.push(pkt.to_vec());
            }
            pos += n;
        }
        assert_eq!(a, b);
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
        // ... and the rate the stream is at.
        let low = Rate {
            bitrate: Bitrate::K88,
            frames_per_packet: 1,
        };
        p.set_rate(low);
        p.retarget(f24, &plan);
        assert_eq!(p.rate(), low);
    }
}
