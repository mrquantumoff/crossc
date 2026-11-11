//! The host pin's default format: what the stored registry values select,
//! what is stored for a choice, and how the session reports it.

use crossc_ioctl::audio::{AudioState, OFFERED_FORMATS, PcmFormat};

use crate::audio_core::{Session, stored_default, stored_values, write_state};

#[test]
fn stored_values_select_the_offered_format() {
    // Nothing stored, or a rate the endpoint does not offer: 48 kHz/24, the
    // default the driver had before the choice existed.
    assert_eq!(stored_default(None, None), 0);
    assert_eq!(stored_default(None, Some(16)), 0);
    assert_eq!(stored_default(Some(44_100), Some(16)), 0);
    assert_eq!(stored_default(Some(0), None), 0);
    assert_eq!(stored_default(Some(u32::MAX), Some(u32::MAX)), 0);
    // The three formats, with and without the depth.
    assert_eq!(stored_default(Some(48_000), Some(24)), 0);
    assert_eq!(stored_default(Some(48_000), None), 0);
    assert_eq!(stored_default(Some(96_000), Some(24)), 1);
    assert_eq!(stored_default(Some(96_000), None), 1);
    assert_eq!(stored_default(Some(48_000), Some(16)), 2);
    // A depth the rate does not have: the rate decides.
    assert_eq!(stored_default(Some(96_000), Some(16)), 1);
    assert_eq!(stored_default(Some(48_000), Some(8)), 0);
}

#[test]
fn what_is_stored_reads_back_as_the_same_format() {
    for i in 0..OFFERED_FORMATS.len() {
        let (rate, bits) = stored_values(i).unwrap();
        assert_eq!(stored_default(Some(rate), Some(bits)), i, "index {i}");
    }
    assert_eq!(stored_values(OFFERED_FORMATS.len()), None);
    assert_eq!(stored_values(1), Some((96_000, 24)));
    assert_eq!(stored_values(2), Some((48_000, 16)));
}

#[test]
fn the_session_reports_the_default_and_a_change_bumps_seq() {
    let mut s = Session::new();
    assert_eq!(s.state().default_format, PcmFormat::default(), "unknown until set");
    let seq = s.state().seq;
    assert!(s.set_default_format(OFFERED_FORMATS[1]));
    assert_ne!(s.state().seq, seq);
    assert_eq!(s.state().default_format, OFFERED_FORMATS[1]);
    let seq = s.state().seq;
    assert!(!s.set_default_format(OFFERED_FORMATS[1]), "no change, no bump");
    assert_eq!(s.state().seq, seq);
    // On the wire: a v4 buffer gets it, a v3 one (64 bytes) the rest.
    let mut out = [0u8; AudioState::SIZE];
    assert_eq!(write_state(&mut out, &s.state()), Some(AudioState::SIZE));
    assert_eq!(AudioState::decode(&out).unwrap().default_format, OFFERED_FORMATS[1]);
    let mut v3 = [0u8; AudioState::SIZE_V3];
    assert_eq!(write_state(&mut v3, &s.state()), Some(AudioState::SIZE_V3));
    assert_eq!(AudioState::decode(&v3).unwrap().default_format, PcmFormat::default());
}
