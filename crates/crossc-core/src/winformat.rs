//! The format picked for the buds' playback device in the Windows sound
//! settings, and making the driver's default format follow it.
//!
//! Windows does not play in the format picked there: the endpoint answers
//! `KSPROPERTY_PIN_PROPOSEDATAFORMAT2` with the default of its format list,
//! and the audio engine then always opens its stream in that default
//! (`crossc_ioctl::audio`, "Default format"). So the service reads the
//! choice and makes it the driver's default with
//! `IOCTL_AUDIO_SET_DEFAULT_FORMAT`; the next stream Windows opens has it.
//!
//! # Where the choice is
//! Windows keeps every render endpoint under [`RENDER_KEY`] (HKLM), one
//! subkey per endpoint with a `Properties` subkey. The registry is read
//! rather than the MMDevice API because the service runs as LocalSystem in
//! session 0, and because a change notification on a key costs nothing
//! while nothing changes.
//! * [`KS_PATH_VALUE`]: the KS filter the endpoint belongs to, as
//!   `{2}.\\?\<the device interface of the circuit>\<reference string>`. The
//!   device instance part is the same as in the crossc interface path the
//!   session runs on, and the reference string is the circuit's name
//!   ([`CIRCUIT_REFERENCE`]). The device instance property
//!   (`{b3f8fa53-0004-438e-9003-51a46e139bfc},2`) alone is not enough: the
//!   endpoint the inbox A2DP driver left on the same device node has it too.
//! * `DeviceState` (DWORD, in the endpoint's key): the low bits are
//!   `DEVICE_STATE_ACTIVE` (1), `_DISABLED` (2), `_NOTPRESENT` (4),
//!   `_UNPLUGGED` (8). Unplugged counts: the jack reports unplugged
//!   whenever the buds are away, which is just when the default should be
//!   set for the next connection.
//! * [`DEVICE_FORMAT_VALUE`] (`PKEY_AudioEngine_DeviceFormat`): a
//!   `PROPVARIANT`-like blob, `VT_BLOB` (0x41) in its first two bytes and
//!   the `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` from byte 8 on
//!   ([`parse_device_format`]).

use std::time::Duration;

use crossc_ioctl::audio::{OFFERED_FORMATS, PcmFormat, StreamState, offered_for};

/// Render endpoints, under HKLM.
pub const RENDER_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render";
/// `PKEY_AudioEngine_DeviceFormat`, in an endpoint's `Properties` key.
pub const DEVICE_FORMAT_VALUE: &str = "{f19f064d-082c-4e27-bc73-6882a1bb8e4c},0";
/// The KS filter path of the endpoint, in its `Properties` key.
pub const KS_PATH_VALUE: &str = "{233164c8-1b2c-4c7d-bc68-b671687a2567},1";
/// The render circuit's reference string (`KSNAME_Headphones` in the INF).
pub const CIRCUIT_REFERENCE: &str = "headphones0";
/// How long the engine may keep rendering another format after the default
/// was set before the session says that the change waits for a new stream.
pub const APPLY_GRACE: Duration = Duration::from_secs(5);
/// The only format a driver older than protocol 4 offers (48000 Hz/24).
pub const OLD_DRIVER_FORMAT: PcmFormat = OFFERED_FORMATS[0];

const VT_BLOB: u16 = 0x41;
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// The common tail of `KSDATAFORMAT_SUBTYPE_PCM` / `_IEEE_FLOAT`
/// (`xxxxxxxx-0000-0010-8000-00aa00389b71`), bytes 4..16 as stored.
const SUBTYPE_TAIL: [u8; 12] = [0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71];

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// The format in a [`DEVICE_FORMAT_VALUE`] blob. Integer PCM keeps its
/// container and valid bits; IEEE float comes out as 32 valid bits in 32.
/// `None` for anything else, or a blob too short for what it claims.
pub fn parse_device_format(blob: &[u8]) -> Option<PcmFormat> {
    if u16_at(blob, 0)? != VT_BLOB {
        return None;
    }
    let w = blob.get(8..)?;
    let tag = u16_at(w, 0)?;
    let channels = u8::try_from(u16_at(w, 2)?).ok()?;
    let rate = u32_at(w, 4)?;
    let container = u8::try_from(u16_at(w, 14)?).ok()?;
    let (float, valid) = match tag {
        WAVE_FORMAT_PCM => (false, container),
        WAVE_FORMAT_IEEE_FLOAT => (true, container),
        WAVE_FORMAT_EXTENSIBLE => {
            if u16_at(w, 16)? < 22 {
                return None;
            }
            let valid = u8::try_from(u16_at(w, 18)?).ok()?;
            let sub = w.get(24..40)?;
            if sub[4..] != SUBTYPE_TAIL {
                return None;
            }
            let float = match u32_at(sub, 0)? {
                1 => false,
                3 => true,
                _ => return None,
            };
            (float, if valid == 0 { container } else { valid })
        }
        _ => return None,
    };
    if channels == 0 || rate == 0 || container == 0 || valid > container {
        return None;
    }
    Some(PcmFormat {
        sample_rate: rate,
        channels,
        container_bits: container,
        valid_bits: if float { 32 } else { valid },
    })
}

