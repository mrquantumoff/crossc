//! Media packets: 12-byte RTP header (V=2, PT=96, fixed SSRC) + SSC frames
//! back to back (no payload header octet, no fragmentation), and the rule for
//! how large such a packet may be on a media channel.

use std::time::Duration;

use avdtp::rtp::RtpHeader;
use ssc_enc::FRAME_SAMPLES;

/// How the RTP timestamp advances.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TsMode {
    /// +1 per packet (Samsung).
    Packets,
    /// +samples per packet (RFC 3550 style).
    Samples,
}

pub const RTP_PT: u8 = 96;
pub const RTP_SSRC: u32 = 0x0000_0001;

/// Media MTU the phone used with Buds4 Pro (air capture).
pub const KNOWN_BUDS_MEDIA_MTU: usize = 1005;

pub struct Packetizer {
    hdr: RtpHeader,
    step: u32,
}

impl Packetizer {
    /// Sequence and timestamp start at 0.
    pub fn new(ts: TsMode, fpp: usize) -> Self {
        Self {
            hdr: RtpHeader::new(RTP_PT, 0, 0, RTP_SSRC),
            step: match ts {
                TsMode::Packets => 1,
                TsMode::Samples => (fpp * FRAME_SAMPLES) as u32,
            },
        }
    }
    /// Header + `payload` (frames back to back) into `out`; then advance.
    pub fn build(&mut self, payload: &[u8], out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&self.hdr.encode());
        out.extend_from_slice(payload);
        self.hdr.advance(self.step);
    }
}

/// Deadline of packet `k` from the stream start: exact integer arithmetic,
/// so the schedule never accumulates rounding error.
pub fn packet_deadline(k: u64, fpp: usize, rate: u32) -> Duration {
    let ns = u128::from(k) * fpp as u128 * FRAME_SAMPLES as u128 * 1_000_000_000 / u128::from(rate);
    Duration::from_nanos(ns as u64)
}

const fn avdtp_min_mtu() -> u16 {
    48
}

/// Largest media packet we allow on a channel the driver reported with
/// (`out_mtu`, `in_mtu`) after we asked for `requested_in_mtu`, and why.
/// `None`: neither value is plausible.
///
/// Which driver field carries the peer's MTU is unverified (SETUP 7a). So:
/// * 0 and 65535 are treated as "not reported" (65535 is what an unset
///   L2CAP MTU field defaults to, and is not plausible from the buds);
/// * the limit is the smallest remaining value, i.e. we assume either field
///   could be the peer's receive MTU;
/// * if a value equals our own requested in_mtu it may just be our request
///   echoed back, so the limit is additionally capped at 1005 (the media MTU
///   the phone used with Buds4 Pro).
pub fn media_mtu_limit(
    out_mtu: u16,
    in_mtu: u16,
    requested_in_mtu: u16,
) -> Option<(usize, String)> {
    let plausible: Vec<u16> = [out_mtu, in_mtu]
        .into_iter()
        .filter(|&v| v >= avdtp_min_mtu() && v != 0xFFFF)
        .collect();
    let &min = plausible.iter().min()?;
    let mut limit = usize::from(min);
    let mut why =
        format!("min of plausible reported MTUs (out_mtu={out_mtu} in_mtu={in_mtu}) = {min}");
    if plausible.contains(&requested_in_mtu) && limit > KNOWN_BUDS_MEDIA_MTU {
        limit = KNOWN_BUDS_MEDIA_MTU;
        why.push_str(&format!(
            "; {requested_in_mtu} is our own requested in_mtu (may be echoed), so capped at {KNOWN_BUDS_MEDIA_MTU} (phone's Buds4 Pro media MTU)"
        ));
    }
    Some((limit, why))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packetizer_ts_modes() {
        for (ts, fpp, step) in [
            (TsMode::Packets, 1, 1u32),
            (TsMode::Packets, 2, 1),
            (TsMode::Samples, 1, 864),
            (TsMode::Samples, 2, 1728),
        ] {
            let mut p = Packetizer::new(ts, fpp);
            let payload = vec![0xAB; fpp * 10];
            let mut out = Vec::new();
            for k in 0..3u32 {
                p.build(&payload, &mut out);
                assert_eq!(out.len(), 12 + payload.len());
                let (h, off) = RtpHeader::decode(&out).unwrap();
                assert_eq!(off, 12);
                assert_eq!((out[0], out[1]), (0x80, 96), "V=2, no P/X/CC, M=0, PT=96");
                assert_eq!(h.sequence, k as u16);
                assert_eq!(h.timestamp, k * step);
                assert_eq!(h.ssrc, RTP_SSRC);
                assert_eq!(&out[12..], &payload[..], "frames directly after the header");
            }
        }
    }

    #[test]
    fn deadlines_do_not_drift() {
        // 864/48000 = 18 ms exactly; 864/44100 is not an integer number of ns.
        assert_eq!(packet_deadline(1, 1, 48000), Duration::from_millis(18));
        assert_eq!(packet_deadline(1000, 2, 48000), Duration::from_secs(36));
        assert_eq!(packet_deadline(1, 1, 96000), Duration::from_millis(9));
        // 44.1k: 100k packets = 1959.183673469... s, exact to the ns.
        assert_eq!(
            packet_deadline(100_000, 1, 44100),
            Duration::from_nanos(1_959_183_673_469)
        );
    }

    #[test]
    fn mtu_limit_rule() {
        // Live values: media out=1021 in=65535 after requesting 1021.
        let (l, why) = media_mtu_limit(1021, 65535, 1021).unwrap();
        assert_eq!(l, 1005, "{why}");
        assert!(why.contains("capped at 1005"), "{why}");
        assert_eq!(media_mtu_limit(1005, 65535, 900).unwrap().0, 1005);
        assert_eq!(media_mtu_limit(1021, 900, 900).unwrap().0, 900);
        assert_eq!(media_mtu_limit(679, 65535, 1021).unwrap().0, 679);
        assert!(media_mtu_limit(0, 65535, 1021).is_none());
        assert!(media_mtu_limit(47, 0, 1021).is_none());
    }
}
