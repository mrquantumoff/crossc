//! Shutdown request shared between whoever asks (console Ctrl+C, the service
//! control handler) and the threads that must wind down.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

struct Inner {
    flag: AtomicBool,
    lock: Mutex<()>,
    cond: Condvar,
    #[cfg(windows)]
    event: Option<win::Event>,
}

/// A one-way "stop now" flag. Cheap to clone; all clones share the flag.
#[derive(Clone)]
pub struct StopSignal(Arc<Inner>);

impl Default for StopSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl StopSignal {
    pub fn new() -> Self {
        StopSignal(Arc::new(Inner {
            flag: AtomicBool::new(false),
            lock: Mutex::new(()),
            cond: Condvar::new(),
            #[cfg(windows)]
            event: win::Event::new(),
        }))
    }

    /// Ask for the stop. Safe from any thread, any number of times.
    pub fn request(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        #[cfg(windows)]
        if let Some(e) = &self.0.event {
            e.set();
        }
        // Take the lock so a waiter cannot miss the wake-up between its
        // check of the flag and its wait.
        let _g = self.0.lock.lock().unwrap_or_else(|p| p.into_inner());
        self.0.cond.notify_all();
    }

    pub fn is_set(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }

    /// Sleep up to `timeout`; returns true as soon as the stop is requested.
    pub fn wait(&self, timeout: Duration) -> bool {
        let g = self.0.lock.lock().unwrap_or_else(|p| p.into_inner());
        let r = self
            .0
            .cond
            .wait_timeout_while(g, timeout, |_| !self.is_set());
        drop(r);
        self.is_set()
    }

    /// Manual-reset event that is signalled once the stop is requested, for
    /// waits on several handles. `None` if the event could not be created
    /// (callers then poll [`Self::is_set`]).
    #[cfg(windows)]
    pub fn event(&self) -> Option<windows::Win32::Foundation::HANDLE> {
        self.0.event.as_ref().map(|e| e.0)
    }
}

#[cfg(windows)]
mod win {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{CreateEventW, SetEvent};
    use windows::core::PCWSTR;

    pub struct Event(pub HANDLE);

    // SAFETY: an event handle may be used from any thread.
    unsafe impl Send for Event {}
    unsafe impl Sync for Event {}

    impl Event {
        pub fn new() -> Option<Self> {
            unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
                .ok()
                .map(Event)
        }
        pub fn set(&self) {
            let _ = unsafe { SetEvent(self.0) };
        }
    }

    impl Drop for Event {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn wait_returns_early_on_request() {
        let s = StopSignal::new();
        assert!(!s.is_set());
        assert!(!s.wait(Duration::from_millis(5)), "times out while not set");
        let s2 = s.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            s2.request();
        });
        let t0 = Instant::now();
        assert!(s.wait(Duration::from_secs(20)));
        assert!(t0.elapsed() < Duration::from_secs(10));
        t.join().unwrap();
        assert!(s.is_set());
        assert!(s.wait(Duration::from_secs(20)), "returns at once when set");
        s.request();
    }

    #[cfg(windows)]
    #[test]
    fn event_is_signalled() {
        use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::WaitForSingleObject;
        let s = StopSignal::new();
        let e = s.event().unwrap();
        assert_eq!(unsafe { WaitForSingleObject(e, 0) }, WAIT_TIMEOUT);
        s.request();
        assert_eq!(unsafe { WaitForSingleObject(e, 0) }, WAIT_OBJECT_0);
    }
}
