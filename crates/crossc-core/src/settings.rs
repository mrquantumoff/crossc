//! Service settings from `HKLM\SOFTWARE\crossc`. Every value is optional; a
//! missing or invalid one falls back to its default (invalid ones with a
//! warning for the log).
//!
//! | Value | Type | Default | Meaning |
//! |---|---|---|---|
//! | `UhqConfig` | DWORD | `0x0A` | SSC configuration octet sent for UHQ. Must contain `0x02` and only bits of `0x0E` (`0x0A`, or `0x0E` as another implementation sends). |
//! | `HifiBitrate` | DWORD | 229 | HiFi bitrate in kbps, the top of the adaptive ladder: 88, 96, 128, 192 or 229 (what the buds' decoder takes at 48 kHz). 256 and 328 are lowered to 229. |
//! | `UhqBitrate` | DWORD | 584 | UHQ bitrate in kbps, the top of the adaptive ladder: 152, 291 or 584 (the UHQ rates the buds decode). Another UHQ rate is lowered to the next of these. |
//! | `AdaptiveBitrate` | DWORD | 1 | 1: the bitrate follows the link ([`crate::abr`]), never above the two values before. 0: always those (after the MTU fit, which steps down the rates the buds decode: 584 to 291 kbps, where 0.2.0 took 442). |
//! | `LogLevel` | DWORD or string | 2 | 0 error, 1 warn, 2 info, 3 debug. |
//!
//! DWORD values may also be given as strings (`"0x0A"`, `"192"`).

use avdtp::codec::SscFlags;
use ssc_enc::{Bitrate, Mode};

use crate::abr::decodable;
use crate::log::Level;

/// Registry key under HKEY_LOCAL_MACHINE.
pub const KEY: &str = r"SOFTWARE\crossc";

pub const VALUE_UHQ_CONFIG: &str = "UhqConfig";
pub const VALUE_HIFI_BITRATE: &str = "HifiBitrate";
pub const VALUE_UHQ_BITRATE: &str = "UhqBitrate";
pub const VALUE_ADAPTIVE: &str = "AdaptiveBitrate";
pub const VALUE_LOG_LEVEL: &str = "LogLevel";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub uhq_config: u8,
    pub hifi_bitrate: Bitrate,
    pub uhq_bitrate: Bitrate,
    /// The bitrate follows the link, with the two above as the top.
    pub adaptive: bool,
    pub log_level: Level,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            uhq_config: SscFlags::CONFIG_UHQ_V2,
            hifi_bitrate: Bitrate::K229,
            uhq_bitrate: Bitrate::K584,
            adaptive: true,
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

fn uhq_config_ok(v: u32) -> bool {
    let allowed = u32::from(
        SscFlags::UHQ_V2_96K_24 | SscFlags::HIFI_48K_24 | SscFlags::NO_BITRATE_LIMIT_24BIT,
    );
    v & u32::from(SscFlags::UHQ_V2_96K_24) != 0 && v & !allowed == 0
}

/// What a bitrate setting asks for.
enum Asked {
    /// A rate the buds are known to decode in the mode (which also means
    /// that a one-frame packet fits the media MTU they are known to take).
    Rate(Bitrate),
    /// An SSC rate of the mode's family that they are not known to decode:
    /// the next one below it that they are. Never one above what was asked
    /// for.
    Lowered(Bitrate),
    /// Not a rate of the mode at all.
    Bad,
}

fn bitrate(v: u32, mode: Mode) -> Asked {
    let Some(b) = v.checked_mul(1000).and_then(Bitrate::from_bps) else {
        return Asked::Bad;
    };
    if b.is_e_family() != mode.is_96k() {
        return Asked::Bad;
    }
    let rates = decodable(mode);
    if rates.contains(&b) {
        return Asked::Rate(b);
    }
    match rates.iter().find(|d| d.bps() < b.bps()) {
        Some(d) => Asked::Lowered(*d),
        None => Asked::Bad,
    }
}

