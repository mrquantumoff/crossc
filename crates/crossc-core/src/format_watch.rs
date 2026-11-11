//! The Windows device format of one crossc device's endpoint, watched in
//! the registry ([`crate::winformat`] says where and why there).
//!
//! A helper thread does the registry work, so the session thread, which
//! paces the audio, never enumerates keys: it waits for a change
//! notification on [`RENDER_KEY`] (`RegNotifyChangeKeyValue`, whole subtree,
//! names and values), re-arms it, reads the endpoint's format again and,
//! only if the parsed format differs from the last one, publishes it and
//! signals the [`WakeEvent`]. The session's driver waits on that event
//! with everything else ([`crate::real::RealDriver::set_wake`]) and the
//! session picks the format up through [`FormatSource`]. Nothing polls:
//! the thread sleeps until the registry changes or it is told to stop.
//!
//! A notification is acted on at once (the sooner the driver has the new
//! default, the likelier the engine's next stream gets it). Notifications
//! come in bursts, though (Windows rewrites several properties of an
//! endpoint at once), so after reading the thread lets the registry settle
//! for [`SETTLE`] before it re-arms and reads again; a change in that time
//! is caught by that second reading. Once the endpoint is known only its
//! own values are read; the endpoint list is enumerated again only when
//! that endpoint is gone or no longer usable.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossc_ioctl::audio::PcmFormat;
use windows::Win32::Foundation::{
    ERROR_SUCCESS, GetLastError, HANDLE, WAIT_EVENT, WAIT_OBJECT_0, WIN32_ERROR,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_NOTIFY, KEY_READ, REG_NOTIFY_CHANGE_LAST_SET,
    REG_NOTIFY_CHANGE_NAME, REG_NOTIFY_THREAD_AGNOSTIC, REG_ROUTINE_FLAGS, RRF_RT_REG_BINARY,
    RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW,
    RegNotifyChangeKeyValue, RegOpenKeyExW,
};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use crate::device::OwnedHandle;
use crate::log::{Level, Log};
use crate::winformat::{
    DEVICE_FORMAT_VALUE, FormatSource, KS_PATH_VALUE, RENDER_KEY, is_crossc_endpoint,
    parse_device_format, short, state_rank,
};

/// Quiet time after a notification before the values are read.
pub const SETTLE: Duration = Duration::from_millis(250);
/// Between attempts when the render key cannot be opened or watched.
pub const RETRY: Duration = Duration::from_secs(30);
/// Endpoints looked at in one enumeration (a machine has a few dozen).
const MAX_ENDPOINTS: u32 = 1024;

/// A manual-reset event the watcher signals when the format changed. Whoever
/// waits on it resets it ([`Self::reset`]) before acting, so a wait never
/// spins on it.
#[derive(Clone)]
pub struct WakeEvent(Arc<OwnedHandle>);

impl WakeEvent {
    pub fn handle(&self) -> HANDLE {
        self.0.0
    }
    pub fn reset(&self) {
        let _ = unsafe { ResetEvent(self.0.0) };
    }
}

fn event(manual: bool) -> Result<OwnedHandle, String> {
    unsafe { CreateEventW(None, manual, false, PCWSTR::null()) }
        .map(OwnedHandle)
        .map_err(|e| format!("CreateEvent: {e}"))
}

struct Shared {
    /// Bumped with every published change.
    generation: AtomicU64,
    format: Mutex<Option<PcmFormat>>,
    wake: WakeEvent,
}

/// See the module docs. Stops and joins its thread when dropped.
pub struct FormatWatch {
    shared: Arc<Shared>,
    stop: Arc<OwnedHandle>,
    thread: Option<JoinHandle<()>>,
    seen: u64,
}

impl FormatWatch {
    /// Watch the endpoint of the crossc device interface `interface`.
    /// `name` prefixes the log lines.
    pub fn start(interface: &str, log: Arc<dyn Log>, name: &str) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            generation: AtomicU64::new(0),
            format: Mutex::new(None),
            wake: WakeEvent(Arc::new(event(true)?)),
        });
        let stop = Arc::new(event(true)?);
        let (iface, prefix) = (interface.to_string(), name.to_string());
        let (sh, st) = (shared.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name(format!("format-{name}"))
            .spawn(move || watch(&iface, &sh, &st, log.as_ref(), &prefix))
            .map_err(|e| format!("cannot start the format watcher: {e}"))?;
        Ok(FormatWatch {
            shared,
            stop,
            thread: Some(thread),
            // The first change is the first reading.
            seen: 0,
        })
    }

    /// The event to wait on.
    pub fn wake_event(&self) -> WakeEvent {
        self.shared.wake.clone()
    }
}

