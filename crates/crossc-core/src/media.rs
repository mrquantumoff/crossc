//! The AVDTP stream steps: SET_CONFIGURATION and OPEN (AVDTP 1.3: the INT
//! opens the media transport channel only after both were accepted for the
//! chosen SEP), START, SUSPEND and the teardown (CLOSE, ABORT). Pure logic
//! over [`SduChannel`]; the second L2CAP open itself is done by the caller
//! between [`configure_and_open`] and [`release`].
//!
//! [`choose`] / [`sbc_config`] build the deliberately minimal SBC
//! configuration the `media` experiment uses (SBC is mandatory for every A2DP
//! sink): exactly one bit per field, Media Transport + Media Codec only, no
//! Delay Reporting, no content protection.

use avdtp::caps::Capability;
use avdtp::codec::{CodecInfo, MEDIA_TYPE_AUDIO, Sbc};
use avdtp::msg::{self, MediaType, Reject, Response, SepType};
use avdtp::signal::SignalId;

use crate::channel::{ChanError, SduChannel};
use crate::signaling::{SepReport, Signaling};

/// Our (virtual) source SEP id, sent as INT SEID in SET_CONFIGURATION.
pub const INT_SEID: u8 = 0x01;

/// Highest bit set in `mask` among `prefs` (first match wins).
fn pick(mask: u8, prefs: &[u8]) -> Option<u8> {
    prefs.iter().copied().find(|b| mask & b != 0)
}

/// One concrete SBC configuration inside the sink's SBC capability, or None
/// if a field has no supported bit or the bitpool range is empty.
pub fn sbc_config(cap: &Sbc) -> Option<Sbc> {
    let min_bitpool = cap.min_bitpool.max(2);
    let max_bitpool = cap.max_bitpool.min(53);
    if min_bitpool > max_bitpool {
        return None;
    }
    Some(Sbc {
        sampling_frequency: pick(cap.sampling_frequency, &[0x10, 0x20, 0x40, 0x80])?, // 48k first
        channel_mode: pick(cap.channel_mode, &[0x01, 0x02, 0x04, 0x08])?, // joint stereo first
        block_length: pick(cap.block_length, &[0x10, 0x20, 0x40, 0x80])?, // 16 blocks first
        subbands: pick(cap.subbands, &[0x04, 0x08])?,                     // 8 subbands first
        allocation_method: pick(cap.allocation_method, &[0x01, 0x02])?,   // loudness first
        min_bitpool,
        max_bitpool,
    })
}

/// Chosen SEP and the configuration to send.
#[derive(Debug, PartialEq, Eq)]
pub struct Choice {
    pub acp_seid: u8,
    pub config: Vec<Capability>,
}

/// Pick a free audio sink SEP with SBC (or exactly `seid`, which must be an
/// audio sink with SBC) and build its configuration.
pub fn choose(reports: &[SepReport], seid: Option<u8>) -> Result<Choice, String> {
    for r in reports {
        let i = &r.info;
        if seid.is_some_and(|s| s != i.seid) {
            continue;
        }
        let usable = i.media_type == MediaType::Audio && i.tsep == SepType::Sink && !i.in_use;
        let sbc = r.caps.as_ref().ok().and_then(|caps| {
            caps.iter().find_map(|c| match c {
                Capability::MediaCodec {
                    codec: CodecInfo::Sbc(s),
                    ..
                } => Some(*s),
                _ => None,
            })
        });
        match (usable, sbc.and_then(|s| sbc_config(&s))) {
            (true, Some(cfg)) => {
                return Ok(Choice {
                    acp_seid: i.seid,
                    config: vec![
                        Capability::MediaTransport,
                        Capability::MediaCodec {
                            media_type: MEDIA_TYPE_AUDIO,
                            codec: CodecInfo::Sbc(cfg),
                        },
                    ],
                });
            }
            _ if seid.is_some() => {
                return Err(format!(
                    "SEP {:#04x} is not a free audio sink with a usable SBC capability",
                    i.seid
                ));
            }
            _ => {}
        }
    }
    Err(match seid {
        Some(s) => format!("SEP {s:#04x} not found"),
        None => "no free audio sink SEP with SBC found".into(),
    })
}

/// Failure of one AVDTP step.
#[derive(Debug)]
pub enum StepError {
    Chan(ChanError),
    /// The sink answered with a reject / general reject / malformed response.
    Refused(String),
}

impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepError::Chan(e) => write!(f, "{e}"),
            StepError::Refused(s) => f.write_str(s),
        }
    }
}
impl std::error::Error for StepError {}
impl From<ChanError> for StepError {
    fn from(e: ChanError) -> Self {
        StepError::Chan(e)
    }
}

