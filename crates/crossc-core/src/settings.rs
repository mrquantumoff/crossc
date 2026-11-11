//! Service settings from `HKLM\SOFTWARE\crossc`. Every value is optional; a
//! missing or invalid one falls back to its default (invalid ones with a
//! warning for the log).
//!
//! | Value | Type | Default | Meaning |
//! |---|---|---|---|
//! | `UhqConfig` | DWORD | `0x0A` | SSC configuration octet sent for UHQ. Must contain `0x02` and only bits of `0x0E` (`0x0A`, or `0x0E` as another implementation sends). |
//! | `HifiBitrate` | DWORD | 192 | HiFi bitrate in kbps: 88, 96, 128, 192, 229, 256 or 328. |
//! | `UhqBitrate` | DWORD | 584 | UHQ bitrate in kbps: 152, 250, 291, 308, 442 or 584. |
//! | `LogLevel` | DWORD or string | 2 | 0 error, 1 warn, 2 info, 3 debug. |
//!
//! DWORD values may also be given as strings (`"0x0A"`, `"192"`).

use avdtp::codec::SscFlags;
use avdtp::rtp::RtpHeader;
use ssc_enc::{Bitrate, Mode};

use crate::log::Level;
use crate::rtp::KNOWN_BUDS_MEDIA_MTU;

/// Registry key under HKEY_LOCAL_MACHINE.
pub const KEY: &str = r"SOFTWARE\crossc";

pub const VALUE_UHQ_CONFIG: &str = "UhqConfig";
pub const VALUE_HIFI_BITRATE: &str = "HifiBitrate";
pub const VALUE_UHQ_BITRATE: &str = "UhqBitrate";
pub const VALUE_LOG_LEVEL: &str = "LogLevel";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub uhq_config: u8,
    pub hifi_bitrate: Bitrate,
    pub uhq_bitrate: Bitrate,
    pub log_level: Level,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            uhq_config: SscFlags::CONFIG_UHQ_V2,
            hifi_bitrate: Bitrate::K192,
            uhq_bitrate: Bitrate::K584,
            log_level: Level::Info,
        }
    }
}

/// A registry value as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegValue {
    Dword(u32),
    Str(String),
}

impl RegValue {
    fn number(&self) -> Option<u32> {
        match self {
            RegValue::Dword(v) => Some(*v),
            RegValue::Str(s) => crate::hex::parse_num(s.trim()).ok(),
        }
    }
    fn show(&self) -> String {
        match self {
            RegValue::Dword(v) => format!("{v} ({v:#x})"),
            RegValue::Str(s) => format!("{s:?}"),
        }
    }
}

/// A one-frame media packet at this rate fits the media MTU the buds are
/// known to take.
fn fits(mode: Mode, b: Bitrate) -> bool {
    RtpHeader::LEN + ssc_enc::frame_size(mode, b) <= KNOWN_BUDS_MEDIA_MTU
}

fn uhq_config_ok(v: u32) -> bool {
    let allowed = u32::from(
        SscFlags::UHQ_V2_96K_24 | SscFlags::HIFI_48K_24 | SscFlags::NO_BITRATE_LIMIT_24BIT,
    );
    v & u32::from(SscFlags::UHQ_V2_96K_24) != 0 && v & !allowed == 0
}

fn bitrate(v: u32, mode: Mode, e_family: bool) -> Option<Bitrate> {
    let b = Bitrate::from_bps(v.checked_mul(1000)?)?;
    (b.is_e_family() == e_family && fits(mode, b)).then_some(b)
}