impl Drop for FormatWatch {
    fn drop(&mut self) {
        let _ = unsafe { SetEvent(self.stop.0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl FormatSource for FormatWatch {
    fn changed(&mut self) -> bool {
        let g = self.shared.generation.load(Ordering::Acquire);
        if g == self.seen {
            return false;
        }
        self.seen = g;
        true
    }
    fn current(&self) -> Option<PcmFormat> {
        *self.shared.format.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// An open registry key, closed on drop.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn open_render_key() -> Result<Key, WIN32_ERROR> {
    let mut h = HKEY::default();
    let r = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(RENDER_KEY),
            None,
            KEY_READ | KEY_NOTIFY,
            &mut h,
        )
    };
    if r == ERROR_SUCCESS {
        Ok(Key(h))
    } else {
        Err(r)
    }
}

/// `true`: stop was requested (within `timeout`).
fn stop_within(stop: &OwnedHandle, timeout: Duration) -> bool {
    let ms = timeout.as_millis().min(u128::from(INFINITE - 1)) as u32;
    unsafe { WaitForSingleObject(stop.0, ms) == WAIT_OBJECT_0 }
}

/// The thread.
fn watch(interface: &str, shared: &Shared, stop: &OwnedHandle, log: &dyn Log, name: &str) {
    let say = |level: Level, msg: &str| {
        if log.enabled(level) {
            log.log(level, &format!("[{name}] {msg}"));
        }
    };
    let Ok(changed) = event(false) else {
        say(
            Level::Error,
            "Windows device format: no event to watch the registry with",
        );
        return;
    };
    let mut key: Option<Key> = None;
    let mut known: Option<String> = None;
    let mut last: Option<Option<PcmFormat>> = None;
    let mut complained = false;
    loop {
        if key.is_none() {
            match open_render_key() {
                Ok(k) => key = Some(k),
                Err(e) => {
                    if !complained {
                        say(
                            Level::Error,
                            &format!(
                                "Windows device format: cannot open HKLM\\{RENDER_KEY} (Win32 error {}); retrying every {} s",
                                e.0,
                                RETRY.as_secs()
                            ),
                        );
                        complained = true;
                    }
                    if stop_within(stop, RETRY) {
                        return;
                    }
                    continue;
                }
            }
        }
        let Some(k) = key.as_ref() else { continue };
        // Armed before reading: a change while we read signals at once.
        let armed = unsafe {
            RegNotifyChangeKeyValue(
                k.0,
                true,
                REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_THREAD_AGNOSTIC,
                Some(changed.0),
                true,
            )
        };
        let now = read_format(k, interface, &mut known);
        if last != Some(now) {
            match now {
                Some(f) => say(
                    Level::Debug,
                    &format!(
                        "Windows device format of endpoint {}: {} ({} ch, {}-bit in {})",
                        known.as_deref().unwrap_or("?"),
                        short(f),
                        f.channels,
                        f.valid_bits,
                        f.container_bits
                    ),
                ),
                None => say(
                    Level::Debug,
                    "Windows device format: no endpoint of these buds found (yet)",
                ),
            }
            *shared.format.lock().unwrap_or_else(|p| p.into_inner()) = now;
            shared.generation.fetch_add(1, Ordering::AcqRel);
            let _ = unsafe { SetEvent(shared.wake.handle()) };
            last = Some(now);
        }
        if armed != ERROR_SUCCESS {
            if !complained {
                say(
                    Level::Error,
                    &format!(
                        "Windows device format: cannot watch the registry (Win32 error {}); reading it every {} s instead",
                        armed.0,
                        RETRY.as_secs()
                    ),
                );
                complained = true;
            }
            key = None;
            if stop_within(stop, RETRY) {
                return;
            }
            continue;
        }
        // Let a burst of writes pass (they signal the armed event, so the
        // wait below returns at once for them), then wait for the next.
        if stop_within(stop, SETTLE) {
            return;
        }
        let handles = [stop.0, changed.0];
        let w = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
        match woken_by(w) {
            Woken::Changed => {}
            Woken::Stop => return,
            Woken::Failed => {
                // Do not spin on it; but say that changes are no longer
                // followed (unless it was the stop after all).
                let err = unsafe { GetLastError() };
                if !stop_within(stop, Duration::ZERO) {
                    say(
                        Level::Error,
                        &format!(
                            "Windows device format: waiting for registry changes failed (wait result {:#x}, Win32 error {}); changes of the format in the sound settings are no longer followed",
                            w.0, err.0
                        ),
                    );
                }
                return;
            }
        }
    }
}

/// What woke the watcher's wait on `[stop, changed]`.
#[derive(Debug, PartialEq, Eq)]
enum Woken {
    Stop,
    Changed,
    /// `WAIT_FAILED`, or anything else the wait has no business returning.
    Failed,
}

fn woken_by(w: WAIT_EVENT) -> Woken {
    match w.0.wrapping_sub(WAIT_OBJECT_0.0) {
        0 => Woken::Stop,
        1 => Woken::Changed,
        _ => Woken::Failed,
    }
}

/// A value of `sub` (relative to the render key) as raw bytes.
fn value(key: &Key, sub: &str, name: &str, kind: REG_ROUTINE_FLAGS) -> Option<Vec<u8>> {
    let (sub, name) = (HSTRING::from(sub), HSTRING::from(name));
    let mut size = 0u32;
    let r = unsafe { RegGetValueW(key.0, &sub, &name, kind, None, None, Some(&mut size)) };
    if r != ERROR_SUCCESS || size == 0 || size > 64 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let r = unsafe {
        RegGetValueW(
            key.0,
            &sub,
            &name,
            kind,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    buf.truncate(size as usize);
    Some(buf)
}

fn utf16_string(b: &[u8]) -> String {
    let w: Vec<u16> = b
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16_lossy(&w)
}

/// What one endpoint says: its rank ([`state_rank`]) and format, if it is
/// the crossc device's endpoint and usable.
fn endpoint(key: &Key, guid: &str, interface: &str) -> Option<(u8, Option<PcmFormat>)> {
    let props = format!("{guid}\\Properties");
    let ks = value(key, &props, KS_PATH_VALUE, RRF_RT_REG_SZ)?;
    if !is_crossc_endpoint(&utf16_string(&ks), interface) {
        return None;
    }
    let state = value(key, guid, "DeviceState", RRF_RT_REG_DWORD)?;
    let state = u32::from_le_bytes(state.get(..4)?.try_into().ok()?);
    let rank = state_rank(state)?;
    let format = value(key, &props, DEVICE_FORMAT_VALUE, RRF_RT_REG_BINARY)
        .and_then(|b| parse_device_format(&b));
    Some((rank, format))
}

/// The names of the endpoint keys under the render key (at most
/// [`MAX_ENDPOINTS`]).
fn endpoint_names(key: &Key) -> impl Iterator<Item = String> + '_ {
    (0..MAX_ENDPOINTS).map_while(move |i| {
        let mut name = [0u16; 128];
        let mut len = name.len() as u32;
        let r = unsafe {
            RegEnumKeyExW(
                key.0,
                i,
                Some(PWSTR(name.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        (r == ERROR_SUCCESS)
            .then(|| String::from_utf16_lossy(&name[..(len as usize).min(name.len())]))
    })
}

/// The device format of the crossc device's endpoint. `known`: the
/// endpoint found last time, tried first; updated.
fn read_format(key: &Key, interface: &str, known: &mut Option<String>) -> Option<PcmFormat> {
    pick_endpoint(
        known,
        |g| endpoint(key, g, interface),
        || endpoint_names(key),
    )
}

/// [`read_format`] without the registry: `lookup` is [`endpoint`], `names`
/// enumerates the endpoint keys.
///
/// The known endpoint is taken in any usable state: unplugged is how it
/// is whenever the buds are away, and a device has one endpoint for its
/// circuit (its id follows from the device interface and the pin), so
/// there is nothing better to find. All endpoints are looked at only when
/// none is known yet or the known one is gone, disabled or not present.
fn pick_endpoint<I: IntoIterator<Item = String>>(
    known: &mut Option<String>,
    mut lookup: impl FnMut(&str) -> Option<(u8, Option<PcmFormat>)>,
    names: impl FnOnce() -> I,
) -> Option<PcmFormat> {
    if let Some(g) = known.as_deref()
        && let Some((_, f)) = lookup(g)
    {
        return f;
    }
    // Unknown or no longer usable: look at all of them, best rank first.
    let mut best: Option<(u8, String, Option<PcmFormat>)> = None;
    for guid in names() {
        if let Some((rank, f)) = lookup(&guid)
            && best.as_ref().is_none_or(|b| rank < b.0)
        {
            best = Some((rank, guid, f));
        }
    }
    match best {
        Some((_, guid, f)) => {
            *known = Some(guid);
            f
        }
        None => {
            *known = None;
            None
        }
    }
}

/// A wake event of nobody's watcher, for tests of the waiters.
#[cfg(test)]
pub(crate) fn tests_wake_event() -> WakeEvent {
    WakeEvent(Arc::new(event(true).unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::NullLog;

    #[test]
    fn a_device_nobody_has_gives_no_format_and_the_watcher_stops() {
        // A path no endpoint belongs to: the watcher reads the real
        // registry, finds nothing, reports that once, and stops promptly.
        let iface = r"\\?\BTHENUM#{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013#b&0&0&000000000000_C00000000#{433f9eba-036f-480d-826f-e0ec953319a9}";
        let mut w = FormatWatch::start(iface, Arc::new(NullLog), "test").unwrap();
        let ev = w.wake_event();
        let signalled = unsafe { WaitForSingleObject(ev.handle(), 20_000) } == WAIT_OBJECT_0;
        assert!(signalled, "the first reading is published");
        assert!(w.changed());
        assert!(!w.changed(), "once per change");
        assert_eq!(w.current(), None);
        ev.reset();
        let t0 = std::time::Instant::now();
        drop(w);
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    /// On a machine with crossc installed: what the watcher finds for each
    /// present crossc device (`cargo test -p crossc-core -- --ignored
    /// --nocapture installed`).
    #[test]
    #[ignore = "needs an installed crossc device"]
    fn installed_devices_have_an_endpoint_format() {
        let k = open_render_key().expect("render key");
        let ifaces = crate::device::list_interfaces().unwrap();
        assert!(!ifaces.is_empty(), "no crossc device interface present");
        for iface in ifaces {
            let mut known = None;
            let f = read_format(&k, &iface, &mut known);
            println!("device format: {f:?}");
            // An endpoint Windows never opened may have no device format
            // stored yet; the endpoint itself is always there.
            assert!(known.is_some(), "no endpoint found for a present device");
        }
    }

    #[test]
    fn wait_results() {
        use windows::Win32::Foundation::{WAIT_ABANDONED_0, WAIT_FAILED, WAIT_TIMEOUT};
        assert_eq!(woken_by(WAIT_OBJECT_0), Woken::Stop);
        assert_eq!(woken_by(WAIT_EVENT(WAIT_OBJECT_0.0 + 1)), Woken::Changed);
        assert_eq!(woken_by(WAIT_FAILED), Woken::Failed);
        assert_eq!(woken_by(WAIT_TIMEOUT), Woken::Failed);
        assert_eq!(woken_by(WAIT_ABANDONED_0), Woken::Failed);
        assert_eq!(woken_by(WAIT_EVENT(WAIT_OBJECT_0.0 + 2)), Woken::Failed);
    }

    #[test]
    fn the_known_endpoint_is_kept_while_unplugged_without_a_scan() {
        use crossc_ioctl::audio::OFFERED_FORMATS;
        let f = Some(OFFERED_FORMATS[1]);
        let mut state: Option<(u8, Option<PcmFormat>)> = Some((1, f));
        let mut scans = 0;
        let mut known = Some("{ours}".to_string());
        let read = |known: &mut Option<String>,
                    state: Option<(u8, Option<PcmFormat>)>,
                    scans: &mut u32| {
            pick_endpoint(
                known,
                |g| if g == "{ours}" { state } else { None },
                || {
                    *scans += 1;
                    ["{other}".to_string(), "{ours}".to_string()]
                },
            )
        };
        // Unplugged (the buds away), then active: no enumeration either way.
        for _ in 0..4 {
            assert_eq!(read(&mut known, state, &mut scans), f);
        }
        state = Some((0, f));
        assert_eq!(read(&mut known, state, &mut scans), f);
        assert_eq!(scans, 0);
        // Gone (not present): enumerated, and nothing found.
        assert_eq!(read(&mut known, None, &mut scans), None);
        assert_eq!((scans, known.as_deref()), (1, None));
        // Back: found by the next enumeration and known again.
        assert_eq!(read(&mut known, Some((1, f)), &mut scans), f);
        assert_eq!((scans, known.as_deref()), (2, Some("{ours}")));
        assert_eq!(read(&mut known, Some((1, f)), &mut scans), f);
        assert_eq!(scans, 2);
    }

    #[test]
    fn an_enumeration_prefers_an_active_endpoint() {
        use crossc_ioctl::audio::OFFERED_FORMATS;
        let (a, b) = (Some(OFFERED_FORMATS[0]), Some(OFFERED_FORMATS[1]));
        let mut known = None;
        let got = pick_endpoint(
            &mut known,
            |g| match g {
                "{unplugged}" => Some((1, a)),
                "{active}" => Some((0, b)),
                _ => None,
            },
            || ["{x}", "{unplugged}", "{active}"].map(String::from),
        );
        assert_eq!((got, known.as_deref()), (b, Some("{active}")));
    }

    #[test]
    fn the_render_key_is_readable_without_privileges() {
        // The service runs as LocalSystem; this test as whoever runs it.
        // Either way the key opens and enumerates.
        let k = open_render_key().expect("render key");
        let mut known = None;
        let _ = read_format(&k, r"\\?\x#y#z#w", &mut known);
        assert_eq!(known, None);
    }
}
