//! Where the service's threads leave their state, and when a snapshot of
//! it goes out.
//!
//! [`Hub`] is the latest state: one [`DeviceStatus`] per device, set by
//! that device's session thread, plus the service's own fields. Setting a
//! status takes a short lock, replaces a value and signals the server's
//! thread; it does no I/O and never waits for a client. A status equal to
//! the one already there does nothing at all. What is put in is cut to
//! the wire format's caps on the way ([`crate::wire::limit_device`]): a
//! long log line or name cannot make the state too large to send.
//!
//! [`Pacer`] is the rule for the server's thread: a change of substance
//! goes out at once, running numbers at most every [`STREAMING_INTERVAL`]
//! while audio streams and every [`IDLE_INTERVAL`] otherwise, and with no
//! change at all a snapshot still goes out every [`IDLE_INTERVAL`], which
//! is how a client knows the service is alive.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::PROTOCOL_VERSION;
use crate::model::{DeviceStatus, Event, ServiceInfo, Snapshot};
use crate::wire::{self, MAX_DEVICES};

/// Running numbers go out this often while audio streams.
pub const STREAMING_INTERVAL: Duration = Duration::from_millis(500);
/// Running numbers go out this often otherwise; also the interval of the
/// snapshots that say nothing new.
pub const IDLE_INTERVAL: Duration = Duration::from_secs(2);
/// Two snapshots are at least this far apart, whatever changed.
pub const MIN_GAP: Duration = Duration::from_millis(100);

type Waker = Box<dyn Fn() + Send + Sync>;

/// Names a waker registered with [`Hub::on_change`], to take it out again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakerId(u64);

#[derive(Default)]
struct Wakers {
    next: u64,
    list: Vec<(u64, Waker)>,
}

struct State {
    service: ServiceInfo,
    /// Sorted by id, at most [`MAX_DEVICES`].
    devices: Vec<DeviceStatus>,
    /// Counts the changes.
    generation: u64,
}

struct Inner {
    state: Mutex<State>,
    /// Called (outside the lock) after every change.
    wakers: Mutex<Wakers>,
}

/// The latest state of the service. Cheap to clone; all clones are the
/// same hub.
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

