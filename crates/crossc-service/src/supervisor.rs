//! One session thread per crossc device interface. The supervisor rescans
//! the interfaces every few seconds, starts a session for each one that
//! arrived and stops the session of each one that went away. It also tears
//! everything down while the machine sleeps and on stop.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use crossc_core::log::{Level, Log};
use crossc_core::stop::StopSignal;

/// How often the device interfaces are listed.
pub const RESCAN: Duration = Duration::from_secs(5);

/// What the outside tells the supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Stop everything and return.
    Stop,
    /// The machine is going to sleep: stop all sessions, keep waiting.
    Suspend,
    /// The machine is awake again: rescan and restart sessions.
    Resume,
}

/// A running session thread.
pub trait Worker {
    /// The thread has ended (by itself, or after its stop was requested).
    fn finished(&self) -> bool;
    /// Wait for the thread to end.
    fn join(self: Box<Self>);
}

/// What the supervisor needs from the system.
pub trait Host {
    /// The crossc device interfaces present right now.
    fn list(&self) -> Result<Vec<String>, String>;
    /// Start a session for interface `path`; it must end once `stop` is
    /// requested.
    fn spawn(&self, path: &str, stop: StopSignal) -> Box<dyn Worker>;
    /// Every session has been stopped and joined (the machine goes to
    /// sleep): nothing of them may be left on show.
    fn sessions_stopped(&self) {}
}

struct Running {
    stop: StopSignal,
    worker: Box<dyn Worker>,
}

pub struct Supervisor<H: Host> {
    host: H,
    log: Arc<dyn Log>,
    sessions: BTreeMap<String, Running>,
    list_failed: bool,
}

/// The device a path belongs to, short enough for a log prefix: the buds'
/// Bluetooth address from the interface path, else the path's tail.
pub fn short_name(path: &str) -> String {
    // \\?\BTHENUM#{...}_VID&..._PID&...#b&1a2b3c4d&0&AABBCCDDEEFF_C00000000#{guid}
    let addr = path.split('#').find_map(|part| {
        let tail = part.rsplit('&').next()?;
        let a = tail.split('_').next()?;
        (a.len() == 12 && a.bytes().all(|b| b.is_ascii_hexdigit())).then(|| a.to_ascii_uppercase())
    });
    match addr {
        Some(a) => a,
        None => {
            let n = path.chars().count();
            path.chars().skip(n.saturating_sub(24)).collect()
        }
    }
}

impl<H: Host> Supervisor<H> {
    pub fn new(host: H, log: Arc<dyn Log>) -> Self {
        Supervisor {
            host,
            log,
            sessions: BTreeMap::new(),
            list_failed: false,
        }
    }