impl Settings {
    /// Build the settings from raw values (`get` returns `None` for a value
    /// that does not exist). Returns the settings and one warning per value
    /// that was present but not usable.
    pub fn from_values(get: impl Fn(&str) -> Option<RegValue>) -> (Settings, Vec<String>) {
        let mut s = Settings::default();
        let mut warnings = Vec::new();
        let mut bad = |name: &str, v: &RegValue, want: &str| {
            warnings.push(format!(
                "setting {name} = {} is not valid ({want}); using the default",
                v.show()
            ));
        };
        if let Some(v) = get(VALUE_UHQ_CONFIG) {
            match v.number().filter(|&n| uhq_config_ok(n)) {
                Some(n) => s.uhq_config = n as u8,
                None => bad(
                    VALUE_UHQ_CONFIG,
                    &v,
                    "must contain 0x02 and only bits of 0x0E, e.g. 0x0A or 0x0E",
                ),
            }
        }
        if let Some(v) = get(VALUE_HIFI_BITRATE) {
            match v.number().and_then(|n| bitrate(n, Mode::Hifi48k, false)) {
                Some(b) => s.hifi_bitrate = b,
                None => bad(
                    VALUE_HIFI_BITRATE,
                    &v,
                    "kbps: 88, 96, 128, 192, 229, 256 or 328",
                ),
            }
        }
        if let Some(v) = get(VALUE_UHQ_BITRATE) {
            match v.number().and_then(|n| bitrate(n, Mode::Uhq96k, true)) {
                Some(b) => s.uhq_bitrate = b,
                None => bad(
                    VALUE_UHQ_BITRATE,
                    &v,
                    "kbps: 152, 250, 291, 308, 442 or 584",
                ),
            }
        }
        if let Some(v) = get(VALUE_LOG_LEVEL) {
            let level = match &v {
                RegValue::Dword(n) => Level::parse(&n.to_string()),
                RegValue::Str(t) => Level::parse(t),
            };
            match level {
                Some(l) => s.log_level = l,
                None => bad(VALUE_LOG_LEVEL, &v, "0 error, 1 warn, 2 info, 3 debug"),
            }
        }
        (s, warnings)
    }

    /// One line for the log.
    pub fn describe(&self) -> String {
        format!(
            "UhqConfig={:#04x} HifiBitrate={} UhqBitrate={} LogLevel={}",
            self.uhq_config,
            self.hifi_bitrate.bps() / 1000,
            self.uhq_bitrate.bps() / 1000,
            self.log_level.name().to_ascii_lowercase()
        )
    }

    /// Read `HKLM\SOFTWARE\crossc` (64-bit view). A missing key gives the
    /// defaults.
    #[cfg(windows)]
    pub fn load() -> (Settings, Vec<String>) {
        Settings::from_values(win::read)
    }
}

