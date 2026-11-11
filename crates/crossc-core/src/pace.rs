//! Clocks for `play`'s packet pacing. Deadlines are always computed from the
//! stream start (`k * packet_duration`), never by adding sleeps, so the
//! schedule cannot drift; the clock only has to sleep until an absolute time.

use std::time::{Duration, Instant};

pub trait Clock {
    /// Time since the clock was created.
    fn now(&self) -> Duration;
    /// Block until `now() >= t` (returns at once if already past).
    fn sleep_until(&mut self, t: Duration);
    fn describe(&self) -> &'static str;
}

/// Virtual time: `sleep_until` jumps forward. Used by tests and `--dry-run`
/// (which therefore runs as fast as the encoder can go).
#[derive(Default)]
pub struct VirtualClock {
    pub t: Duration,
}

impl Clock for VirtualClock {
    fn now(&self) -> Duration {
        self.t
    }
    fn sleep_until(&mut self, t: Duration) {
        self.t = self.t.max(t);
    }
    fn describe(&self) -> &'static str {
        "virtual (no pacing)"
    }
}

/// Wall clock. On Windows sleeps on a high-resolution waitable timer
/// (CreateWaitableTimerExW + CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, Windows
/// 10 1803+), waking ~0.3 ms early and spinning the rest; elsewhere, or if
/// the timer cannot be created, `thread::sleep` to ~2 ms before the deadline
/// and spin.
pub struct WallClock {
    start: Instant,
    #[cfg(windows)]
    timer: Option<win::Timer>,
}

impl WallClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            #[cfg(windows)]
            timer: win::Timer::new(),
        }
    }
}

impl Default for WallClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for WallClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }
    fn sleep_until(&mut self, t: Duration) {
        let left = t.saturating_sub(self.now());
        #[cfg(windows)]
        let coarse = if self.timer.is_some() {
            Duration::from_micros(300)
        } else {
            Duration::from_millis(2)
        };
        #[cfg(not(windows))]
        let coarse = Duration::from_millis(2);
        if left > coarse {
            let d = left - coarse;
            #[cfg(windows)]
            match &self.timer {
                Some(tm) if tm.wait(d) => {}
                _ => std::thread::sleep(d),
            }
            #[cfg(not(windows))]
            std::thread::sleep(d);
        }
        while self.now() < t {
            std::hint::spin_loop();
        }
    }
    fn describe(&self) -> &'static str {
        #[cfg(windows)]
        if self.timer.is_some() {
            return "high-resolution waitable timer + spin";
        }
        "sleep + spin"
    }
}

#[cfg(windows)]
mod win {
    use std::time::Duration;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
        WaitForSingleObject,
    };
    use windows::core::PCWSTR;

    /// SYNCHRONIZE | TIMER_MODIFY_STATE.
    const TIMER_ACCESS: u32 = 0x0010_0000 | 0x0002;

    pub struct Timer(HANDLE);

    impl Timer {
        pub fn new() -> Option<Self> {
            let h = unsafe {
                CreateWaitableTimerExW(
                    None,
                    PCWSTR::null(),
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ACCESS,
                )
            }
            .ok()?;
            Some(Self(h))
        }
        /// Relative wait; false if the timer could not be armed.
        pub fn wait(&self, d: Duration) -> bool {
            // Negative = relative, in 100 ns units.
            let due = -i64::try_from(d.as_nanos() / 100).unwrap_or(i64::MAX).max(1);
            if unsafe { SetWaitableTimer(self.0, &due, 0, None, None, false) }.is_err() {
                return false;
            }
            unsafe { WaitForSingleObject(self.0, u32::MAX) == WAIT_OBJECT_0 }
        }
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_clock_jumps() {
        let mut c = VirtualClock::default();
        c.sleep_until(Duration::from_millis(5));
        assert_eq!(c.now(), Duration::from_millis(5));
        c.sleep_until(Duration::from_millis(1));
        assert_eq!(c.now(), Duration::from_millis(5));
    }

    #[test]
    fn wall_clock_hits_deadlines_without_drift() {
        let mut c = WallClock::new();
        let step = Duration::from_micros(2500);
        let mut worst = Duration::ZERO;
        for k in 1..=20u32 {
            let t = step * k;
            c.sleep_until(t);
            let now = c.now();
            assert!(now >= t);
            worst = worst.max(now - t);
        }
        // Generous: CI machines can be busy. Typically a few microseconds.
        assert!(
            worst < Duration::from_millis(20),
            "{worst:?} ({})",
            c.describe()
        );
    }
}
