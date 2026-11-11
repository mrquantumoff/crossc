//! The wire format: one snapshot per message.
//!
//! ```text
//! +----------------+---------------------------+
//! | length, u32 LE | JSON object, UTF-8        |
//! +----------------+---------------------------+
//! ```
//!
//! The length counts the JSON only and is at most [`MAX_MESSAGE`]. The
//! pipe is a message pipe and every message is one write, but a reader
//! needs no message mode: the length is enough to cut a byte stream
//! ([`FrameReader`]).
//!
//! Compatibility: the object carries `version`
//! ([`crate::PROTOCOL_VERSION`]). A reader takes a snapshot of its own
//! version whatever fields it has: unknown ones are ignored, missing ones
//! take their default. Another version is reported as
//! [`DecodeError::Version`] and nothing of it is used.
//!
//! Everything read is untrusted: sizes, counts and string lengths are
//! capped, characters that are not shown (or change how the rest is
//! shown) are taken out, and no input makes these functions panic. The
//! sender applies the same caps to what it is given ([`crate::Hub`]), so
//! whatever it holds fits a message.

use serde::Deserialize;

use crate::PROTOCOL_VERSION;
use crate::model::{DeviceStatus, Event, ServiceInfo, Snapshot};

/// Bytes of the length prefix.
pub const PREFIX: usize = 4;
/// Largest JSON body, bytes.
pub const MAX_MESSAGE: usize = 64 * 1024;
/// Devices kept from a snapshot (the rest is dropped).
pub const MAX_DEVICES: usize = 8;
/// Characters kept of any string (the rest is cut).
pub const MAX_TEXT: usize = 300;
/// UTF-8 bytes kept of any string. With [`MAX_TEXT`] this bounds a string
/// in JSON to 800 bytes, and eight full devices to less than
/// [`MAX_MESSAGE`].
pub const MAX_TEXT_BYTES: usize = 600;
/// Rates kept of a bitrate ladder.
pub const MAX_LADDER: usize = 16;