pub fn expect_accept(
    what: &str,
    signal: SignalId,
    r: &avdtp::signal::Message,
) -> Result<(), StepError> {
    match msg::classify_response(r, r.label, signal) {
        Ok(Response::Accept(_)) => Ok(()),
        Ok(Response::Reject(Reject {
            error, category, ..
        })) => Err(StepError::Refused(format!(
            "{what} rejected: {} ({error:#04x}){}",
            msg::error_name(error),
            category
                .map(|c| format!(" in category {}", Capability::category_name(c)))
                .unwrap_or_default()
        ))),
        Ok(Response::GeneralReject) => Err(StepError::Refused(format!("{what}: general reject"))),
        Err(e) => Err(StepError::Refused(format!("{what}: bad response: {e}"))),
    }
}

/// SET_CONFIGURATION then OPEN. After Ok the INT may open the media
/// transport channel (second L2CAP connection on PSM 0x19).
pub fn configure_and_open<C: SduChannel>(
    sig: &mut Signaling<C>,
    choice: &Choice,
) -> Result<(), StepError> {
    set_configuration(sig, choice)?;
    open(sig, choice.acp_seid)
}

/// SET_CONFIGURATION alone (the SEP is CONFIGURED after Ok).
pub fn set_configuration<C: SduChannel>(
    sig: &mut Signaling<C>,
    choice: &Choice,
) -> Result<(), StepError> {
    let m = msg::set_configuration(0, choice.acp_seid, INT_SEID, &choice.config)
        .map_err(|e| StepError::Refused(format!("cannot encode configuration: {e}")))?;
    let r = sig.request(|l| avdtp::signal::Message { label: l, ..m })?;
    expect_accept("SET_CONFIGURATION", SignalId::SetConfiguration, &r)
}

/// OPEN alone (after an accepted SET_CONFIGURATION).
pub fn open<C: SduChannel>(sig: &mut Signaling<C>, acp_seid: u8) -> Result<(), StepError> {
    let r = sig.request(|l| msg::open(l, acp_seid))?;
    expect_accept("OPEN", SignalId::Open, &r)
}

/// START (the stream must be OPEN).
pub fn start<C: SduChannel>(sig: &mut Signaling<C>, acp_seid: u8) -> Result<(), StepError> {
    let r = sig.request(|l| msg::start(l, &[acp_seid]))?;
    expect_accept("START", SignalId::Start, &r)
}

/// SUSPEND (the stream must be STREAMING).
pub fn suspend<C: SduChannel>(sig: &mut Signaling<C>, acp_seid: u8) -> Result<(), StepError> {
    let r = sig.request(|l| msg::suspend(l, &[acp_seid]))?;
    expect_accept("SUSPEND", SignalId::Suspend, &r)
}

/// ABORT: the only release valid in every state (CLOSE needs OPEN).
pub fn abort<C: SduChannel>(sig: &mut Signaling<C>, acp_seid: u8) -> Result<(), StepError> {
    let r = sig.request(|l| msg::abort(l, acp_seid))?;
    expect_accept("ABORT", SignalId::Abort, &r)
}

/// Tear the stream down: CLOSE, falling back to ABORT if CLOSE is refused.
/// Returns which one was accepted.
pub fn release<C: SduChannel>(
    sig: &mut Signaling<C>,
    acp_seid: u8,
) -> Result<&'static str, StepError> {
    let r = sig.request(|l| msg::close(l, acp_seid))?;
    if expect_accept("CLOSE", SignalId::Close, &r).is_ok() {
        return Ok("CLOSE");
    }
    abort(sig, acp_seid).map(|_| "ABORT")
}

