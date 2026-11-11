//! `probe`: DISCOVER + GET_ALL_CAPABILITIES on every SEP
//! ([`crossc_core::signaling`]), pretty-printed here.

use std::fmt::Write as _;

use avdtp::caps::Capability;
use avdtp::codec::CodecInfo;
pub use crossc_core::signaling::{SepReport, Signaling, probe as run};

use crate::hex;

/// Pretty-print a probe result.
pub fn render(reports: &[SepReport]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{} stream end point(s)", reports.len());
    for r in reports {
        let i = &r.info;
        let _ = writeln!(
            s,
            "\nSEP {:#04x}: {:?} {:?}{}  [{}]",
            i.seid,
            i.media_type,
            i.tsep,
            if i.in_use { " (in use)" } else { "" },
            r.via
        );
        match &r.caps {
            Err(e) => {
                let _ = writeln!(s, "  error: {e}");
            }
            Ok(caps) => {
                for c in caps {
                    render_cap(&mut s, c);
                }
            }
        }
    }
    s
}

fn render_cap(s: &mut String, c: &Capability) {
    let name = Capability::category_name(c.category());
    match c {
        Capability::Recovery {
            recovery_type,
            mrws,
            mnmp,
        } => {
            let _ = writeln!(s, "  {name}: type={recovery_type} MRWS={mrws} MNMP={mnmp}");
        }
        Capability::ContentProtection { cp_type, value } => {
            let t = match *cp_type {
                1 => "DTCP",
                2 => "SCMS-T",
                _ => "?",
            };
            let _ = writeln!(s, "  {name}: {t} ({cp_type:#06x}) {}", hex::format(value));
        }
        Capability::HeaderCompression {
            back_channel,
            media,
            recovery,
        } => {
            let _ = writeln!(
                s,
                "  {name}: back_channel={back_channel} media={media} recovery={recovery}"
            );
        }
        Capability::Multiplexing(d) => {
            let _ = writeln!(s, "  {name}: {}", hex::format(d));
        }
        Capability::Unknown { category, data } => {
            let _ = writeln!(s, "  category {category:#04x}: {}", hex::format(data));
        }
        Capability::MediaCodec { media_type, codec } => {
            let _ = write!(s, "  {name} (media type {media_type}): ");
            render_codec(s, codec);
        }
        _ => {
            let _ = writeln!(s, "  {name}");
        }
    }
}

fn render_codec(s: &mut String, c: &CodecInfo) {
    match c {
        CodecInfo::Sbc(b) => {
            let rates: Vec<String> = b.sample_rates().map(|r| r.to_string()).collect();
            let _ = writeln!(
                s,
                "SBC rates={} ch_mode={:#x} blocks={:#x} subbands={:#x} alloc={:#x} bitpool={}..{}",
                rates.join(","),
                b.channel_mode,
                b.block_length,
                b.subbands,
                b.allocation_method,
                b.min_bitpool,
                b.max_bitpool
            );
        }
        CodecInfo::Aac(a) => {
            let rates: Vec<String> = a.sample_rates().map(|r| r.to_string()).collect();
            let _ = writeln!(
                s,
                "AAC object_type={:#04x} rates={} channels={:#x} vbr={} bitrate={}",
                a.object_type,
                rates.join(","),
                a.channels,
                a.vbr,
                a.bit_rate
            );
        }
        CodecInfo::Vendor(v) => {
            let _ = writeln!(
                s,
                "vendor {:#010x} codec {:#06x} value [{}]",
                v.vendor_id,
                v.codec_id,
                hex::format(&v.data)
            );
            if let Some(ssc) = v.ssc() {
                let f = ssc.flags;
                let _ = writeln!(s, "    Samsung SSC flags = {:#04x} ({:08b})", f.0, f.0);
                for (bit, n) in avdtp::codec::SscFlags::ALL {
                    let _ = writeln!(
                        s,
                        "      [{}] {:#04x} {}",
                        if f.has(bit) { 'x' } else { ' ' },
                        bit,
                        n
                    );
                }
                let _ = writeln!(
                    s,
                    "    -> 48k/24 HiFi: {}   96k/24 UHQv2 usable: {}",
                    f.hifi_48k_24(),
                    f.uhq_v2_usable()
                );
                if !ssc.extra.is_empty() {
                    let _ = writeln!(s, "    extra bytes: {}", hex::format(&ssc.extra));
                }
            }
        }
        CodecInfo::Other { codec_type, data } => {
            let _ = writeln!(s, "codec {codec_type:#04x} [{}]", hex::format(data));
        }
        CodecInfo::Malformed { codec_type, data } => {
            let _ = writeln!(
                s,
                "MALFORMED codec {codec_type:#04x} element [{}]",
                hex::format(data)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossc_core::fake::FakeSink;

    #[test]
    fn probe_against_fake_sink() {
        let mut sig = Signaling::new(FakeSink::new(672));
        let r = run(&mut sig).unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].info.seid, 1);
        assert_eq!(r[2].info.seid, 3);
        assert!(
            r.iter()
                .all(|x| x.caps.is_ok() && x.via == "GET_ALL_CAPABILITIES")
        );
        let text = render(&r);
        assert!(text.contains("SBC rates=44100"), "{text}");
        assert!(text.contains("AAC object_type=0x80"), "{text}");
        assert!(text.contains("Samsung SSC flags = 0x16"), "{text}");
        assert!(text.contains("[x] 0x02 UHQ v2 96 kHz/24-bit"), "{text}");
        assert!(text.contains("96k/24 UHQv2 usable: true"), "{text}");
        assert!(text.contains("SCMS-T"), "{text}");
        assert!(text.contains("Delay Reporting"), "{text}");
    }

    #[test]
    fn silent_after_first_sep_keeps_earlier_reports() {
        let mut fake = FakeSink::new(672);
        fake.silent_from_seid = Some(2);
        let mut sig = Signaling::new(fake);
        let r = run(&mut sig).unwrap();
        assert_eq!(r.len(), 3);
        assert!(
            r[0].caps.is_ok(),
            "SEP 1's report must survive a later timeout"
        );
        assert!(
            r[1].caps
                .as_ref()
                .unwrap_err()
                .contains("channel error: timed out"),
            "{:?}",
            r[1].caps
        );
        assert!(
            r[2].caps.as_ref().unwrap_err().contains("not probed"),
            "{:?}",
            r[2].caps
        );
        let text = render(&r);
        assert!(text.contains("SBC rates=44100"), "{text}");
    }
}