/// The endpoint format the driver offers for a Windows device format: the
/// same rate, 24-bit for anything of 24 bits or more. `None` for a rate the
/// endpoint does not offer.
pub fn offered(windows: PcmFormat) -> Option<PcmFormat> {
    offered_for(windows.sample_rate, u32::from(windows.valid_bits)).map(|i| OFFERED_FORMATS[i])
}

/// `96000 Hz/24`.
pub fn short(f: PcmFormat) -> String {
    format!("{} Hz/{}", f.sample_rate, f.valid_bits)
}

/// The first three `#`-separated parts of a device interface path (bus and
/// the two halves of the device instance id), lowercased; `None` if the
/// path has fewer than four parts.
fn instance_parts(path: &str) -> Option<[String; 3]> {
    let lower = path.to_ascii_lowercase();
    let mut parts = lower.split('#');
    let p = [parts.next()?, parts.next()?, parts.next()?].map(str::to_string);
    parts.next()?;
    Some(p)
}

/// Whether a [`KS_PATH_VALUE`] is the circuit of the crossc device whose
/// interface path is `interface`: same device instance, reference string
/// [`CIRCUIT_REFERENCE`].
pub fn is_crossc_endpoint(ks_path: &str, interface: &str) -> bool {
    // "{2}.\\?\bthenum#...": the "{2}." in front is not part of the path.
    let path = match ks_path.find("\\\\?\\") {
        Some(i) => &ks_path[i..],
        None => return false,
    };
    let (Some(ours), Some(theirs)) = (instance_parts(interface), instance_parts(path)) else {
        return false;
    };
    let reference = path.rsplit('\\').next().unwrap_or("");
    ours == theirs && reference.eq_ignore_ascii_case(CIRCUIT_REFERENCE)
}

/// How usable an endpoint in `DeviceState` `state` is: `Some(0)` active,
/// `Some(1)` unplugged (lower is better), `None` disabled or not present.
pub fn state_rank(state: u32) -> Option<u8> {
    match state & 0xF {
        1 => Some(0),
        8 => Some(1),
        _ => None,
    }
}

/// Where the session learns the Windows device format from.
pub trait FormatSource {
    /// True once after every change of [`Self::current`] (and once at the
    /// start); cheap enough to be asked before every wait.
    fn changed(&mut self) -> bool;
    /// The device format of the buds' endpoint; `None` while no endpoint
    /// of theirs is found.
    fn current(&self) -> Option<PcmFormat>;
}

/// What [`FormatSync::decide`] wants done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Make this the driver's default (SET_DEFAULT_FORMAT).
    Send(PcmFormat),
    /// The Windows format changed and the driver's default is already
    /// this: say so.
    AlreadySet(PcmFormat),
    /// The Windows format changed to one the endpoint does not offer.
    NotOffered(PcmFormat),
    /// The driver is older than protocol 4 and cannot follow a format other
    /// than the one it always plays, [`OLD_DRIVER_FORMAT`] (said once per
    /// attach).
    OldDriver(PcmFormat),
}

