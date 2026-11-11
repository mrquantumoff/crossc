use std::string::String;
use std::vec::Vec;

use crate::bt_name::{MAX_UNITS, Utf16Name, to_utf16};

fn conv(name: &[u8]) -> Option<String> {
    let mut out: Utf16Name = [0xFFFF; MAX_UNITS + 1];
    let n = to_utf16(name, &mut out);
    if n == 0 {
        return None;
    }
    assert_eq!(out[n - 1], 0, "terminated");
    assert!(n <= MAX_UNITS + 1);
    assert!(out[..n - 1].iter().all(|&u| u != 0), "no inner NUL");
    Some(String::from_utf16(&out[..n - 1]).expect("valid UTF-16"))
}

/// The 248-byte field of BTH_DEVICE_INFO, zero padded.
fn field(s: &str) -> [u8; 248] {
    let mut f = [0u8; 248];
    f[..s.len()].copy_from_slice(s.as_bytes());
    f
}

#[test]
fn plain_names() {
    assert_eq!(conv(&field("Someone's Buds4 Pro")).as_deref(), Some("Someone's Buds4 Pro"));
    assert_eq!(conv(b"Galaxy Buds3 (1234)").as_deref(), Some("Galaxy Buds3 (1234)"));
    // Not NUL-terminated: the whole slice is the name.
    assert_eq!(conv(b"AB").as_deref(), Some("AB"));
    // Stops at the first NUL.
    assert_eq!(conv(b"AB\0CD").as_deref(), Some("AB"));
}

#[test]
fn non_ascii_names() {
    let s = "S24 Ultra \u{43a}\u{43e}\u{440}\u{438}\u{441}\u{442}\u{443}\u{432}\u{430}\u{447}\u{430}";
    assert_eq!(conv(&field(s)).as_deref(), Some(s));
    // Outside the BMP: a surrogate pair.
    assert_eq!(conv("Buds \u{1F3A7}".as_bytes()).as_deref(), Some("Buds \u{1F3A7}"));
}

#[test]
fn empty_and_unusable_names() {
    assert_eq!(conv(&[0u8; 248]), None);
    assert_eq!(conv(b""), None);
    assert_eq!(conv(b"   "), None);
    assert_eq!(conv(b"\t\r\n"), None);
    assert_eq!(conv(&[0xFF, 0xFE]), None);
}

#[test]
fn invalid_utf8_ends_the_name() {
    assert_eq!(conv(b"Buds\xFFjunk").as_deref(), Some("Buds"));
    // A multi-byte character cut in half by the end of the field.
    assert_eq!(conv(b"Buds \xF0\x9F\x8E").as_deref(), Some("Buds"));
}

#[test]
fn control_characters_and_padding_are_removed() {
    assert_eq!(conv(b"  Buds\x01 Pro\x7F \r\n").as_deref(), Some("Buds Pro"));
    assert_eq!(conv(b"A\x07 ").as_deref(), Some("A"));
}

#[test]
fn long_names_are_cut_at_a_character_boundary() {
    let long: Vec<u8> = core::iter::repeat_n(b'x', 248).collect();
    let s = conv(&long).unwrap();
    assert_eq!(s.len(), MAX_UNITS);

    // MAX_UNITS - 1 BMP characters, then one that needs two units: it must
    // be left out as a whole, never split.
    let mut name = String::new();
    for _ in 0..MAX_UNITS - 1 {
        name.push('\u{43a}');
    }
    name.push('\u{1F3A7}');
    let s = conv(name.as_bytes()).unwrap();
    assert_eq!(s.encode_utf16().count(), MAX_UNITS - 1);
    assert!(s.chars().all(|c| c == '\u{43a}'));
}