impl Settings {
    /// Build the settings from raw values (`get` returns `None` for a value
    /// that does not exist). Returns the settings and one warning per value
    /// that was present but not usable.
    pub fn from_values(get: impl Fn(&str) -> Option<RegValue>) -> (Settings, Vec<String>) {
        let mut s = Settings::default();
        let mut warnings = Vec::new();
        let mut lowered = Vec::new();
        let mut lower = |name: &str, v: &RegValue, to: Bitrate| {
            lowered.push(format!(
                "setting {name} = {} is not a rate the buds are known to decode; using {} kbps, the next one below",
                v.show(),
                to.bps() / 1000
            ));
            to
        };
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
            match v.number().map_or(Asked::Bad, |n| bitrate(n, Mode::Hifi48k)) {
                Asked::Rate(b) => s.hifi_bitrate = b,
                Asked::Lowered(b) => s.hifi_bitrate = lower(VALUE_HIFI_BITRATE, &v, b),
                Asked::Bad => bad(
                    VALUE_HIFI_BITRATE,
                    &v,
                    "kbps: 88, 96, 128, 192 or 229; the buds decode no more at 48 kHz",
                ),
            }
        }
        if let Some(v) = get(VALUE_UHQ_BITRATE) {
            match v.number().map_or(Asked::Bad, |n| bitrate(n, Mode::Uhq96k)) {
                Asked::Rate(b) => s.uhq_bitrate = b,
                Asked::Lowered(b) => s.uhq_bitrate = lower(VALUE_UHQ_BITRATE, &v, b),
                Asked::Bad => bad(
                    VALUE_UHQ_BITRATE,
                    &v,
                    "kbps: 152, 291 or 584, the UHQ rates the buds decode",
                ),
            }
        }
        if let Some(v) = get(VALUE_ADAPTIVE) {
            match v.number() {
                Some(0) => s.adaptive = false,
                Some(1) => s.adaptive = true,
                _ => bad(VALUE_ADAPTIVE, &v, "0 off, 1 on"),
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
        warnings.append(&mut lowered);
        (s, warnings)
    }

    /// One line for the log.
    pub fn describe(&self) -> String {
        format!(
            "UhqConfig={:#04x} HifiBitrate={} UhqBitrate={} AdaptiveBitrate={} LogLevel={}",
            self.uhq_config,
            self.hifi_bitrate.bps() / 1000,
            self.uhq_bitrate.bps() / 1000,
            u8::from(self.adaptive),
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
            (0x0A, Bitrate::K229, Bitrate::K584, Level::Info)
        );
        assert!(st.adaptive);
        assert_eq!(
            st.describe(),
            "UhqConfig=0x0a HifiBitrate=229 UhqBitrate=584 AdaptiveBitrate=1 LogLevel=info"
        );
    }

    #[test]
    fn valid_values_are_taken() {
        let (st, w) = from(&[
            (VALUE_UHQ_CONFIG, d(0x0E)),
            (VALUE_HIFI_BITRATE, d(229)),
            (VALUE_UHQ_BITRATE, d(291)),
            (VALUE_ADAPTIVE, d(0)),
            (VALUE_LOG_LEVEL, d(3)),
        ]);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(
            st,
            Settings {
                uhq_config: 0x0E,
                hifi_bitrate: Bitrate::K229,
                uhq_bitrate: Bitrate::K291,
                adaptive: false,
                log_level: Level::Debug,
            }
        );
        assert!(st.describe().contains("AdaptiveBitrate=0"));
        // Strings work for every value.
        let (st, w) = from(&[
            (VALUE_UHQ_CONFIG, s("0x0A")),
            (VALUE_HIFI_BITRATE, s(" 128 ")),
            (VALUE_UHQ_BITRATE, s("152")),
            (VALUE_ADAPTIVE, s("0")),
            (VALUE_LOG_LEVEL, s("Warn")),
        ]);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(
            (st.uhq_config, st.hifi_bitrate, st.uhq_bitrate, st.log_level),
            (0x0A, Bitrate::K128, Bitrate::K152, Level::Warn)
        );
        assert!(!st.adaptive);
        assert!(from(&[(VALUE_ADAPTIVE, s("1"))]).0.adaptive);
    }

    #[test]
    fn every_accepted_bitrate() {
        for k in [88, 96, 128, 192, 229] {
            let (st, w) = from(&[(VALUE_HIFI_BITRATE, d(k))]);
            assert!(w.is_empty(), "{k}");
            assert_eq!(st.hifi_bitrate.bps(), k * 1000);
        }
        for k in [152, 291, 584] {
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
            // Not an SSC rate, the other family, overflow.
            (VALUE_HIFI_BITRATE, d(200)),
            (VALUE_HIFI_BITRATE, d(584)),
            (VALUE_ADAPTIVE, d(2)),
            (VALUE_ADAPTIVE, s("yes")),
            (VALUE_HIFI_BITRATE, d(0)),
            (VALUE_HIFI_BITRATE, d(u32::MAX)),
            (VALUE_UHQ_BITRATE, d(192)),
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
        let (st, w) = from(&[(VALUE_HIFI_BITRATE, d(1)), (VALUE_UHQ_BITRATE, d(291))]);
        assert_eq!(w.len(), 1);
        assert_eq!(
            (st.hifi_bitrate, st.uhq_bitrate),
            (Bitrate::K229, Bitrate::K291)
        );
    }

    #[test]
    fn rates_the_buds_do_not_decode_are_lowered_never_raised() {
        // (value, asked for, used): never more than what was asked for,
        // which a fall-back to the default (229, 584) would be.
        for (name, asked, used) in [
            (VALUE_HIFI_BITRATE, 256, 229),
            (VALUE_HIFI_BITRATE, 328, 229),
            (VALUE_UHQ_BITRATE, 250, 152),
            (VALUE_UHQ_BITRATE, 308, 291),
            (VALUE_UHQ_BITRATE, 442, 291),
            (VALUE_UHQ_BITRATE, 886, 584),
        ] {
            let (st, w) = from(&[(name, d(asked))]);
            let got = if name == VALUE_HIFI_BITRATE {
                st.hifi_bitrate
            } else {
                st.uhq_bitrate
            };
            assert_eq!(got.bps(), used * 1000, "{name} {asked}");
            assert!(got.bps() <= asked * 1000);
            assert_eq!(w.len(), 1, "{name} {asked}");
            assert!(
                w[0].contains(name) && w[0].contains(&format!("using {used} kbps")),
                "{}",
                w[0]
            );
        }
        // Whatever is set, the result is a rate the buds decode.
        for v in 0..1000 {
            let (st, _) = from(&[(VALUE_HIFI_BITRATE, d(v)), (VALUE_UHQ_BITRATE, d(v))]);
            assert!(decodable(Mode::Hifi48k).contains(&st.hifi_bitrate), "{v}");
            assert!(decodable(Mode::Uhq96k).contains(&st.uhq_bitrate), "{v}");
        }
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