/// The decisions behind following the Windows format, as plain data: what
/// to send when, never the same thing twice, and when to say that the
/// engine still renders the old format.
#[derive(Clone, Debug, Default)]
pub struct FormatSync {
    /// The Windows device format as last reported.
    windows: Option<PcmFormat>,
    /// The offered format for it.
    target: Option<PcmFormat>,
    /// A change of the Windows format not yet acted on or reported.
    news: bool,
    /// The target last sent to this driver (successfully or not) for the
    /// current Windows format; not sent again until that format changes or
    /// the driver is attached anew.
    sent: Option<PcmFormat>,
    old_driver_said: bool,
    /// A default set in this attach, waited for in the stream format.
    applying: Option<PcmFormat>,
    /// Since when the running stream has had another format.
    other_since: Option<Duration>,
    warned: bool,
}

impl FormatSync {
    pub fn new() -> Self {
        Self::default()
    }

    /// The Windows format as reported now.
    pub fn windows(&self) -> Option<PcmFormat> {
        self.windows
    }

    /// The source reports `f`.
    pub fn report(&mut self, f: Option<PcmFormat>) {
        if f == self.windows {
            return;
        }
        self.windows = f;
        self.target = f.and_then(offered);
        self.news = f.is_some();
        // A new choice is acted on even if an earlier one was the same
        // format (and failed): "not twice" is per choice.
        self.sent = None;
    }

    /// The driver session ended: what was sent is forgotten (a driver that
    /// comes back may have restarted with another default), and nothing
    /// is waited for any more.
    pub fn driver_lost(&mut self) {
        self.sent = None;
        self.old_driver_said = false;
        self.stop_waiting();
    }

    fn stop_waiting(&mut self) {
        self.applying = None;
        self.other_since = None;
        self.warned = false;
    }

    /// What to do, with the driver attached at `protocol` and reporting
    /// `default` (all zero: unknown).
    pub fn decide(&mut self, protocol: u32, default: PcmFormat) -> Step {
        let news = std::mem::take(&mut self.news);
        let Some(windows) = self.windows else {
            return Step::Nothing;
        };
        if protocol < 4 {
            match self.target {
                // What that driver plays anyway: nothing to say.
                Some(t) if t == OLD_DRIVER_FORMAT => return Step::Nothing,
                Some(_) if self.old_driver_said => return Step::Nothing,
                Some(_) => {
                    self.old_driver_said = true;
                    return Step::OldDriver(windows);
                }
                // Not offered by any driver: said as for a v4 one below.
                None => {}
            }
        }
        let Some(target) = self.target else {
            return if news {
                Step::NotOffered(windows)
            } else {
                Step::Nothing
            };
        };
        if !default.is_valid() {
            return Step::Nothing;
        }
        if default == target {
            return if news {
                Step::AlreadySet(target)
            } else {
                Step::Nothing
            };
        }
        if self.sent == Some(target) {
            return Step::Nothing;
        }
        self.sent = Some(target);
        Step::Send(target)
    }

    /// The default set in this attach that the stream has not followed yet.
    pub fn applying(&self) -> Option<PcmFormat> {
        self.applying
    }

    /// The driver took `f` as its default: wait for the stream to follow.
    pub fn applied(&mut self, f: PcmFormat) {
        self.stop_waiting();
        self.applying = Some(f);
    }

    /// The stream as the driver reports it, at `now`. True once when the
    /// engine has rendered another format than the default just set for
    /// [`APPLY_GRACE`] without a break.
    pub fn check(&mut self, now: Duration, stream: StreamState, format: PcmFormat) -> bool {
        let Some(want) = self.applying else {
            return false;
        };
        if stream != StreamState::Running || !format.is_valid() {
            self.other_since = None;
            return false;
        }
        if format == want {
            self.stop_waiting();
            return false;
        }
        let since = *self.other_since.get_or_insert(now);
        if !self.warned && now.saturating_sub(since) >= APPLY_GRACE {
            self.warned = true;
            return true;
        }
        false
    }