impl Hub {
    pub fn new(mut service: ServiceInfo) -> Hub {
        wire::limit_service(&mut service);
        Hub {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    service,
                    devices: Vec::new(),
                    generation: 0,
                }),
                wakers: Mutex::new(Wakers::default()),
            }),
        }
    }

    /// A thread that panicked while it held the lock left a complete value
    /// behind (every change is one assignment): keep using it.
    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn changed(&self) {
        let wakers = self
            .inner
            .wakers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (_, w) in &wakers.list {
            w();
        }
    }

    /// Set the status of the device `status.id` (new or known). Returns
    /// whether anything changed. A device beyond [`MAX_DEVICES`] is not
    /// taken.
    pub fn set_device(&self, mut status: DeviceStatus) -> bool {
        wire::limit_device(&mut status);
        {
            let mut s = self.state();
            match s.devices.binary_search_by(|d| d.id.cmp(&status.id)) {
                Ok(i) if s.devices[i] == status => return false,
                Ok(i) => s.devices[i] = status,
                Err(_) if s.devices.len() >= MAX_DEVICES => return false,
                Err(i) => s.devices.insert(i, status),
            }
            s.generation += 1;
        }
        self.changed();
        true
    }

    /// The device is gone (its interface was removed, its session ended).
    pub fn remove_device(&self, id: &str) -> bool {
        let id = key(id);
        {
            let mut s = self.state();
            let Ok(i) = s
                .devices
                .binary_search_by(|d| d.id.as_str().cmp(id.as_str()))
            else {
                return false;
            };
            s.devices.remove(i);
            s.generation += 1;
        }
        self.changed();
        true
    }

    /// No device is left (the service stops its sessions).
    pub fn clear_devices(&self) {
        {
            let mut s = self.state();
            if s.devices.is_empty() {
                return;
            }
            s.devices.clear();
            s.generation += 1;
        }
        self.changed();
    }

    /// The last warning or error that belongs to no device.
    pub fn set_service_event(&self, mut event: Option<Event>) {
        if let Some(e) = &mut event {
            wire::limit_event(e);
        }
        {
            let mut s = self.state();
            if s.service.last_event == event {
                return;
            }
            s.service.last_event = event;
            s.generation += 1;
        }
        self.changed();
    }

    /// The last warning or error of device `id`, kept across the statuses
    /// its session sets (those carry `last_event: None`; see
    /// [`Self::set_device_keeping_event`]). Unknown id: nothing happens.
    pub fn set_device_event(&self, id: &str, mut event: Event) {
        wire::limit_event(&mut event);
        let id = key(id);
        {
            let mut s = self.state();
            let Ok(i) = s
                .devices
                .binary_search_by(|d| d.id.as_str().cmp(id.as_str()))
            else {
                return;
            };
            if s.devices[i].last_event.as_ref() == Some(&event) {
                return;
            }
            s.devices[i].last_event = Some(event);
            s.generation += 1;
        }
        self.changed();
    }

    /// [`Self::set_device`] for a status without an event of its own: the
    /// event the hub holds for the device stays.
    pub fn set_device_keeping_event(&self, mut status: DeviceStatus) -> bool {
        if status.last_event.is_none() {
            let id = key(&status.id);
            let s = self.state();
            if let Ok(i) = s.devices.binary_search_by(|d| d.id.cmp(&id)) {
                status.last_event = s.devices[i].last_event.clone();
            }
        }
        self.set_device(status)
    }

    /// Counts the changes so far.
    pub fn generation(&self) -> u64 {
        self.state().generation
    }

    /// The state now, as a snapshot (sequence number and time are the
    /// sender's to fill in).
    pub fn snapshot(&self) -> Snapshot {
        let s = self.state();
        Snapshot {
            version: PROTOCOL_VERSION,
            seq: 0,
            time_unix: 0,
            service: s.service.clone(),
            devices: s.devices.clone(),
        }
    }

    /// Call `waker` after every change, from the thread that made it. It
    /// must be quick and must not call back into the hub. It stays until
    /// [`Self::remove_waker`] is called with what this returns.
    pub fn on_change(&self, waker: impl Fn() + Send + Sync + 'static) -> WakerId {
        let mut wakers = self
            .inner
            .wakers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        wakers.next += 1;
        let id = wakers.next;
        wakers.list.push((id, Box::new(waker)));
        WakerId(id)
    }

    /// Stop calling the waker `id` and drop it. Unknown: nothing happens.
    pub fn remove_waker(&self, id: WakerId) {
        let removed: Vec<(u64, Waker)> = {
            let mut wakers = self
                .inner
                .wakers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let (gone, kept) = std::mem::take(&mut wakers.list)
                .into_iter()
                .partition(|(i, _)| *i == id.0);
            wakers.list = kept;
            gone
        };
        // Dropped outside the lock: what a waker owns may do anything.
        drop(removed);
    }

    /// Wakers registered at the moment.
    pub fn waker_count(&self) -> usize {
        self.inner
            .wakers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .list
            .len()
    }
}

/// The id a device is kept under: the caller's, cut like every string.
fn key(id: &str) -> String {
    let mut id = id.to_string();
    wire::clean(&mut id);
    id
}

/// What the server's thread does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pace {
    /// Send the current state now.
    Send,
    /// Nothing is due; look again after this long (or at the next change).
    Wait(Duration),
}

/// Decides when the next snapshot goes out. Times are a monotonic clock's.
#[derive(Debug, Default)]
pub struct Pacer {
    /// What was sent last and when.
    last: Option<(Duration, Snapshot)>,
}