#[cfg(windows)]
mod win {
    use super::{KEY, RegValue};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, REG_DWORD, REG_EXPAND_SZ, REG_SZ, REG_VALUE_TYPE, RRF_RT_REG_DWORD,
        RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
    };
    use windows::core::HSTRING;

    pub fn read(name: &str) -> Option<RegValue> {
        // Large enough for any value this module accepts.
        let mut buf = [0u16; 128];
        let mut size = std::mem::size_of_val(&buf) as u32;
        let mut ty = REG_VALUE_TYPE(0);
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                &HSTRING::from(KEY),
                &HSTRING::from(name),
                RRF_RT_REG_DWORD | RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
                Some(&mut ty),
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if r != ERROR_SUCCESS {
            return None;
        }
        if ty == REG_DWORD {
            Some(RegValue::Dword(
                u32::from(buf[0]) | (u32::from(buf[1]) << 16),
            ))
        } else if ty == REG_SZ || ty == REG_EXPAND_SZ {
            let chars = (size as usize / 2).min(buf.len());
            let text = &buf[..chars];
            let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
            Some(RegValue::Str(String::from_utf16_lossy(&text[..end])))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn from(vals: &[(&str, RegValue)]) -> (Settings, Vec<String>) {
        let m: HashMap<String, RegValue> = vals
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        Settings::from_values(|k| m.get(k).cloned())
    }

    fn d(v: u32) -> RegValue {
        RegValue::Dword(v)
    }
    fn s(v: &str) -> RegValue {
        RegValue::Str(v.to_string())
    }

    #[test]
    fn defaults_when_nothing_is_set() {
        let (st, w) = from(&[]);
        assert_eq!(st, Settings::default());
        assert!(w.is_empty());
        assert_eq!(
            (st.uhq_config, st.hifi_bitrate, st.uhq_bitrate, st.log_level),
            (0x0A, Bitrate::K192, Bitrate::K584, Level::Info)
        );
        assert_eq!(
            st.describe(),
            "UhqConfig=0x0a HifiBitrate=192 UhqBitrate=584 LogLevel=info"
        );
    }

    #[test]
    fn valid_values_are_taken() {
        let (st, w) = from(&[
            (VALUE_UHQ_CONFIG, d(0x0E)),
            (VALUE_HIFI_BITRATE, d(229)),
            (VALUE_UHQ_BITRATE, d(291)),
            (VALUE_LOG_LEVEL, d(3)),
        ]);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(
            st,
            Settings {
                uhq_config: 0x0E,
                hifi_bitrate: Bitrate::K229,
                uhq_bitrate: Bitrate::K291,
                log_level: Level::Debug,
            }
        );
        // Strings work for every value.
        let (st, w) = from(&[
            (VALUE_UHQ_CONFIG, s("0x0A")),
            (VALUE_HIFI_BITRATE, s(" 128 ")),
            (VALUE_UHQ_BITRATE, s("152")),
            (VALUE_LOG_LEVEL, s("Warn")),
        ]);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(
            (st.uhq_config, st.hifi_bitrate, st.uhq_bitrate, st.log_level),
            (0x0A, Bitrate::K128, Bitrate::K152, Level::Warn)
        );
    }

    #[test]
    fn every_accepted_bitrate() {
        for k in [88, 96, 128, 192, 229, 256, 328] {
            let (st, w) = from(&[(VALUE_HIFI_BITRATE, d(k))]);
            assert!(w.is_empty(), "{k}");
            assert_eq!(st.hifi_bitrate.bps(), k * 1000);
        }
        for k in [152, 250, 291, 308, 442, 584] {
            let (st, w) = from(&[(VALUE_UHQ_BITRATE, d(k))]);
            assert!(w.is_empty(), "{k}");
            assert_eq!(st.uhq_bitrate.bps(), k * 1000);
        }
    }

    #[test]
    fn invalid_values_fall_back_with_a_warning() {
        let cases: &[(&str, RegValue)] = &[
            // UHQ configuration without the UHQ bit, with the sink-only bit,
            // with a sample-rate bit, out of range, or not a number.
            (VALUE_UHQ_CONFIG, d(0x0C)),
            (VALUE_UHQ_CONFIG, d(0x0B)),
            (VALUE_UHQ_CONFIG, d(0x1A)),
            (VALUE_UHQ_CONFIG, d(0x10A)),
            (VALUE_UHQ_CONFIG, s("uhq")),
            // Not an SSC rate, the other family, too big for a packet, overflow.
            (VALUE_HIFI_BITRATE, d(200)),
            (VALUE_HIFI_BITRATE, d(584)),
            (VALUE_HIFI_BITRATE, d(0)),
            (VALUE_HIFI_BITRATE, d(u32::MAX)),
            (VALUE_UHQ_BITRATE, d(192)),
            (VALUE_UHQ_BITRATE, d(886)),
            (VALUE_UHQ_BITRATE, s("fast")),
            (VALUE_LOG_LEVEL, d(4)),
            (VALUE_LOG_LEVEL, s("chatty")),
        ];
        for (name, v) in cases {
            let (st, w) = from(&[(name, v.clone())]);
            assert_eq!(st, Settings::default(), "{name} {v:?}");
            assert_eq!(w.len(), 1, "{name} {v:?}");
            assert!(
                w[0].contains(name) && w[0].contains("using the default"),
                "{}",
                w[0]
            );
        }
        // One bad value does not spoil the others.
        let (st, w) = from(&[(VALUE_HIFI_BITRATE, d(1)), (VALUE_UHQ_BITRATE, d(442))]);
        assert_eq!(w.len(), 1);
        assert_eq!(
            (st.hifi_bitrate, st.uhq_bitrate),
            (Bitrate::K192, Bitrate::K442)
        );
    }

    #[test]
    fn uhq_config_rule() {
        let ok: Vec<u32> = (0..=0x1FF).filter(|&v| uhq_config_ok(v)).collect();
        assert_eq!(ok, vec![0x02, 0x06, 0x0A, 0x0E]);
    }

    #[cfg(windows)]
    #[test]
    fn loading_never_fails() {
        // Whatever is (or is not) in the registry of this machine, loading
        // yields usable settings.
        let (st, _) = Settings::load();
        assert!(uhq_config_ok(u32::from(st.uhq_config)));
        assert!(st.uhq_bitrate.is_e_family() && !st.hifi_bitrate.is_e_family());
        assert_eq!(win::read("NoSuchValueForSure"), None);
    }
}