    /// When [`Self::check`] may next say yes (the session wakes up by then).
    pub fn deadline(&self) -> Option<Duration> {
        if self.warned {
            return None;
        }
        self.other_since.map(|t| t + APPLY_GRACE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F48: PcmFormat = OFFERED_FORMATS[0];
    const F96: PcmFormat = OFFERED_FORMATS[1];
    const F16: PcmFormat = OFFERED_FORMATS[2];

    /// A device format blob as Windows stores it: the 8-byte header, then a
    /// WAVEFORMATEXTENSIBLE (PCM or float subtype).
    fn blob(rate: u32, container: u16, valid: u16, float: bool) -> Vec<u8> {
        let mut b = vec![0x41, 0, 0, 0, 1, 0, 0, 0];
        b.extend_from_slice(&0xFFFEu16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        let align = 2 * container / 8;
        b.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
        b.extend_from_slice(&align.to_le_bytes());
        b.extend_from_slice(&container.to_le_bytes());
        b.extend_from_slice(&22u16.to_le_bytes());
        b.extend_from_slice(&valid.to_le_bytes());
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&(if float { 3u32 } else { 1u32 }).to_le_bytes());
        b.extend_from_slice(&SUBTYPE_TAIL);
        b
    }

    #[test]
    fn device_format_blobs() {
        // As stored for "24 bit, 48000 Hz" (bytes copied from a real
        // endpoint; the format is all there is in it).
        let real: [u8; 48] = [
            0x41, 0, 0, 0, 1, 0, 0, 0, 0xfe, 0xff, 2, 0, 0x80, 0xbb, 0, 0, 0, 0xdc, 5, 0, 8, 0,
            0x20, 0, 0x16, 0, 0x18, 0, 3, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0,
            0x38, 0x9b, 0x71,
        ];
        assert_eq!(parse_device_format(&real), Some(F48));
        assert_eq!(real.to_vec(), blob(48_000, 32, 24, false));
        assert_eq!(parse_device_format(&blob(96_000, 32, 24, false)), Some(F96));
        assert_eq!(parse_device_format(&blob(48_000, 16, 16, false)), Some(F16));
        // Float: 32 valid bits.
        let fl = parse_device_format(&blob(48_000, 32, 32, true)).unwrap();
        assert_eq!((fl.container_bits, fl.valid_bits), (32, 32));
        // No valid bits given: the container's.
        let b = blob(44_100, 16, 0, false);
        assert_eq!(parse_device_format(&b).map(|f| f.valid_bits), Some(16));
        // Plain WAVEFORMATEX (PCM): 18 bytes after the header.
        let mut plain = vec![0x41, 0, 0, 0, 1, 0, 0, 0, 1, 0, 2, 0];
        plain.extend_from_slice(&96_000u32.to_le_bytes());
        plain.extend_from_slice(&[0; 4]);
        plain.extend_from_slice(&[4, 0, 16, 0, 0, 0]);
        let p = parse_device_format(&plain).unwrap();
        assert_eq!(
            (p.sample_rate, p.container_bits, p.valid_bits),
            (96_000, 16, 16)
        );
    }

    #[test]
    fn bad_blobs_are_refused_not_misread() {
        let good = blob(96_000, 32, 24, false);
        assert_eq!(parse_device_format(&[]), None);
        assert_eq!(parse_device_format(&good[..20]), None, "cut short");
        assert_eq!(parse_device_format(&good[..47]), None, "subtype cut");
        let mut b = good.clone();
        b[0] = 0x1F; // not VT_BLOB
        assert_eq!(parse_device_format(&b), None);
        let mut b = good.clone();
        b[8] = 0x55; // unknown format tag
        assert_eq!(parse_device_format(&b), None);
        let mut b = good.clone();
        b[32] = 0x92; // another subtype (AC-3 over S/PDIF, say)
        assert_eq!(parse_device_format(&b), None);
        let mut b = good.clone();
        b[47] = 0; // not the KSDATAFORMAT_SUBTYPE family
        assert_eq!(parse_device_format(&b), None);
        let mut b = good.clone();
        b[10] = 0; // no channels
        assert_eq!(parse_device_format(&b), None);
        let mut b = good.clone();
        b[26] = 40; // more valid bits than the container
        assert_eq!(parse_device_format(&b), None);
        let mut b = good;
        b[24] = 0; // cbSize too small for the extensible part
        assert_eq!(parse_device_format(&b), None);
    }

    #[test]
    fn windows_formats_map_to_offered_ones() {
        assert_eq!(offered(F96), Some(F96));
        assert_eq!(offered(F48), Some(F48));
        assert_eq!(offered(F16), Some(F16));
        let float48 = PcmFormat {
            valid_bits: 32,
            ..F48
        };
        assert_eq!(offered(float48), Some(F48));
        assert_eq!(offered(PcmFormat { channels: 1, ..F96 }), Some(F96));
        assert_eq!(
            offered(PcmFormat {
                sample_rate: 44_100,
                ..F48
            }),
            None
        );
        assert_eq!(short(F96), "96000 Hz/24");
    }

    const IFACE: &str = r"\\?\BTHENUM#{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013#b&1a2b3c4d&0&AABBCCDDEEFF_C00000000#{433f9eba-036f-480d-826f-e0ec953319a9}";

    #[test]
    fn endpoints_are_matched_by_instance_and_reference() {
        let ks = r"{2}.\\?\bthenum#{0000110b-0000-1000-8000-00805f9b34fb}_vid&00010075_pid&a013#b&1a2b3c4d&0&aabbccddeeff_c00000000#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\headphones0";
        assert!(is_crossc_endpoint(ks, IFACE));
        assert!(is_crossc_endpoint(&ks.to_ascii_uppercase(), IFACE));
        // The endpoint the inbox A2DP driver left on the same device node.
        assert!(!is_crossc_endpoint(
            &ks.replace(r"\headphones0", r"\src"),
            IFACE
        ));
        // Other buds, other node.
        assert!(!is_crossc_endpoint(
            &ks.replace("aabbccddeeff", "112233445566"),
            IFACE
        ));
        // Another device entirely, a truncated value, garbage.
        let hfp = r"{2}.\\?\bthhfenum#bthhfpaudio#c&1a2b&0&97#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\wave";
        assert!(!is_crossc_endpoint(hfp, IFACE));
        assert!(!is_crossc_endpoint(r"{2}.\\?\bthenum#x", IFACE));
        assert!(!is_crossc_endpoint("", IFACE));
        assert!(!is_crossc_endpoint(ks, "not a path"));
    }

    #[test]
    fn device_states() {
        assert_eq!(state_rank(1), Some(0));
        assert_eq!(state_rank(8), Some(1));
        assert_eq!(state_rank(0x1000_0001), Some(0), "high bits ignored");
        assert_eq!(state_rank(0x2100_0004), None, "not present");
        assert_eq!(state_rank(2), None, "disabled");
        assert_eq!(state_rank(0), None);
    }

    #[test]
    fn sends_on_a_change_and_never_twice() {
        let mut s = FormatSync::new();
        // Nothing known yet: nothing to do.
        assert_eq!(s.decide(4, F48), Step::Nothing);
        s.report(Some(F96));
        assert_eq!(s.decide(4, F48), Step::Send(F96));
        // Until the driver reports the new default: not sent again.
        assert_eq!(s.decide(4, F48), Step::Nothing);
        assert_eq!(s.decide(4, F96), Step::Nothing);
        // The same report again changes nothing.
        s.report(Some(F96));
        assert_eq!(s.decide(4, F96), Step::Nothing);
        // Back to 48 kHz.
        s.report(Some(F48));
        assert_eq!(s.decide(4, F96), Step::Send(F48));
        assert_eq!(s.decide(4, F48), Step::Nothing);
        // 16-bit, and a format already in place.
        s.report(Some(F16));
        assert_eq!(s.decide(4, F48), Step::Send(F16));
        s.report(Some(F48));
        assert_eq!(s.decide(4, F48), Step::AlreadySet(F48));
        assert_eq!(s.decide(4, F48), Step::Nothing);
    }

    #[test]
    fn a_failed_send_waits_for_a_change_or_a_new_driver() {
        let mut s = FormatSync::new();
        s.report(Some(F96));
        assert_eq!(s.decide(4, F48), Step::Send(F96));
        // It failed: the driver still says 48. Not retried at every step.
        for _ in 0..5 {
            assert_eq!(s.decide(4, F48), Step::Nothing);
        }
        // The driver is attached anew: compared again, sent again.
        s.driver_lost();
        assert_eq!(s.decide(4, F48), Step::Send(F96));
        // A driver that came back with the default already set: quiet.
        s.driver_lost();
        assert_eq!(s.decide(4, F96), Step::Nothing);

        // Failed, then the user goes to 48 kHz (already the default) and
        // back to 96: a new choice, sent again.
        let mut s = FormatSync::new();
        s.report(Some(F96));
        assert_eq!(s.decide(4, F48), Step::Send(F96));
        assert_eq!(s.decide(4, F48), Step::Nothing);
        s.report(Some(F48));
        assert_eq!(s.decide(4, F48), Step::AlreadySet(F48));
        s.report(Some(F96));
        assert_eq!(s.decide(4, F48), Step::Send(F96));
        assert_eq!(s.decide(4, F48), Step::Nothing);
    }

    #[test]
    fn old_drivers_unknown_defaults_and_formats_not_offered() {
        let mut s = FormatSync::new();
        s.report(Some(F96));
        assert_eq!(s.decide(3, PcmFormat::default()), Step::OldDriver(F96));
        assert_eq!(
            s.decide(3, PcmFormat::default()),
            Step::Nothing,
            "said once"
        );
        s.report(Some(F48));
        assert_eq!(s.decide(3, PcmFormat::default()), Step::Nothing);
        s.driver_lost();
        assert_eq!(
            s.decide(3, PcmFormat::default()),
            Step::Nothing,
            "48k/24 is what it plays"
        );
        s.report(Some(F16));
        assert_eq!(s.decide(3, PcmFormat::default()), Step::OldDriver(F16));
        // Windows already at what a v3 driver plays: never a word, and a
        // later change to another format is still said.
        let mut s = FormatSync::new();
        s.report(Some(F48));
        for _ in 0..3 {
            assert_eq!(s.decide(3, PcmFormat::default()), Step::Nothing);
        }
        s.driver_lost();
        assert_eq!(s.decide(3, PcmFormat::default()), Step::Nothing);
        s.report(Some(F96));
        assert_eq!(s.decide(3, PcmFormat::default()), Step::OldDriver(F96));
        // A rate no driver offers: the same note as for a v4 driver.
        let f44 = PcmFormat {
            sample_rate: 44_100,
            ..F48
        };
        s.report(Some(f44));
        assert_eq!(s.decide(3, PcmFormat::default()), Step::NotOffered(f44));
        assert_eq!(s.decide(3, PcmFormat::default()), Step::Nothing);
        // A v4 driver that reports no default: nothing is sent.
        let mut s = FormatSync::new();
        s.report(Some(F96));
        assert_eq!(s.decide(4, PcmFormat::default()), Step::Nothing);
        // A rate the endpoint does not offer: said, nothing sent.
        let f44 = PcmFormat {
            sample_rate: 44_100,
            ..F48
        };
        s.report(Some(f44));
        assert_eq!(s.decide(4, F48), Step::NotOffered(f44));
        assert_eq!(s.decide(4, F48), Step::Nothing);
        // The endpoint gone: nothing either.
        s.report(None);
        assert_eq!(s.decide(4, F48), Step::Nothing);
        assert_eq!(s.windows(), None);
    }

    #[test]
    fn says_once_when_the_engine_keeps_the_old_format() {
        let ms = Duration::from_millis;
        let mut s = FormatSync::new();
        // Nothing set: nothing to wait for.
        assert!(!s.check(ms(0), StreamState::Running, F48));
        s.applied(F96);
        // Paused or no stream: no clock running.
        assert!(!s.check(ms(0), StreamState::Paused, F48));
        assert_eq!(s.deadline(), None);
        assert!(!s.check(ms(1000), StreamState::Running, F48));
        assert_eq!(s.deadline(), Some(ms(6000)));
        assert!(!s.check(ms(5999), StreamState::Running, F48));
        // A pause in between starts it over.
        assert!(!s.check(ms(3000), StreamState::Paused, F48));
        assert!(!s.check(ms(4000), StreamState::Running, F48));
        assert!(!s.check(ms(8999), StreamState::Running, F48));
        assert!(s.check(ms(9000), StreamState::Running, F48));
        assert!(!s.check(ms(20_000), StreamState::Running, F48), "once");
        assert_eq!(s.deadline(), None);
        // The stream follows after all: done.
        assert!(!s.check(ms(21_000), StreamState::Running, F96));
        assert!(!s.check(ms(40_000), StreamState::Running, F48));
        // Following at once: never a word.
        s.applied(F48);
        assert!(!s.check(ms(0), StreamState::Running, F48));
        assert!(!s.check(ms(10_000), StreamState::Running, F96));
        assert_eq!(s.deadline(), None);
    }
}