impl Pacer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Given the state `current` at time `now`.
    pub fn next(&self, now: Duration, current: &Snapshot) -> Pace {
        let Some((at, sent)) = &self.last else {
            return Pace::Send;
        };
        let since = now.saturating_sub(*at);
        let due = if sent.steady() != current.steady() {
            MIN_GAP
        } else if sent.devices != current.devices && current.streaming() {
            STREAMING_INTERVAL
        } else {
            IDLE_INTERVAL
        };
        if since >= due {
            Pace::Send
        } else {
            Pace::Wait(due - since)
        }
    }

    /// `current` was sent at `now`.
    pub fn sent(&mut self, now: Duration, current: Snapshot) {
        self.last = Some((now, current));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LinkPhase, sample};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn dev(id: &str) -> DeviceStatus {
        DeviceStatus {
            id: id.into(),
            ..DeviceStatus::default()
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn devices_are_set_replaced_and_removed_by_id() {
        let hub = Hub::new(ServiceInfo::default());
        let woken = Arc::new(AtomicUsize::new(0));
        let w = woken.clone();
        hub.on_change(move || {
            w.fetch_add(1, Ordering::SeqCst);
        });
        assert!(hub.set_device(dev("B")));
        assert!(hub.set_device(dev("A")));
        assert!(
            !hub.set_device(dev("A")),
            "the same status again: no change"
        );
        assert_eq!(woken.load(Ordering::SeqCst), 2);
        let s = hub.snapshot();
        assert_eq!(s.version, PROTOCOL_VERSION);
        assert_eq!(
            s.devices.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["A", "B"]
        );
        let mut a = dev("A");
        a.phase = LinkPhase::Ready;
        assert!(hub.set_device(a));
        assert_eq!(hub.snapshot().devices[0].phase, LinkPhase::Ready);
        assert_eq!(hub.snapshot().devices.len(), 2);
        assert!(hub.remove_device("A"));
        assert!(!hub.remove_device("A"));
        assert_eq!(hub.snapshot().devices.len(), 1);
        assert_eq!(hub.generation(), 4);
        hub.clear_devices();
        hub.clear_devices();
        assert_eq!(hub.generation(), 5);
        assert!(hub.snapshot().devices.is_empty());
    }

    #[test]
    fn the_device_count_is_bounded() {
        let hub = Hub::new(ServiceInfo::default());
        for i in 0..MAX_DEVICES {
            assert!(hub.set_device(dev(&format!("{i:02}"))));
        }
        assert!(!hub.set_device(dev("one too many")));
        assert_eq!(hub.snapshot().devices.len(), MAX_DEVICES);
        // Known ones are still updated.
        let mut d = dev("03");
        d.name = "x".into();
        assert!(hub.set_device(d));
    }

    /// The largest thing a string can be in JSON after the caps: 200
    /// characters that are escaped and 100 of four bytes.
    fn worst(tag: char) -> String {
        format!("{tag}{}{}", "\"".repeat(199), "🎧".repeat(4000))
    }

    #[test]
    fn whatever_is_put_in_fits_a_message() {
        use crate::model::*;
        let event = || Event {
            level: EventLevel::Warning,
            time_unix: u64::MAX,
            text: worst('e'),
        };
        let hub = Hub::new(ServiceInfo {
            version: worst('v'),
            started_unix: u64::MAX,
            log_dir: worst('l'),
            last_event: Some(event()),
        });
        for i in 0..MAX_DEVICES + 3 {
            hub.set_device(DeviceStatus {
                id: worst(char::from(b'a' + i as u8)),
                name: worst('n'),
                phase: LinkPhase::WaitingDriver,
                phase_reason: worst('r'),
                endpoint: Some(PcmFormat {
                    sample_rate: u32::MAX,
                    bits: u8::MAX,
                    channels: u8::MAX,
                }),
                link: Some(LinkStatus {
                    mode: CodecMode::Unknown,
                    config: u8::MAX,
                    caps: u8::MAX,
                    stream_rate: u32::MAX,
                    stream_bits: u8::MAX,
                    bitrate_kbps: u32::MAX,
                    top_kbps: u32::MAX,
                    adaptive: false,
                    ladder_kbps: vec![u32::MAX; 500],
                    rung: u32::MAX,
                    frames_per_packet: u32::MAX,
                    packet_bytes: u32::MAX,
                    mtu_limit: u32::MAX,
                    mtu_reason: worst('m'),
                    sink_delay_tenth_ms: Some(u32::MAX),
                    streaming_ms: Some(u64::MAX),
                }),
                volume: Some(VolumeStatus {
                    mode: VolumeMode::Software,
                    level_percent: Some(u8::MAX),
                    muted: false,
                }),
                counters: Counters {
                    packets: u64::MAX,
                    pcm_frames: u64::MAX,
                    dropped_frames: u64::MAX,
                    discontinuities: u64::MAX,
                    write_errors: u64::MAX,
                    write_avg_us: u32::MAX,
                    write_max_us: u32::MAX,
                    late_packets: u64::MAX,
                    reconnects: u32::MAX,
                },
                driver: Some(DriverInfo {
                    version: worst('d'),
                    protocol: u32::MAX,
                }),
                last_event: Some(event()),
            });
        }
        hub.set_service_event(Some(event()));
        let id = hub.snapshot().devices[0].id.clone();
        hub.set_device_event(&id, event());
        let mut s = hub.snapshot();
        s.seq = u64::MAX;
        s.time_unix = u64::MAX;
        assert_eq!(s.devices.len(), MAX_DEVICES);
        let d = &s.devices[0];
        assert_eq!(d.phase_reason.chars().count(), 300);
        assert_eq!(d.phase_reason.len(), 600);
        assert_eq!(d.link.as_ref().unwrap().ladder_kbps.len(), wire::MAX_LADDER);
        assert_eq!(d.volume.as_ref().unwrap().level_percent, Some(100));
        assert_eq!(d.last_event.as_ref().unwrap().text.len(), 600);
        assert_eq!(s.service.log_dir.len(), 600);
        let message = wire::encode(&s).expect("the worst state fits a message");
        assert!(
            message.len() < wire::MAX_MESSAGE * 9 / 10,
            "{} bytes: no room left for a new field",
            message.len()
        );
        // And comes out of a reader as it went in: the caps are the same.
        assert_eq!(wire::decode(&message).unwrap(), s);
    }

    #[test]
    fn a_status_that_differs_only_beyond_the_caps_is_no_change() {
        let hub = Hub::new(ServiceInfo::default());
        let mut d = dev("A");
        d.phase_reason = "r".repeat(400);
        assert!(hub.set_device(d.clone()));
        d.phase_reason.push_str("more");
        assert!(!hub.set_device(d), "cut to the same 300 characters");
        assert_eq!(hub.generation(), 1);
    }

    #[test]
    fn a_device_is_found_by_the_id_it_was_given() {
        // An id beyond the caps is kept cut; the caller still uses its own.
        let hub = Hub::new(ServiceInfo::default());
        let long = "i".repeat(400);
        assert!(hub.set_device_keeping_event(dev(&long)));
        hub.set_device_event(
            &long,
            Event {
                text: "x".into(),
                ..Event::default()
            },
        );
        let mut d = dev(&long);
        d.phase = LinkPhase::Ready;
        assert!(hub.set_device_keeping_event(d));
        let s = hub.snapshot();
        assert_eq!(s.devices.len(), 1);
        assert_eq!(s.devices[0].id.len(), 300);
        assert_eq!(s.devices[0].last_event.as_ref().unwrap().text, "x");
        assert!(hub.remove_device(&long));
        assert!(hub.snapshot().devices.is_empty());
    }

    #[test]
    fn wakers_can_be_taken_out() {
        let hub = Hub::new(ServiceInfo::default());
        let count = Arc::new(AtomicUsize::new(0));
        let ids: Vec<WakerId> = (0..3)
            .map(|_| {
                let c = count.clone();
                hub.on_change(move || {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();
        hub.set_device(dev("A"));
        assert_eq!(count.load(Ordering::SeqCst), 3);
        hub.remove_waker(ids[1]);
        hub.remove_waker(ids[1]);
        assert_eq!(hub.waker_count(), 2);
        // What the waker held is dropped with it.
        assert_eq!(Arc::strong_count(&count), 3);
        hub.remove_device("A");
        assert_eq!(count.load(Ordering::SeqCst), 5);
        hub.remove_waker(ids[0]);
        hub.remove_waker(ids[2]);
        assert_eq!(hub.waker_count(), 0);
        hub.clear_devices();
        assert_eq!(count.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn events_are_kept_across_statuses() {
        let hub = Hub::new(ServiceInfo::default());
        let e = Event {
            text: "link down".into(),
            time_unix: 5,
            ..Event::default()
        };
        hub.set_device_event("A", e.clone()); // unknown device: ignored
        assert_eq!(hub.generation(), 0);
        hub.set_device_keeping_event(dev("A"));
        hub.set_device_event("A", e.clone());
        hub.set_device_event("A", e.clone());
        assert_eq!(hub.generation(), 2);
        let mut a = dev("A");
        a.phase = LinkPhase::Ready;
        assert!(hub.set_device_keeping_event(a));
        assert_eq!(hub.snapshot().devices[0].last_event, Some(e.clone()));
        hub.set_service_event(Some(e.clone()));
        hub.set_service_event(Some(e.clone()));
        assert_eq!(hub.snapshot().service.last_event, Some(e));
        assert_eq!(hub.generation(), 4);
    }

    #[test]
    fn a_hub_is_shared_between_threads() {
        let hub = Hub::new(ServiceInfo::default());
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let hub = hub.clone();
                std::thread::spawn(move || {
                    for i in 0..500u64 {
                        let mut d = dev(&format!("dev{t}"));
                        d.counters.packets = i;
                        hub.set_device(d);
                        let _ = hub.snapshot();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let s = hub.snapshot();
        assert_eq!(s.devices.len(), 4);
        assert!(s.devices.iter().all(|d| d.counters.packets == 499));
    }

    #[test]
    fn pacing() {
        let mut p = Pacer::new();
        let mut s = sample(); // streaming
        assert_eq!(p.next(ms(0), &s), Pace::Send, "the first one at once");
        p.sent(ms(0), s.clone());
        // Nothing new: a snapshot every 2 s all the same.
        assert_eq!(p.next(ms(300), &s), Pace::Wait(ms(1700)));
        assert_eq!(p.next(ms(2000), &s), Pace::Send);
        // Running numbers while streaming: every 500 ms.
        s.devices[0].counters.packets += 50;
        assert_eq!(p.next(ms(300), &s), Pace::Wait(ms(200)));
        assert_eq!(p.next(ms(500), &s), Pace::Send);
        p.sent(ms(500), s.clone());
        // A change of substance: at once, but never two within 100 ms.
        s.devices[0].link.as_mut().unwrap().bitrate_kbps = 152;
        assert_eq!(p.next(ms(520), &s), Pace::Wait(ms(80)));
        assert_eq!(p.next(ms(600), &s), Pace::Send);
        p.sent(ms(600), s.clone());
        // Not streaming: running numbers (the driver's dropped frames, say)
        // wait for the 2 s.
        s.devices[0].phase = LinkPhase::Ready;
        assert_eq!(p.next(ms(700), &s), Pace::Send);
        p.sent(ms(700), s.clone());
        s.devices[0].counters.dropped_frames += 1;
        assert_eq!(p.next(ms(1300), &s), Pace::Wait(ms(1400)));
        assert_eq!(p.next(ms(2700), &s), Pace::Send);
        // A device that appears or goes is a change of substance.
        p.sent(ms(2700), s.clone());
        s.devices.clear();
        assert_eq!(p.next(ms(2800), &s), Pace::Send);
        // A clock that steps back does not stall anything for long.
        p.sent(ms(5000), s.clone());
        assert_eq!(p.next(ms(10), &s), Pace::Wait(IDLE_INTERVAL));
    }
}
