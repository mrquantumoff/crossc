//! Finding the sink's Samsung SSC stream end point, and what our own source
//! SEP looks like to the sink.

use avdtp::caps::{self, Capability};
use avdtp::codec::{CodecInfo, SscFlags};
use avdtp::msg::{MediaType, SepType};

use crate::signaling::SepReport;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SscSep {
    pub seid: u8,
    pub flags: SscFlags,
    pub delay_reporting: bool,
    /// DISCOVER said "in use". We try anyway (the sink answers SEP_IN_USE if
    /// it really is), since whether the buds set this bit while idle is unknown.
    pub in_use: bool,
}

/// The audio sink SEP whose codec is Samsung SSC (a free one preferred).
pub fn choose_ssc(reports: &[SepReport]) -> Result<SscSep, String> {
    let mut found: Vec<SscSep> = Vec::new();
    for r in reports {
        let i = &r.info;
        if i.media_type != MediaType::Audio || i.tsep != SepType::Sink {
            continue;
        }
        let Ok(caps) = &r.caps else { continue };
        let ssc = caps.iter().find_map(|c| match c {
            Capability::MediaCodec {
                codec: CodecInfo::Vendor(v),
                ..
            } => v.ssc(),
            _ => None,
        });
        if let Some(ssc) = ssc {
            found.push(SscSep {
                seid: i.seid,
                flags: ssc.flags,
                delay_reporting: caps.contains(&Capability::DelayReporting),
                in_use: i.in_use,
            });
        }
    }
    let pos = found.iter().position(|s| !s.in_use).unwrap_or(0);
    if found.is_empty() {
        return Err("the sink has no SSC (vendor 0x75 / codec 0x0103) sink SEP".into());
    }
    Ok(found.swap_remove(pos))
}

/// Our source SEP's capabilities, as the sink would see them if it asked.
pub fn local_caps() -> Vec<Capability> {
    caps::ssc_configuration(
        SscFlags::SR_44K1
            | SscFlags::SR_48K
            | SscFlags::NO_BITRATE_LIMIT_24BIT
            | SscFlags::HIFI_48K_24
            | SscFlags::UHQ_V2_96K_24,
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeSink;
    use crate::signaling::{Signaling, probe};

    #[test]
    fn ssc_sep_choice() {
        let mut fake = FakeSink::new(672);
        fake.ssc_flags = 0x3E;
        let mut sig = Signaling::new(fake);
        let mut reports = probe(&mut sig).unwrap();
        let s = choose_ssc(&reports).unwrap();
        assert_eq!(
            (s.seid, s.flags, s.delay_reporting, s.in_use),
            (3, SscFlags(0x3E), true, true)
        );
        // A free SSC SEP is preferred over an in-use one.
        let mut free = reports[2].clone();
        free.info.seid = 5;
        free.info.in_use = false;
        reports.push(free);
        assert_eq!(choose_ssc(&reports).unwrap().seid, 5);
        assert!(choose_ssc(&reports[..2]).unwrap_err().contains("no SSC"));
    }

    #[test]
    fn local_caps_offer_every_mode_we_encode() {
        let c = local_caps();
        assert_eq!(c.len(), 3);
        assert!(c.contains(&Capability::DelayReporting));
    }
}
