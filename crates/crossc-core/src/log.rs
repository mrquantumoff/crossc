//! Logging seam: the session reports through [`Log`]; the service writes it
//! to a file, a foreground run to stdout, tests to memory.

use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
        }
    }
    /// `0`..`3` or `error` / `warn` / `info` / `debug` (any case).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "0" | "error" => Level::Error,
            "1" | "warn" | "warning" => Level::Warn,
            "2" | "info" => Level::Info,
            "3" | "debug" => Level::Debug,
            _ => return None,
        })
    }
}

pub trait Log: Send + Sync {
    fn log(&self, level: Level, msg: &str);
    /// Whether `level` is written at all (lets callers skip building
    /// expensive messages).
    fn enabled(&self, _level: Level) -> bool {
        true
    }
}

/// Discards everything.
pub struct NullLog;

impl Log for NullLog {
    fn log(&self, _: Level, _: &str) {}
    fn enabled(&self, _: Level) -> bool {
        false
    }
}

/// Lets a message with a given key through at most once per period, and
/// counts what it held back, so a failure that repeats every few seconds
/// is logged once with a repeat count instead of flooding the log.
pub struct RateLimit {
    period: Duration,
    seen: HashMap<String, (Duration, u32)>,
}

impl RateLimit {
    pub fn new(period: Duration) -> Self {
        RateLimit {
            period,
            seen: HashMap::new(),
        }
    }

    /// `Some(n)` if a message with `key` may be logged at `now`, `n` being
    /// how many were held back since the last one; `None` to hold it back.
    pub fn pass(&mut self, key: &str, now: Duration) -> Option<u32> {
        match self.seen.get_mut(key) {
            Some((last, held)) if now.saturating_sub(*last) < self.period => {
                *held += 1;
                None
            }
            Some((last, held)) => {
                let n = *held;
                *last = now;
                *held = 0;
                Some(n)
            }
            None => {
                // Bounded: keys are a small fixed set of message kinds, but
                // never let a bug grow this without limit.
                if self.seen.len() >= 256 {
                    self.seen.clear();
                }
                self.seen.insert(key.to_string(), (now, 0));
                Some(0)
            }
        }
    }

    /// Forget `key`, so its next message is logged at once.
    pub fn forget(&mut self, key: &str) {
        self.seen.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert!(Level::Error < Level::Warn && Level::Info < Level::Debug);
        assert_eq!(Level::parse("2"), Some(Level::Info));
        assert_eq!(Level::parse(" Debug "), Some(Level::Debug));
        assert_eq!(Level::parse("WARNING"), Some(Level::Warn));
        assert_eq!(Level::parse("4"), None);
        assert_eq!(Level::Error.name(), "ERROR");
    }

    #[test]
    fn rate_limit_passes_once_per_period_and_counts() {
        let s = Duration::from_secs;
        let mut r = RateLimit::new(s(60));
        assert_eq!(r.pass("a", s(0)), Some(0));
        assert_eq!(r.pass("a", s(10)), None);
        assert_eq!(r.pass("a", s(59)), None);
        assert_eq!(r.pass("b", s(59)), Some(0), "keys are independent");
        assert_eq!(r.pass("a", s(60)), Some(2));
        assert_eq!(r.pass("a", s(61)), None);
        r.forget("a");
        assert_eq!(r.pass("a", s(62)), Some(0));
    }
}