    /// Interfaces that have a session.
    #[cfg(test)]
    pub fn active(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    /// One rescan: stop sessions whose interface is gone, start sessions for
    /// new interfaces, restart sessions that ended by themselves.
    pub fn scan(&mut self) {
        let present = match self.host.list() {
            Ok(l) => {
                self.list_failed = false;
                l
            }
            Err(e) => {
                // Keep what runs; say it once until it works again.
                if !self.list_failed {
                    self.log.log(
                        Level::Error,
                        &format!("cannot list the crossc device interfaces: {e}"),
                    );
                }
                self.list_failed = true;
                return;
            }
        };
        let gone: Vec<String> = self
            .sessions
            .keys()
            .filter(|p| !present.contains(p))
            .cloned()
            .collect();
        for path in gone {
            self.log.log(
                Level::Info,
                &format!("[{}] device interface removed", short_name(&path)),
            );
            self.stop_one(&path);
        }
        for path in &present {
            let ended = self.sessions.get(path).is_some_and(|r| r.worker.finished());
            if ended {
                self.log.log(
                    Level::Error,
                    &format!(
                        "[{}] the session ended unexpectedly; restarting it",
                        short_name(path)
                    ),
                );
                self.stop_one(path);
            }
            if !self.sessions.contains_key(path) {
                self.log.log(
                    Level::Info,
                    &format!("[{}] device interface found: {path}", short_name(path)),
                );
                let stop = StopSignal::new();
                let worker = self.host.spawn(path, stop.clone());
                self.sessions.insert(path.clone(), Running { stop, worker });
            }
        }
    }

    fn stop_one(&mut self, path: &str) {
        if let Some(r) = self.sessions.remove(path) {
            r.stop.request();
            r.worker.join();
        }
    }

    /// Stop every session: ask all of them first, then wait for each.
    pub fn stop_all(&mut self) {
        for r in self.sessions.values() {
            r.stop.request();
        }
        for (_, r) in std::mem::take(&mut self.sessions) {
            r.worker.join();
        }
    }

    /// Scan now and then every [`RESCAN`], until [`Command::Stop`] arrives
    /// (or the sender is gone). Returns with the sessions still running, so
    /// the caller can report "stopping" before [`Self::stop_all`].
    pub fn run(&mut self, commands: &Receiver<Command>) {
        self.run_every(commands, RESCAN);
    }

    pub fn run_every(&mut self, commands: &Receiver<Command>, rescan: Duration) {
        let mut suspended = false;
        if self.sessions.is_empty() {
            self.scan();
            if self.sessions.is_empty() && !self.list_failed {
                self.log.log(
                    Level::Info,
                    "no crossc device interface present; waiting for one",
                );
            }
        }
        loop {
            match commands.recv_timeout(rescan) {
                Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(Command::Suspend) => {
                    if !suspended {
                        self.log
                            .log(Level::Info, "system is suspending: stopping sessions");
                        self.stop_all();
                        self.host.sessions_stopped();
                    }
                    suspended = true;
                }
                Ok(Command::Resume) => {
                    if suspended {
                        self.log
                            .log(Level::Info, "system resumed: restarting sessions");
                    }
                    suspended = false;
                    self.scan();
                }
                Err(RecvTimeoutError::Timeout) => {
                    if !suspended {
                        self.scan();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossc_core::fake::MemoryLog;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    /// (path, stop signal, "thread died by itself" flag) of one spawn.
    type Spawned = (String, StopSignal, Arc<AtomicBool>);

    struct World {
        present: Mutex<Result<Vec<String>, String>>,
        spawned: Mutex<Vec<Spawned>>,
        joined: Mutex<Vec<String>>,
        /// Sessions joined at each "all sessions stopped".
        all_stopped: Mutex<Vec<usize>>,
    }

    struct FakeHost(Arc<World>);

    struct FakeWorker {
        path: String,
        stop: StopSignal,
        died: Arc<AtomicBool>,
        world: Arc<World>,
    }

    impl Worker for FakeWorker {
        fn finished(&self) -> bool {
            self.died.load(Ordering::SeqCst)
        }
        fn join(self: Box<Self>) {
            assert!(
                self.stop.is_set() || self.died.load(Ordering::SeqCst),
                "joined a session that was not asked to stop"
            );
            self.world.joined.lock().unwrap().push(self.path.clone());
        }
    }

    impl Host for FakeHost {
        fn list(&self) -> Result<Vec<String>, String> {
            self.0.present.lock().unwrap().clone()
        }
        fn spawn(&self, path: &str, stop: StopSignal) -> Box<dyn Worker> {
            let died = Arc::new(AtomicBool::new(false));
            self.0
                .spawned
                .lock()
                .unwrap()
                .push((path.to_string(), stop.clone(), died.clone()));
            Box::new(FakeWorker {
                path: path.to_string(),
                stop,
                died,
                world: self.0.clone(),
            })
        }
        fn sessions_stopped(&self) {
            let joined = self.0.joined.lock().unwrap().len();
            self.0.all_stopped.lock().unwrap().push(joined);
        }
    }

    fn world(present: &[&str]) -> Arc<World> {
        let w = Arc::new(World {
            present: Mutex::new(Ok(Vec::new())),
            spawned: Mutex::default(),
            joined: Mutex::default(),
            all_stopped: Mutex::default(),
        });
        set(&w, present);
        w
    }

    fn set(w: &World, present: &[&str]) {
        *w.present.lock().unwrap() = Ok(present.iter().map(|s| s.to_string()).collect());
    }

    fn sup(w: &Arc<World>) -> (Supervisor<FakeHost>, Arc<MemoryLog>) {
        let log = Arc::new(MemoryLog::default());
        (Supervisor::new(FakeHost(w.clone()), log.clone()), log)
    }

    fn spawned(w: &World) -> Vec<String> {
        w.spawned
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.0.clone())
            .collect()
    }

    #[test]
    fn arrivals_and_removals() {
        let w = world(&["a"]);
        let (mut s, log) = sup(&w);
        s.scan();
        assert_eq!(s.active(), ["a"]);
        s.scan();
        assert_eq!(spawned(&w), ["a"], "one session per interface");
        set(&w, &["a", "b"]);
        s.scan();
        assert_eq!(s.active(), ["a", "b"]);
        set(&w, &["b"]);
        s.scan();
        assert_eq!(s.active(), ["b"]);
        assert_eq!(*w.joined.lock().unwrap(), ["a"]);
        assert!(w.spawned.lock().unwrap()[0].1.is_set(), "a was stopped");
        assert!(!w.spawned.lock().unwrap()[1].1.is_set(), "b runs on");
        // It comes back: a new session.
        set(&w, &["a", "b"]);
        s.scan();
        assert_eq!(spawned(&w), ["a", "b", "a"]);
        s.stop_all();
        assert!(s.active().is_empty());
        assert!(w.spawned.lock().unwrap().iter().all(|x| x.1.is_set()));
        assert_eq!(w.joined.lock().unwrap().len(), 3);
        assert_eq!(log.find("device interface found").len(), 3);
        assert_eq!(log.find("device interface removed").len(), 1);
    }

    #[test]
    fn a_session_that_died_is_restarted() {
        let w = world(&["a"]);
        let (mut s, log) = sup(&w);
        s.scan();
        w.spawned.lock().unwrap()[0].2.store(true, Ordering::SeqCst);
        s.scan();
        assert_eq!(spawned(&w), ["a", "a"]);
        assert_eq!(s.active(), ["a"]);
        assert_eq!(log.find("ended unexpectedly").len(), 1);
        s.stop_all();
    }

    #[test]
    fn a_failing_list_keeps_sessions_and_is_logged_once() {
        let w = world(&["a"]);
        let (mut s, log) = sup(&w);
        s.scan();
        *w.present.lock().unwrap() = Err("CONFIGRET 13".into());
        s.scan();
        s.scan();
        assert_eq!(s.active(), ["a"], "nothing is torn down on a failed scan");
        assert_eq!(log.find("cannot list").len(), 1);
        set(&w, &[]);
        s.scan();
        assert!(s.active().is_empty());
    }

    #[test]
    fn commands_stop_suspend_and_resume() {
        let w = world(&["a", "b"]);
        let (mut s, log) = sup(&w);
        let (tx, rx) = mpsc::channel();
        tx.send(Command::Suspend).unwrap();
        tx.send(Command::Suspend).unwrap();
        tx.send(Command::Resume).unwrap();
        tx.send(Command::Resume).unwrap();
        tx.send(Command::Stop).unwrap();
        s.run_every(&rx, Duration::from_secs(60));
        // Started at once, stopped for the suspend, restarted at the resume;
        // run returns with the sessions alive.
        assert_eq!(spawned(&w), ["a", "b", "a", "b"]);
        assert_eq!(*w.joined.lock().unwrap(), ["a", "b"]);
        assert_eq!(s.active(), ["a", "b"]);
        assert_eq!(log.find("system is suspending").len(), 1);
        assert_eq!(log.find("system resumed").len(), 1);
        // The host heard of the suspend once, after both sessions had ended.
        assert_eq!(*w.all_stopped.lock().unwrap(), [2]);
        s.stop_all();
        assert_eq!(w.joined.lock().unwrap().len(), 4);
    }

    #[test]
    fn rescans_periodically_and_ends_when_the_sender_is_gone() {
        let w = world(&[]);
        let (mut s, log) = sup(&w);
        let (tx, rx) = mpsc::channel::<Command>();
        let w2 = w.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            set(&w2, &["late"]);
            std::thread::sleep(Duration::from_millis(120));
            drop(tx);
        });
        s.run_every(&rx, Duration::from_millis(20));
        t.join().unwrap();
        assert_eq!(s.active(), ["late"], "found by a later rescan");
        assert_eq!(log.find("no crossc device interface present").len(), 1);
        s.stop_all();
    }

    #[test]
    fn no_scan_while_suspended() {
        let w = world(&["a"]);
        let (mut s, _) = sup(&w);
        let (tx, rx) = mpsc::channel();
        tx.send(Command::Suspend).unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            tx.send(Command::Stop).unwrap();
        });
        s.run_every(&rx, Duration::from_millis(10));
        t.join().unwrap();
        assert_eq!(spawned(&w), ["a"], "not restarted by the periodic rescan");
        assert!(s.active().is_empty());
    }

    #[test]
    fn short_names() {
        let p = r"\\?\BTHENUM#{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013#b&1a2b3c4d&0&AABBCCDDEEFF_C00000000#{433f9eba-036f-480d-826f-e0ec953319a9}";
        assert_eq!(short_name(p), "AABBCCDDEEFF");
        assert_eq!(
            short_name(&p.replace("AABBCCDDEEFF", "112233445566")),
            "112233445566"
        );
        assert_eq!(short_name("x"), "x");
        let long = "0123456789".repeat(5);
        assert_eq!(short_name(&long).len(), 24);
        assert_eq!(short_name(""), "");
    }
}