/// Why a snapshot could not be sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// The JSON would be longer than [`MAX_MESSAGE`].
    TooLarge(usize),
    Json(String),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::TooLarge(n) => write!(
                f,
                "the snapshot is {n} bytes, more than the {MAX_MESSAGE} a message may have"
            ),
            EncodeError::Json(e) => write!(f, "the snapshot cannot be written as JSON: {e}"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Why received bytes are not a snapshot this version can use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A length of 0 or more than [`MAX_MESSAGE`]: the stream is not ours
    /// (or out of step) and has to be dropped.
    BadLength(usize),
    /// Not a JSON object with a version, or one whose known fields have
    /// the wrong shape.
    Malformed(String),
    /// A snapshot of another protocol version.
    Version { theirs: u32 },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::BadLength(n) => write!(
                f,
                "a message of {n} bytes (1 to {MAX_MESSAGE} are possible)"
            ),
            DecodeError::Malformed(e) => write!(f, "not a status snapshot: {e}"),
            DecodeError::Version { theirs } => write!(
                f,
                "status protocol version {theirs}, this program speaks version {PROTOCOL_VERSION}"
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

/// The message for `snapshot`: length prefix and JSON.
pub fn encode(snapshot: &Snapshot) -> Result<Vec<u8>, EncodeError> {
    let mut out = vec![0u8; PREFIX];
    serde_json::to_writer(&mut out, snapshot).map_err(|e| EncodeError::Json(e.to_string()))?;
    let len = out.len() - PREFIX;
    if len > MAX_MESSAGE {
        return Err(EncodeError::TooLarge(len));
    }
    out[..PREFIX].copy_from_slice(&(len as u32).to_le_bytes());
    Ok(out)
}

/// A character that has no place in a line of status text: control
/// characters, and the Unicode format characters that are invisible or
/// change the order or direction of what follows (a name must not be able
/// to hide or rearrange the text around it). The zero-width joiner and
/// non-joiner stay: emoji and several scripts need them, and they reorder
/// nothing.
fn unwanted(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'                    // soft hyphen
            | '\u{061C}'                  // Arabic letter mark
            | '\u{180E}'                  // Mongolian vowel separator
            | '\u{200B}'                  // zero-width space
            | '\u{200E}'..='\u{200F}'     // left-to-right / right-to-left mark
            | '\u{2028}'..='\u{202E}'     // line / paragraph separator, embeddings, overrides
            | '\u{2060}'..='\u{2064}'     // word joiner, invisible operators
            | '\u{2066}'..='\u{206F}'     // isolates, deprecated format characters
            | '\u{FEFF}'                  // byte order mark
            | '\u{FFF9}'..='\u{FFFB}'     // interlinear annotation
            | '\u{E0000}'..='\u{E007F}' // tags
        )
}

/// Cut a string to [`MAX_TEXT`] characters and [`MAX_TEXT_BYTES`] bytes
/// and take out the [`unwanted`] characters (a snapshot's text ends up in
/// a tooltip, a window and the clipboard).
pub fn clean(s: &mut String) {
    if s.len() <= MAX_TEXT && !s.chars().any(unwanted) {
        return; // the usual case: short and plain
    }
    let mut out = String::with_capacity(s.len().min(MAX_TEXT_BYTES));
    for c in s.chars().filter(|c| !unwanted(*c)).take(MAX_TEXT) {
        if out.len() + c.len_utf8() > MAX_TEXT_BYTES {
            break;
        }
        out.push(c);
    }
    *s = out;
}

/// [`clean`] for an event's text.
pub fn limit_event(e: &mut Event) {
    clean(&mut e.text);
}

/// Apply the caps to the service's own fields.
pub fn limit_service(s: &mut ServiceInfo) {
    clean(&mut s.version);
    clean(&mut s.log_dir);
    if let Some(e) = &mut s.last_event {
        limit_event(e);
    }
}

/// Apply the caps to one device.
pub fn limit_device(d: &mut DeviceStatus) {
    clean(&mut d.id);
    clean(&mut d.name);
    clean(&mut d.phase_reason);
    if let Some(e) = &mut d.last_event {
        limit_event(e);
    }
    if let Some(l) = &mut d.link {
        clean(&mut l.mtu_reason);
        l.ladder_kbps.truncate(MAX_LADDER);
    }
    if let Some(v) = &mut d.volume {
        v.level_percent = v.level_percent.map(|p| p.min(100));
    }
    if let Some(drv) = &mut d.driver {
        clean(&mut drv.version);
    }
}

/// Apply the caps to a snapshot.
pub fn limit(s: &mut Snapshot) {
    s.devices.truncate(MAX_DEVICES);
    limit_service(&mut s.service);
    s.devices.iter_mut().for_each(limit_device);
}

/// The snapshot in a message body (the JSON, without the length prefix).
pub fn decode_body(body: &[u8]) -> Result<Snapshot, DecodeError> {
    if body.is_empty() || body.len() > MAX_MESSAGE {
        return Err(DecodeError::BadLength(body.len()));
    }
    // The version first, from an object that may have any other shape: a
    // newer snapshot must come out as "another version", not as garbage.
    #[derive(Deserialize)]
    struct Head {
        version: Option<u64>,
    }
    let head: Head =
        serde_json::from_slice(body).map_err(|e| DecodeError::Malformed(e.to_string()))?;
    let version = match head.version {
        None | Some(0) => return Err(DecodeError::Malformed("no version".into())),
        Some(v) => u32::try_from(v).unwrap_or(u32::MAX),
    };
    if version != PROTOCOL_VERSION {
        return Err(DecodeError::Version { theirs: version });
    }
    let mut snapshot: Snapshot =
        serde_json::from_slice(body).map_err(|e| DecodeError::Malformed(e.to_string()))?;
    limit(&mut snapshot);
    Ok(snapshot)
}

/// The snapshot in a whole message (length prefix and JSON, nothing else).
pub fn decode(message: &[u8]) -> Result<Snapshot, DecodeError> {
    let Some((prefix, body)) = message.split_at_checked(PREFIX) else {
        return Err(DecodeError::Malformed("shorter than a length".into()));
    };
    let len = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
    if len == 0 || len > MAX_MESSAGE {
        return Err(DecodeError::BadLength(len));
    }
    if len != body.len() {
        return Err(DecodeError::Malformed(format!(
            "the length says {len} bytes, the message has {}",
            body.len()
        )));
    }
    decode_body(body)
}

/// Cuts a byte stream into messages. Holds at most one message.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add bytes as they were read.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes held that are not a complete message yet.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// The next complete message, decoded. `None`: more bytes are needed.
    /// After [`DecodeError::BadLength`] the stream cannot be followed any
    /// more (every further call repeats the error); after any other error
    /// the bad message is gone and the next one can be read.
    pub fn next_frame(&mut self) -> Option<Result<Snapshot, DecodeError>> {
        let prefix = self.buf.get(..PREFIX)?;
        let len = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
        if len == 0 || len > MAX_MESSAGE {
            return Some(Err(DecodeError::BadLength(len)));
        }
        let end = PREFIX + len;
        if self.buf.len() < end {
            return None;
        }
        let result = decode_body(&self.buf[PREFIX..end]);
        self.buf.drain(..end);
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn body(json: &str) -> Vec<u8> {
        json.as_bytes().to_vec()
    }

    #[test]
    fn round_trip() {
        let s = sample();
        let msg = encode(&s).unwrap();
        assert_eq!(
            u32::from_le_bytes(msg[..4].try_into().unwrap()) as usize,
            msg.len() - 4
        );
        assert_eq!(decode(&msg).unwrap(), s);
        // Compact: a full device is well under 2 KiB.
        assert!(msg.len() < 2048, "{} bytes", msg.len());
        // The default snapshot too (it has no version: not decodable).
        let empty = Snapshot {
            version: PROTOCOL_VERSION,
            ..Snapshot::default()
        };
        assert_eq!(decode(&encode(&empty).unwrap()).unwrap(), empty);
    }

    #[test]
    fn an_older_sender_with_fewer_fields_decodes_with_defaults() {
        // What a first version might have sent: no counters, no volume, no
        // ladder, no service block at all.
        let old = r#"{"version":1,"devices":[{"id":"AABBCCDDEEFF","phase":"streaming",
            "link":{"mode":"hifi","bitrate_kbps":192}}]}"#;
        let s = decode_body(&body(old)).unwrap();
        assert_eq!(s.service, ServiceInfo::default());
        let d = &s.devices[0];
        assert_eq!(d.phase, LinkPhase::Streaming);
        assert_eq!(d.name, "");
        assert_eq!(d.volume, None);
        assert_eq!(d.counters, Counters::default());
        let l = d.link.as_ref().unwrap();
        assert_eq!((l.mode, l.bitrate_kbps), (CodecMode::Hifi, 192));
        assert_eq!(l.ladder_kbps, Vec::<u32>::new());
        assert_eq!(l.sink_delay_tenth_ms, None);
        // Nothing but the version.
        let s = decode_body(&body(r#"{"version":1}"#)).unwrap();
        assert!(s.devices.is_empty());
    }

    #[test]
    fn a_newer_sender_with_more_fields_and_values_decodes() {
        let new = r#"{"version":1,"seq":3,"build":"nightly","service":{"version":"0.9.0",
            "uptime":{"days":3},"log_dir":"D:\\logs"},
            "devices":[{"id":"AABBCCDDEEFF","phase":"calibrating","battery":{"left":80,"right":75},
              "link":{"mode":"lossless","bitrate_kbps":1200,"spatial":true,"ladder_kbps":[1200,584]},
              "volume":{"mode":"spatial","level_percent":40,"muted":false,"balance":-3},
              "last_event":{"level":"notice","time_unix":5,"text":"x","code":17}}],
            "extra":[1,2,{"a":null}]}"#;
        let s = decode_body(&body(new)).unwrap();
        assert_eq!(s.seq, 3);
        assert_eq!(s.service.version, "0.9.0");
        assert_eq!(s.service.log_dir, r"D:\logs");
        let d = &s.devices[0];
        assert_eq!(d.phase, LinkPhase::Unknown);
        let l = d.link.as_ref().unwrap();
        assert_eq!(l.mode, CodecMode::Unknown);
        assert_eq!(l.bitrate_kbps, 1200);
        assert_eq!(l.ladder_kbps, vec![1200, 584]);
        let v = d.volume.as_ref().unwrap();
        assert_eq!((v.mode, v.level_percent), (VolumeMode::Unknown, Some(40)));
        assert_eq!(d.last_event.as_ref().unwrap().level, EventLevel::Unknown);
    }

    #[test]
    fn another_version_is_reported_as_such_whatever_its_shape() {
        for (json, theirs) in [
            (r#"{"version":2}"#, 2),
            // Known fields with shapes version 1 could not read.
            (r#"{"version":2,"devices":"none","service":7,"seq":"x"}"#, 2),
            (r#"{"version":99,"devices":[{"phase":{"name":"x"}}]}"#, 99),
            (r#"{"version":18446744073709551615}"#, u32::MAX),
        ] {
            assert_eq!(
                decode_body(&body(json)),
                Err(DecodeError::Version { theirs }),
                "{json}"
            );
        }
        let text = DecodeError::Version { theirs: 2 }.to_string();
        assert!(
            text.contains("version 2") && text.contains("version 1"),
            "{text}"
        );
    }

    #[test]
    fn garbage_is_an_error_and_never_a_panic() {
        let cases: Vec<Vec<u8>> = vec![
            body("null"),
            body("[]"),
            body("7"),
            body("\"version\""),
            body("{}"),
            body(r#"{"version":0}"#),
            body(r#"{"version":-1}"#),
            body(r#"{"version":"1"}"#),
            body(r#"{"version":1.5}"#),
            body(r#"{"version":1,"devices":7}"#),
            body(r#"{"version":1,"devices":[{"id":5}]}"#),
            body(r#"{"version":1,"devices":[{"link":{"bitrate_kbps":-4}}]}"#),
            body(r#"{"version":1,"devices":[{"link":{"config":256}}]}"#),
            body(r#"{"version":1,"seq":1e400}"#),
            body(r#"{"version":1"#),
            body(r#"{"version":1}}"#),
            vec![0xFF, 0xFE, 0x00, 0x7B],
            vec![0u8; 64],
            body(&"[".repeat(5000)),
        ];
        for c in cases {
            assert!(
                matches!(decode_body(&c), Err(DecodeError::Malformed(_))),
                "{:?}",
                String::from_utf8_lossy(&c[..c.len().min(40)])
            );
        }
        // Deep nesting in a field this version does not know is skipped
        // without recursion.
        let deep = format!(
            r#"{{"version":1,"x":{}1{},"seq":4}}"#,
            "[".repeat(20_000),
            "]".repeat(20_000)
        );
        assert_eq!(decode_body(deep.as_bytes()).unwrap().seq, 4);
        assert_eq!(decode_body(&[]), Err(DecodeError::BadLength(0)));
        let big = vec![b' '; MAX_MESSAGE + 1];
        assert_eq!(
            decode_body(&big),
            Err(DecodeError::BadLength(MAX_MESSAGE + 1))
        );
    }

    #[test]
    fn every_truncation_and_every_single_byte_change_is_survived() {
        let msg = encode(&sample()).unwrap();
        for cut in 0..msg.len() {
            let _ = decode(&msg[..cut]);
            let mut r = FrameReader::new();
            r.push(&msg[..cut]);
            assert!(r.next_frame().is_none() || cut >= PREFIX);
        }
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        for i in 0..msg.len() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let mut m = msg.clone();
            m[i] ^= (seed as u8) | 1;
            let _ = decode(&m);
            let mut r = FrameReader::new();
            r.push(&m);
            while let Some(Ok(_)) = r.next_frame() {}
        }
    }

    #[test]
    fn sizes_counts_and_strings_are_capped() {
        let mut s = sample();
        let d = s.devices[0].clone();
        s.devices = (0..100)
            .map(|i| DeviceStatus {
                id: format!("{i:012X}"),
                ..d.clone()
            })
            .collect();
        // 100 full devices do not fit a message: the sender is told.
        assert!(matches!(encode(&s), Err(EncodeError::TooLarge(_))));
        // A hostile sender is cut down instead.
        let many: Vec<String> = (0..200)
            .map(|i| format!(r#"{{"id":"{i}","name":"{}"}}"#, "n".repeat(100)))
            .collect();
        let json = format!(
            r#"{{"version":1,"service":{{"log_dir":"{}"}},"devices":[{}]}}"#,
            "d".repeat(5000),
            many.join(",")
        );
        let s = decode_body(json.as_bytes()).unwrap();
        assert_eq!(s.devices.len(), MAX_DEVICES);
        assert_eq!(s.service.log_dir.chars().count(), MAX_TEXT);
        let json = format!(
            r#"{{"version":1,"devices":[{{"name":"a\u0000b\nc\u001b[31m","phase_reason":"{}",
            "volume":{{"level_percent":250}},
            "link":{{"ladder_kbps":[{}]}}}}]}}"#,
            "é".repeat(400),
            vec!["1"; 500].join(",")
        );
        let s = decode_body(json.as_bytes()).unwrap();
        let d = &s.devices[0];
        assert_eq!(d.name, "abc[31m");
        assert_eq!(d.phase_reason.chars().count(), MAX_TEXT);
        assert_eq!(d.volume.as_ref().unwrap().level_percent, Some(100));
        assert_eq!(d.link.as_ref().unwrap().ladder_kbps.len(), MAX_LADDER);
    }

    #[test]
    fn invisible_and_reordering_characters_are_taken_out() {
        // A name that would show as "Buds exe.png" with the override in
        // effect, a line separator, a zero-width space, a BOM, a tag.
        let json = r#"{"version":1,"devices":[{"name":"Buds\u202Egnp.exe\u2028\u200B\uFEFF\u2066x\u2069\uDB40\uDC41",
            "phase_reason":"a\u00ADb\u200Ec\u061Cd","id":"👨\u200D👩\u200D👧 می\u200Cخواهم"}]}"#;
        let s = decode_body(json.as_bytes()).unwrap();
        let d = &s.devices[0];
        assert_eq!(d.name, "Budsgnp.exex");
        assert_eq!(d.phase_reason, "abcd");
        // Joiners stay: the family is one emoji, the Persian word is whole.
        assert_eq!(d.id, "👨\u{200D}👩\u{200D}👧 می\u{200C}خواهم");
        // What is left is what `clean` leaves alone.
        let mut again = d.name.clone();
        clean(&mut again);
        assert_eq!(again, d.name);
    }

    #[test]
    fn strings_are_capped_in_bytes_too() {
        let mut s = "🎧".repeat(MAX_TEXT);
        clean(&mut s);
        assert_eq!(s.len(), MAX_TEXT_BYTES);
        assert_eq!(s.chars().count(), MAX_TEXT_BYTES / 4);
        let mut s = format!("{}é", "a".repeat(MAX_TEXT_BYTES - 1));
        clean(&mut s);
        assert_eq!(s.chars().count(), MAX_TEXT, "the character cap comes first");
        let mut s = format!("{}🎧🎧", "é".repeat(299));
        clean(&mut s);
        assert_eq!(s, "é".repeat(299), "never half a character");
    }

    #[test]
    fn decode_checks_the_length_against_the_message() {
        let msg = encode(&sample()).unwrap();
        let mut long = msg.clone();
        long.push(b' ');
        assert!(matches!(decode(&long), Err(DecodeError::Malformed(_))));
        assert!(matches!(decode(&msg[..3]), Err(DecodeError::Malformed(_))));
        let mut zero = msg.clone();
        zero[..4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode(&zero), Err(DecodeError::BadLength(0)));
        let mut huge = msg;
        huge[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            decode(&huge),
            Err(DecodeError::BadLength(u32::MAX as usize))
        );
    }

    #[test]
    fn frame_reader_cuts_a_stream_at_any_chunking() {
        let a = sample();
        let mut b = sample();
        b.seq = 8;
        b.devices.clear();
        let mut stream = encode(&a).unwrap();
        stream.extend(encode(&b).unwrap());
        for chunk in [1usize, 2, 3, 5, 64, 1000, stream.len()] {
            let mut r = FrameReader::new();
            let mut got = Vec::new();
            for part in stream.chunks(chunk) {
                r.push(part);
                while let Some(s) = r.next_frame() {
                    got.push(s.unwrap());
                }
            }
            assert_eq!(got, vec![a.clone(), b.clone()], "chunk {chunk}");
            assert_eq!(r.pending(), 0);
        }
    }

    #[test]
    fn frame_reader_skips_a_bad_message_and_stops_at_a_bad_length() {
        let good = encode(&sample()).unwrap();
        let bad_body = br#"{"version":3}"#;
        let mut stream = (bad_body.len() as u32).to_le_bytes().to_vec();
        stream.extend_from_slice(bad_body);
        stream.extend_from_slice(&good);
        let mut r = FrameReader::new();
        r.push(&stream);
        assert_eq!(
            r.next_frame(),
            Some(Err(DecodeError::Version { theirs: 3 }))
        );
        assert_eq!(r.next_frame(), Some(Ok(sample())));
        assert_eq!(r.next_frame(), None);
        // A length no message has: stuck there, and it does not grow.
        r.push(&(MAX_MESSAGE as u32 + 1).to_le_bytes());
        r.push(&good);
        for _ in 0..3 {
            assert_eq!(
                r.next_frame(),
                Some(Err(DecodeError::BadLength(MAX_MESSAGE + 1)))
            );
        }
        let mut r = FrameReader::new();
        r.push(&[0, 0, 0, 0, 1, 2, 3]);
        assert_eq!(r.next_frame(), Some(Err(DecodeError::BadLength(0))));
    }
}