/// Interpretation of the MTUs the driver reported for a channel opened with
/// `requested_in_mtu` (driver/unverified item 1: which BRB field is which).
pub fn mtu_mapping_hint(requested_in_mtu: u16, out_mtu: u16, in_mtu: u16) -> &'static str {
    match (out_mtu == requested_in_mtu, in_mtu == requested_in_mtu) {
        (false, true) => {
            "in_mtu tracks the requested in_mtu: consistent with the driver's ConfigOut/OutResults mapping"
        }
        (true, false) => {
            "out_mtu equals the in_mtu WE requested: the driver's ConfigOut/OutResults mapping is probably SWAPPED \
             (write splitting / WRITE length check would use our own receive MTU)"
        }
        (true, true) => {
            "both equal the requested in_mtu: ambiguous - repeat with a different in_mtu"
        }
        (false, false) => {
            "neither equals the requested in_mtu: negotiated down by the peer? repeat with a different in_mtu"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeSink;
    use crate::signaling::{Signaling, probe};

    fn probed(fake: FakeSink) -> (Signaling<FakeSink>, Vec<SepReport>) {
        let mut sig = Signaling::new(fake);
        let r = probe(&mut sig).unwrap();
        (sig, r)
    }

    #[test]
    fn sbc_one_bit_per_field() {
        // Fake sink caps: 0x21 = 44.1k + joint, 0x15 = 16 blocks, 8 subbands, loudness, bitpool 2..53
        let cap = Sbc::decode(&[0x21, 0x15, 0x02, 0x35]).unwrap();
        let c = sbc_config(&cap).unwrap();
        assert_eq!(c.encode(), [0x21, 0x15, 0x02, 0x35]);
        // Everything supported: one bit each, preferred values.
        let all = Sbc::decode(&[0xFF, 0xFF, 0x02, 0xFA]).unwrap();
        let c = sbc_config(&all).unwrap();
        assert_eq!(c.encode(), [0x11, 0x15, 0x02, 53]);
        for v in [
            c.sampling_frequency,
            c.channel_mode,
            c.block_length,
            c.subbands,
            c.allocation_method,
        ] {
            assert_eq!(v.count_ones(), 1);
        }
        assert!(sbc_config(&Sbc::decode(&[0x00, 0x15, 2, 53]).unwrap()).is_none());
        assert!(sbc_config(&Sbc::decode(&[0x21, 0x15, 60, 70]).unwrap()).is_none());
    }

    #[test]
    fn chooses_sbc_sink_and_opens() {
        let (mut sig, reports) = probed(FakeSink::new(672));
        let choice = choose(&reports, None).unwrap();
        assert_eq!(choice.acp_seid, 1);
        assert_eq!(
            choice.config.len(),
            2,
            "Media Transport + Media Codec only (no Delay Reporting)"
        );
        configure_and_open(&mut sig, &choice).unwrap();
        assert_eq!(sig.ch.configured, Some((1, INT_SEID)));
        assert!(sig.ch.opened);
        assert_eq!(release(&mut sig, 1).unwrap(), "CLOSE");
        assert!(!sig.ch.opened);
    }

    #[test]
    fn explicit_seid_must_be_sbc_sink() {
        let (_, reports) = probed(FakeSink::new(672));
        assert!(choose(&reports, Some(3)).unwrap_err().contains("SBC"));
        assert!(choose(&reports, Some(9)).unwrap_err().contains("not found"));
        assert_eq!(choose(&reports, Some(1)).unwrap().acp_seid, 1);
    }

    #[test]
    fn set_configuration_reject_is_reported() {
        let mut fake = FakeSink::new(672);
        fake.reject_set_configuration = Some(0xE3);
        let (mut sig, reports) = probed(fake);
        let choice = choose(&reports, None).unwrap();
        let e = configure_and_open(&mut sig, &choice)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("SET_CONFIGURATION rejected: NOT_SUPPORTED_CODEC_PARAMETER (0xe3)"),
            "{e}"
        );
        assert!(
            !sig.ch.opened,
            "OPEN must not be sent after a rejected SET_CONFIGURATION"
        );
    }

    #[test]
    fn release_falls_back_to_abort() {
        let (mut sig, reports) = probed(FakeSink::new(672));
        let choice = choose(&reports, None).unwrap();
        configure_and_open(&mut sig, &choice).unwrap();
        sig.ch.reject_close = true;
        assert_eq!(release(&mut sig, 1).unwrap(), "ABORT");
    }

    #[test]
    fn start_and_suspend() {
        let (mut sig, reports) = probed(FakeSink::new(672));
        let choice = choose(&reports, None).unwrap();
        assert!(
            start(&mut sig, 1).is_err(),
            "START before OPEN is refused by the sink"
        );
        configure_and_open(&mut sig, &choice).unwrap();
        start(&mut sig, 1).unwrap();
        assert!(sig.ch.started);
        suspend(&mut sig, 1).unwrap();
        assert!(!sig.ch.started && sig.ch.opened);
        sig.ch.reject_start = Some(0x31);
        let e = start(&mut sig, 1).unwrap_err().to_string();
        assert!(e.contains("START rejected: BAD_STATE"), "{e}");
    }

    #[test]
    fn mtu_hints() {
        assert!(mtu_mapping_hint(1021, 895, 1021).contains("consistent"));
        assert!(mtu_mapping_hint(1021, 1021, 672).contains("SWAPPED"));
        assert!(mtu_mapping_hint(1021, 1021, 1021).contains("ambiguous"));
        assert!(mtu_mapping_hint(1021, 895, 672).contains("neither"));
    }
}
