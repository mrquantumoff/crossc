//! The named pipe that carries the snapshots.
//!
//! **Server** ([`Server`], in the service). The pipe is created outbound
//! only: the server never reads, so nothing a client sends can reach the
//! service. One thread owns every pipe instance and writes with overlapped
//! I/O; the service's own threads only touch the [`Hub`]. A client whose
//! previous snapshot has still not been taken when the next one is due is
//! disconnected (the pipe's buffer holds many snapshots, so that is a
//! client that stopped reading). At most [`ServerOptions::max_clients`]
//! are connected, at most [`ServerOptions::max_per_session`] of them from
//! one Windows session; while nobody is, the thread sleeps.
//!
//! The security descriptor ([`Security::Service`]) gives SYSTEM and
//! Administrators full control and interactive / authenticated users read
//! access: enough to connect and read, not enough to create an instance of
//! the pipe, to write, or to change it. Remote clients are rejected, and
//! the server never impersonates a client. The first instance is created
//! with FILE_FLAG_FIRST_PIPE_INSTANCE: if another process already holds
//! the name, the server does not join it but reports that and retries.
//! Once it has the name it keeps it: from then on it always holds at least
//! one instance (an instance whose client left is used again, not closed),
//! so no other process can create the pipe while the server runs.
//!
//! **Client** ([`Client`], [`Connection`], [`read_once`]). Opens the pipe
//! read-only at anonymous impersonation level (whoever serves the pipe
//! cannot act as the user) and, before it believes a byte, checks whose
//! pipe it is ([`Trust`]): the service's pipe is owned by SYSTEM or
//! Administrators, and a process of a plain user can neither create such
//! a pipe nor make itself look like one. That covers the times the service
//! does not hold the name (not started yet, stopped). It reads in byte
//! mode and cuts messages by their
//! length prefix. A server that says nothing for
//! [`ClientOptions::silence`] counts as gone. [`Client`] reconnects with
//! [`retry_delay`] and reports each problem once.
//!
//! Both sides take the pipe's name as an option, so the tests run them
//! against each other in one process.

use std::io;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING,
    ERROR_MORE_DATA, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, HANDLE,
    HLOCAL, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    EqualSid, GetKernelObjectSecurity, GetSecurityDescriptorOwner, GetTokenInformation,
    IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE,
    OPEN_EXISTING, PIPE_ACCESS_OUTBOUND, ReadFile, SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT,
    WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientSessionId,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, INFINITE, OpenProcessToken, ResetEvent, SetEvent,
    WaitForMultipleObjects,
};
use windows::core::{HSTRING, PCWSTR};

use crate::hub::{Hub, IDLE_INTERVAL, MIN_GAP, Pace, Pacer, WakerId};
use crate::model::Snapshot;
use crate::wire::{self, DecodeError, FrameReader};

/// SYSTEM and Administrators: everything. Interactive and authenticated
/// users: read (connect and read; not FILE_CREATE_PIPE_INSTANCE, not
/// write, not WRITE_DAC). Protected: nothing is inherited.
pub const SERVICE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;IU)(A;;GR;;;AU)";

/// Clients served at once, unless the options say otherwise.
pub const DEFAULT_MAX_CLIENTS: usize = 8;
/// Of them, from one Windows session (a tray with its flyout open is two).
pub const DEFAULT_MAX_PER_SESSION: usize = 4;
/// The most clients a server can be asked to serve.
pub const MAX_CLIENTS_LIMIT: usize = 32;
/// The pipe's outbound buffer per client: room for dozens of snapshots.
pub const DEFAULT_OUT_BUFFER: u32 = 64 * 1024;
/// A failed attempt to create the pipe is repeated after this long.
pub const LISTEN_RETRY: Duration = Duration::from_secs(5);

fn is(e: &windows::core::Error, code: windows::Win32::Foundation::WIN32_ERROR) -> bool {
    e.code() == code.to_hresult()
}

fn io_err(e: windows::core::Error) -> io::Error {
    io::Error::other(e.message())
}

/// A kernel handle, closed when dropped.
struct Handle(HANDLE);

// SAFETY: a kernel handle may be used from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and still open.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn new_event(manual_reset: bool) -> windows::core::Result<Handle> {
    // SAFETY: plain call, no pointers.
    unsafe { CreateEventW(None, manual_reset, false, PCWSTR::null()) }.map(Handle)
}

fn ms(d: Duration) -> u32 {
    // Rounded up, so a wait never ends before what it waits for; never
    // INFINITE by accident.
    u32::try_from(d.as_nanos().div_ceil(1_000_000)).map_or(INFINITE - 1, |v| v.min(INFINITE - 1))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// One overlapped operation at a time on one pipe handle.
struct Channel {
    pipe: Handle,
    event: Handle,
    /// Boxed: its address is the kernel's while an operation is pending.
    ov: Box<OVERLAPPED>,
    /// An operation is pending (or has completed without us having seen it).
    busy: bool,
    /// What a pending write writes.
    out: Option<Arc<[u8]>>,
    /// Server side: the Windows session of the connected client.
    session: Option<u32>,
}

// SAFETY: handles, a heap block and a shared buffer; the OVERLAPPED's pointers
// are the kernel's, not aliases of anything of ours.
unsafe impl Send for Channel {}

impl Channel {
    fn new(pipe: Handle) -> windows::core::Result<Channel> {
        Ok(Channel {
            pipe,
            event: new_event(true)?,
            ov: Box::default(),
            busy: false,
            out: None,
            session: None,
        })
    }

    fn arm(&mut self) {
        *self.ov = OVERLAPPED {
            hEvent: self.event.0,
            ..OVERLAPPED::default()
        };
        // SAFETY: the event is ours.
        unsafe {
            let _ = ResetEvent(self.event.0);
        }
    }

    /// The result of the pending operation: bytes transferred.
    fn result(&mut self, wait: bool) -> windows::core::Result<u32> {
        let mut n = 0u32;
        // SAFETY: `ov` is the structure the operation was started with.
        let r = unsafe { GetOverlappedResult(self.pipe.0, &*self.ov, &mut n, wait) };
        match r {
            Ok(()) => {
                self.busy = false;
                Ok(n)
            }
            Err(e) => {
                // "Not finished yet" leaves it pending; anything else ended it.
                if !is(&e, windows::Win32::Foundation::ERROR_IO_INCOMPLETE) {
                    self.busy = false;
                }
                Err(e)
            }
        }
    }

    /// End whatever is pending and wait until the kernel has let go of
    /// `ov` and the buffer.
    fn cancel(&mut self) {
        if self.busy {
            // SAFETY: handle and structure are ours; waiting after the
            // cancel is what makes freeing them safe.
            unsafe {
                let _ = CancelIoEx(self.pipe.0, Some(&*self.ov));
            }
            let _ = self.result(true);
            self.busy = false;
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.cancel();
    }
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

/// Who may open the pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    /// [`SERVICE_SDDL`]: for the service (it runs as SYSTEM).
    Service,
    /// The default descriptor: the creating user (and SYSTEM and
    /// Administrators). For tests and for the service run in a console.
    CurrentUser,
}

/// Called with one line when something is wrong (and when it is right
/// again). From the server's thread.
pub type Report = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct ServerOptions {
    pub pipe_name: String,
    pub security: Security,
    /// Clients served at once (1..=[`MAX_CLIENTS_LIMIT`]).
    pub max_clients: usize,
    /// Of these, at most this many from one Windows session: a logged-on
    /// user's programs cannot take the places of another user's. One more
    /// is disconnected as soon as it connects.
    pub max_per_session: usize,
    /// The pipe's outbound buffer per client, bytes.
    pub out_buffer: u32,
    pub report: Option<Report>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            pipe_name: crate::PIPE_NAME.to_string(),
            security: Security::Service,
            max_clients: DEFAULT_MAX_CLIENTS,
            max_per_session: DEFAULT_MAX_PER_SESSION,
            out_buffer: DEFAULT_OUT_BUFFER,
            report: None,
        }
    }
}

/// A security descriptor made from SDDL.
struct Descriptor(PSECURITY_DESCRIPTOR);

// SAFETY: an immutable block of memory.
unsafe impl Send for Descriptor {}

impl Descriptor {
    fn from_sddl(sddl: &str) -> windows::core::Result<Descriptor> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sd` receives a LocalAlloc'ed descriptor on success.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )?;
        }
        Ok(Descriptor(sd))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by the conversion with LocalAlloc.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

/// A new instance of the pipe, not yet connected. `first`: fail if the
/// name exists already (somebody else's pipe).
fn create_instance(
    name: &str,
    first: bool,
    descriptor: Option<&Descriptor>,
    max_instances: u32,
    out_buffer: u32,
) -> windows::core::Result<Handle> {
    let attributes = descriptor.map(|d| SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: d.0.0,
        bInheritHandle: false.into(),
    });
    let mut open_mode = PIPE_ACCESS_OUTBOUND | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: the attributes (if any) outlive the call.
    let h = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(name),
            open_mode,
            PIPE_TYPE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            max_instances,
            out_buffer,
            0,
            0,
            attributes.as_ref().map(std::ptr::from_ref),
        )
    };
    if h.is_invalid() {
        return Err(windows::core::Error::from_thread());
    }
    Ok(Handle(h))
}

/// Put an instance (new, or disconnected from its last client) into the
/// listening state.
fn start_listening(ch: &mut Channel) -> windows::core::Result<()> {
    ch.arm();
    ch.out = None;
    ch.session = None;
    let signal = |ch: &Channel| {
        // SAFETY: the event is ours.
        unsafe {
            let _ = SetEvent(ch.event.0);
        }
    };
    // SAFETY: `ov` lives in the channel, which cancels before it is freed.
    match unsafe { ConnectNamedPipe(ch.pipe.0, Some(&mut *ch.ov)) } {
        Ok(()) => signal(ch),
        Err(e) if is(&e, ERROR_IO_PENDING) => ch.busy = true,
        // A client got in between the creation of the instance and this
        // call, and may even have left again already (ERROR_NO_DATA).
        // Either way the instance counts as connected: `accept` finds out,
        // and puts the same instance back to listening if the client is
        // gone. It is never closed for that: closing it could leave the
        // name without an instance.
        Err(e) if is(&e, ERROR_PIPE_CONNECTED) || is(&e, ERROR_NO_DATA) => signal(ch),
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Write `frame` to a client. `false`: the client is gone or does not
/// read, drop it.
fn write(ch: &mut Channel, frame: &Arc<[u8]>) -> bool {
    if ch.busy && ch.result(false).is_err() {
        // The previous snapshot is still not taken (or failed).
        return false;
    }
    ch.arm();
    ch.out = Some(frame.clone());
    // SAFETY: buffer and `ov` live in the channel, which cancels the
    // write and waits for it before either is freed.
    match unsafe { WriteFile(ch.pipe.0, Some(frame), None, Some(&mut *ch.ov)) } {
        Ok(()) => true,
        Err(e) if is(&e, ERROR_IO_PENDING) => {
            ch.busy = true;
            true
        }
        Err(_) => false,
    }
}

/// End the connection of an instance; the instance stays.
fn disconnect(ch: &mut Channel) {
    ch.cancel();
    ch.out = None;
    // SAFETY: the server end of a pipe.
    unsafe {
        let _ = DisconnectNamedPipe(ch.pipe.0);
    }
}

/// The Windows session of the client on the other end, if it can be told.
fn client_session(ch: &Channel) -> Option<u32> {
    let mut id = 0u32;
    // SAFETY: the server end of a pipe and a u32 to fill.
    unsafe { GetNamedPipeClientSessionId(ch.pipe.0, &mut id) }
        .ok()
        .map(|()| id)
}

/// Whether a [`Server`] has the pipe, as its thread last found. Cheap to
/// clone; all clones are the same value.
#[derive(Clone, Debug, Default)]
pub struct PipeHeld(Arc<std::sync::atomic::AtomicU8>);

/// What [`PipeHeld::get`] says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    /// The server's thread has not tried to create the pipe yet.
    NotYet,
    /// The server holds the pipe: whoever opens that name gets this
    /// server.
    Ours,
    /// The name exists and is not this server's: another process created
    /// it first. Whoever opens that name gets that process.
    Taken,
    /// The pipe could not be created for another reason; no pipe of that
    /// name is known to exist.
    Failed,
}

impl PipeHeld {
    pub fn get(&self) -> Held {
        match self.0.load(std::sync::atomic::Ordering::SeqCst) {
            1 => Held::Ours,
            2 => Held::Taken,
            3 => Held::Failed,
            _ => Held::NotYet,
        }
    }

    fn set(&self, held: Held) {
        let v = match held {
            Held::NotYet => 0,
            Held::Ours => 1,
            Held::Taken => 2,
            Held::Failed => 3,
        };
        self.0.store(v, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The server's thread.
///
/// From the moment the first instance of the pipe exists until the thread
/// ends, the thread holds at least one instance: the name is never free
/// for another process to take (FILE_FLAG_FIRST_PIPE_INSTANCE only guards
/// the creation; a name with no instance left is anybody's). So an
/// instance whose client is gone is not closed but disconnected and used
/// again ([`ServerLoop::recycle`]), and is closed only while another one
/// is held.
struct ServerLoop {
    hub: Hub,
    options: ServerOptions,
    stop: Arc<Handle>,
    wake: Arc<Handle>,
    waker: WakerId,
    descriptor: Option<Descriptor>,
    clients: Vec<Channel>,
    listener: Option<Channel>,
    /// An instance that could not be made to listen: it only keeps the
    /// name, until the next attempt.
    parked: Option<Channel>,
    /// No listener could be made; no new attempt before this time.
    listen_retry: Option<Instant>,
    listen_failed: bool,
    /// The state does not fit a message (reported once).
    encode_failed: bool,
    /// When the clients were last written to only to find the dead ones.
    swept: Option<Instant>,
    pacer: Pacer,
    seq: u64,
    started: Instant,
    /// For whoever asks whether the pipe is this server's.
    held: PipeHeld,
}

impl ServerLoop {
    fn report(&self, line: &str) {
        if let Some(r) = &self.options.report {
            r(line);
        }
    }

    fn max_clients(&self) -> usize {
        self.options.max_clients.clamp(1, MAX_CLIENTS_LIMIT)
    }

    fn listening(&mut self, ch: Channel) {
        if self.listen_failed {
            self.report(&format!(
                "status pipe {} is available now",
                self.options.pipe_name
            ));
        }
        self.listen_failed = false;
        self.listen_retry = None;
        self.listener = Some(ch);
    }

    /// No listener could be made. `holding`: the server has an instance
    /// of the pipe all the same (clients are being served).
    fn not_listening(&mut self, e: &windows::core::Error, holding: bool) {
        if !self.listen_failed {
            let name = &self.options.pipe_name;
            let retry = LISTEN_RETRY.as_secs();
            self.report(&if holding {
                format!(
                    "status pipe {name}: no instance for a further client ({}); the connected \
                     ones are still served. Retrying every {retry} s",
                    e.message()
                )
            } else {
                format!(
                    "status pipe {name} cannot be created ({}); user interfaces get no status. \
                     Retrying every {retry} s",
                    if is(e, ERROR_ACCESS_DENIED) {
                        "another process holds that name".to_string()
                    } else {
                        e.message()
                    }
                )
            });
        }
        self.listen_failed = true;
        self.listen_retry = Some(Instant::now() + LISTEN_RETRY);
    }

    /// Have an instance listening, unless as many clients are connected
    /// as may be. `pending`: clients about to be added to `clients`.
    fn ensure_listener(&mut self, pending: usize) {
        if self.listener.is_some() || self.clients.len() + pending >= self.max_clients() {
            return;
        }
        if self.listen_retry.is_some_and(|t| Instant::now() < t) {
            return;
        }
        if let Some(mut ch) = self.parked.take() {
            disconnect(&mut ch);
            match start_listening(&mut ch) {
                Ok(()) => self.listening(ch),
                Err(e) => {
                    self.parked = Some(ch);
                    self.not_listening(&e, true);
                }
            }
            return;
        }
        // "First" exactly when this thread holds no instance: then the
        // name must not exist yet (else it is somebody else's pipe), and
        // otherwise it must (it is ours).
        let holding = !self.clients.is_empty() || pending > 0;
        let created = create_instance(
            &self.options.pipe_name,
            !holding,
            self.descriptor.as_ref(),
            (self.max_clients() + 1) as u32,
            self.options.out_buffer,
        )
        .and_then(Channel::new);
        match created {
            Ok(mut ch) => {
                // From here on the thread always holds an instance.
                self.held.set(Held::Ours);
                match start_listening(&mut ch) {
                    Ok(()) => self.listening(ch),
                    Err(e) => {
                        // Keep it: it holds the name.
                        self.parked = Some(ch);
                        self.not_listening(&e, true);
                    }
                }
            }
            Err(e) => {
                if !holding {
                    // With FILE_FLAG_FIRST_PIPE_INSTANCE, "access denied"
                    // is what a name that exists already gets.
                    self.held.set(if is(&e, ERROR_ACCESS_DENIED) {
                        Held::Taken
                    } else {
                        Held::Failed
                    });
                }
                self.not_listening(&e, holding)
            }
        }
    }

    /// The client of `ch` is gone (or is not wanted): disconnect it and
    /// use the instance again as the listener. It is closed only if
    /// another instance already listens.
    fn recycle(&mut self, mut ch: Channel) {
        disconnect(&mut ch);
        if self.listener.is_some() || self.parked.is_some() {
            return; // closed here; the other one keeps the name
        }
        match start_listening(&mut ch) {
            Ok(()) => self.listening(ch),
            Err(e) => {
                self.parked = Some(ch);
                self.not_listening(&e, true);
            }
        }
    }

    fn frame(&mut self, mut snapshot: Snapshot) -> Option<Arc<[u8]>> {
        self.seq += 1;
        snapshot.seq = self.seq;
        snapshot.time_unix = unix_now();
        match wire::encode(&snapshot) {
            Ok(bytes) => {
                self.encode_failed = false;
                Some(bytes.into())
            }
            Err(e) => {
                // Once, not with every attempt: the report may itself end
                // up in the state.
                if !self.encode_failed {
                    self.report(&format!("status snapshot not sent: {e}"));
                }
                self.encode_failed = true;
                None
            }
        }
    }

    fn broadcast(&mut self, snapshot: Snapshot) {
        let now = self.started.elapsed();
        if let Some(frame) = self.frame(snapshot.clone()) {
            let clients = std::mem::take(&mut self.clients);
            for mut ch in clients {
                if write(&mut ch, &frame) {
                    self.clients.push(ch);
                } else {
                    self.recycle(ch);
                }
            }
        }
        self.pacer.sent(now, snapshot);
    }

    /// Clients of the Windows session `session` (none if it is not known).
    fn of_session(&self, session: Option<u32>) -> usize {
        self.clients
            .iter()
            .filter(|c| session.is_some() && c.session == session)
            .count()
    }

    /// The listener's connect completed: it is a client now.
    fn accept(&mut self) {
        let Some(mut ch) = self.listener.take() else {
            return;
        };
        if ch.busy && ch.result(false).is_err() {
            // The connect failed: listen again on the same instance.
            self.recycle(ch);
            return;
        }
        // The next client's instance first: the time in which the pipe
        // has none listening (and a client is told "busy") is kept short.
        self.ensure_listener(1);
        ch.session = client_session(&ch);
        let limit = self.options.max_per_session.max(1);
        if self.of_session(ch.session) >= limit && self.swept.is_none_or(|t| t.elapsed() >= MIN_GAP)
        {
            // Some of them may have left: that shows only when they are
            // written to. (Not more often than snapshots go out anyway.)
            self.swept = Some(Instant::now());
            self.broadcast(self.hub.snapshot());
        }
        if self.of_session(ch.session) >= limit {
            // One session does not take the places of the others.
            self.recycle(ch);
            return;
        }
        // The newcomer gets the state at once, whatever the pace.
        let snapshot = self.hub.snapshot();
        match self.frame(snapshot) {
            Some(frame) if write(&mut ch, &frame) => self.clients.push(ch),
            // Gone already (or nothing to send): the instance is kept.
            _ => self.recycle(ch),
        }
    }

    fn run(mut self) {
        loop {
            self.ensure_listener(0);
            let mut timeout = INFINITE;
            let serving = !self.clients.is_empty();
            if serving {
                let snapshot = self.hub.snapshot();
                match self.pacer.next(self.started.elapsed(), &snapshot) {
                    Pace::Send => {
                        self.broadcast(snapshot);
                        continue;
                    }
                    Pace::Wait(d) => timeout = ms(d),
                }
            }
            if let Some(t) = self.listen_retry {
                timeout = timeout.min(ms(t.saturating_duration_since(Instant::now())).max(1));
            }
            // With nobody connected, a change of the state is of no
            // interest: the thread sleeps until a client arrives.
            let mut handles = vec![self.stop.0];
            if let Some(l) = &self.listener {
                handles.push(l.event.0);
            }
            if serving {
                handles.push(self.wake.0);
            }
            // SAFETY: the handles are open for the duration of the call.
            let r = unsafe { WaitForMultipleObjects(&handles, false, timeout) };
            if r == WAIT_OBJECT_0 {
                break;
            }
            if r == WAIT_TIMEOUT {
                continue;
            }
            let index = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            match handles.get(index) {
                Some(h) if self.listener.as_ref().is_some_and(|l| l.event.0 == *h) => self.accept(),
                Some(_) => {} // a change: the pacer decides at the top
                None => {
                    // WAIT_FAILED or an abandoned handle: do not spin.
                    self.report("status pipe: waiting failed; the status server stops");
                    break;
                }
            }
        }
        self.hub.remove_waker(self.waker);
        for mut ch in self.clients.drain(..) {
            disconnect(&mut ch);
        }
    }
}

/// The service's end: a thread that sends the hub's state to whoever is
/// connected. Stops when dropped.
pub struct Server {
    stop: Arc<Handle>,
    thread: Option<JoinHandle<()>>,
    held: PipeHeld,
}

impl Server {
    /// Start serving `hub`. Fails only if the thread or its events cannot
    /// be created; a pipe that cannot be created is reported through
    /// [`ServerOptions::report`] and retried, the service is not held up.
    pub fn start(hub: Hub, options: ServerOptions) -> io::Result<Server> {
        let stop = Arc::new(new_event(true).map_err(io_err)?);
        let wake = Arc::new(new_event(false).map_err(io_err)?);
        let descriptor = match options.security {
            Security::Service => Some(Descriptor::from_sddl(SERVICE_SDDL).map_err(io_err)?),
            Security::CurrentUser => None,
        };
        let event = wake.clone();
        // Taken out again when the thread ends (`ServerLoop::run`).
        let waker = hub.on_change(move || {
            // SAFETY: the event lives as long as this closure.
            unsafe {
                let _ = SetEvent(event.0);
            }
        });
        let unregister = hub.clone();
        let held = PipeHeld::default();
        let state = ServerLoop {
            held: held.clone(),
            hub,
            options,
            stop: stop.clone(),
            wake,
            waker,
            descriptor,
            clients: Vec::new(),
            listener: None,
            parked: None,
            listen_retry: None,
            listen_failed: false,
            encode_failed: false,
            swept: None,
            pacer: Pacer::new(),
            seq: 0,
            started: Instant::now(),
        };
        let thread = std::thread::Builder::new()
            .name("status-pipe".into())
            .spawn(move || state.run())
            .inspect_err(|_| unregister.remove_waker(waker))?;
        Ok(Server {
            stop,
            thread: Some(thread),
            held,
        })
    }

    /// Whether this server has the pipe (it creates it on its own thread,
    /// and keeps trying while another process holds the name).
    pub fn pipe_held(&self) -> PipeHeld {
        self.held.clone()
    }

    /// Disconnect the clients and end the thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(t) = self.thread.take() {
            // SAFETY: the event is ours.
            unsafe {
                let _ = SetEvent(self.stop.0);
            }
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// client
// ---------------------------------------------------------------------------

/// Why there is no snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The pipe does not exist: the service is not running (or is an older
    /// one without a status pipe).
    NotRunning,
    /// The service serves as many clients as it will.
    Busy,
    /// The pipe exists but may not be opened by this user.
    Denied,
    /// The pipe is not the service's: another program created it (see
    /// [`Trust`]). Nothing was read from it.
    Untrusted,
    /// The service speaks another version of the protocol.
    Version { theirs: u32 },
    /// What came was not a snapshot.
    Malformed(String),
    /// Connected, but nothing arrived for the time allowed.
    Silent,
    /// The connection ended (the service stopped or dropped us).
    Lost(String),
    /// [`Stop`] was requested.
    Stopped,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Problem::NotRunning => write!(f, "the crossc service is not running"),
            Problem::Busy => write!(f, "the crossc service is serving too many status clients"),
            Problem::Denied => write!(f, "access to the crossc service's status was denied"),
            Problem::Untrusted => write!(
                f,
                "the status pipe belongs to another program, not to the crossc service"
            ),
            Problem::Version { theirs } => write!(
                f,
                "the crossc service speaks status protocol version {theirs}, this program version {}",
                crate::PROTOCOL_VERSION
            ),
            Problem::Malformed(e) => write!(f, "the crossc service sent something unreadable: {e}"),
            Problem::Silent => write!(f, "the crossc service stopped sending its status"),
            Problem::Lost(e) => write!(f, "the connection to the crossc service ended: {e}"),
            Problem::Stopped => write!(f, "stopped"),
        }
    }
}

impl std::error::Error for Problem {}

/// Ends a blocked [`Connection::read`] and a [`Client`].
#[derive(Clone)]
pub struct Stop(Arc<Handle>);

impl Stop {
    pub fn new() -> io::Result<Stop> {
        Ok(Stop(Arc::new(new_event(true).map_err(io_err)?)))
    }
    pub fn request(&self) {
        // SAFETY: the event is ours.
        unsafe {
            let _ = SetEvent(self.0.0);
        }
    }
    /// Wait for the request, at most `timeout`. `true`: it was requested.
    pub fn wait(&self, timeout: Duration) -> bool {
        // SAFETY: the handle is open for the duration of the call.
        unsafe { WaitForMultipleObjects(&[self.0.0], false, ms(timeout)) == WAIT_OBJECT_0 }
    }
}

/// Whose pipe a client believes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Trust {
    /// Only a pipe owned by SYSTEM or by Administrators: one the service
    /// (LocalSystem, or run from an elevated console) created. A plain
    /// user's process cannot create a pipe with such an owner.
    #[default]
    Service,
    /// Also a pipe the current user owns: for the tests, the `serve`
    /// example and a pipe named on the command line.
    ServiceOrSelf,
}

/// Who owns a pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    System,
    Administrators,
    /// The user this process runs as.
    Me,
    Other,
}

fn trusted(owner: Owner, trust: Trust) -> bool {
    match owner {
        Owner::System | Owner::Administrators => true,
        Owner::Me => trust == Trust::ServiceOrSelf,
        Owner::Other => false,
    }
}

/// The SID of the user this process runs as, in a buffer of its own.
fn current_user(buf: &mut Vec<u64>) -> Option<PSID> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo handle of this process; `token` receives a handle
    // that is closed below.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    let token = Handle(token);
    let mut needed = 0u32;
    // SAFETY: a size query, then the same call with a buffer of that size
    // (u64s, so that the TOKEN_USER at its start is aligned).
    unsafe {
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            return None;
        }
        buf.resize((needed as usize).div_ceil(8), 0);
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
        .ok()?;
        Some((*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid)
    }
}

/// The owner of the pipe behind `pipe` (a handle with READ_CONTROL, which
/// GENERIC_READ includes). An error: it could not be read (the server has
/// disconnected this client already, for one).
fn pipe_owner(pipe: &Handle) -> windows::core::Result<Owner> {
    let mut needed = 0u32;
    // SAFETY: a size query, then the same call with a buffer of that size;
    // the owner SID points into that buffer, which outlives its use.
    unsafe {
        let _ = GetKernelObjectSecurity(pipe.0, OWNER_SECURITY_INFORMATION.0, None, 0, &mut needed);
        if needed == 0 {
            return Err(windows::core::Error::from_thread());
        }
        let mut sd = vec![0u64; (needed as usize).div_ceil(8)];
        let psd = PSECURITY_DESCRIPTOR(sd.as_mut_ptr().cast());
        GetKernelObjectSecurity(
            pipe.0,
            OWNER_SECURITY_INFORMATION.0,
            Some(psd),
            needed,
            &mut needed,
        )?;
        let mut owner = PSID::default();
        let mut defaulted = windows::core::BOOL::default();
        GetSecurityDescriptorOwner(psd, &mut owner, &mut defaulted)?;
        if owner.is_invalid() {
            return Ok(Owner::Other); // a pipe without an owner is nobody's
        }
        if IsWellKnownSid(owner, WinLocalSystemSid).as_bool() {
            return Ok(Owner::System);
        }
        if IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool() {
            return Ok(Owner::Administrators);
        }
        let mut buf = Vec::new();
        Ok(match current_user(&mut buf) {
            Some(me) if EqualSid(owner, me).is_ok() => Owner::Me,
            _ => Owner::Other,
        })
    }
}

/// How long [`Connection::open`] waits for a free instance of a busy pipe.
pub const BUSY_WAIT: Duration = Duration::from_millis(200);

/// An open connection to a status pipe.
pub struct Connection {
    ch: Channel,
    frames: FrameReader,
    buf: Box<[u8]>,
}

impl Connection {
    /// Connect to the pipe `name`, if it is one `trust` allows. Waits only
    /// if every instance of the pipe is taken at the moment (the server is
    /// just making the next one), and then at most [`BUSY_WAIT`].
    pub fn open(name: &str, trust: Trust) -> Result<Connection, Problem> {
        let name = HSTRING::from(name);
        let mut waited = false;
        let pipe = loop {
            // Read access only; anonymous impersonation level, so that the
            // process behind the pipe cannot act as this user.
            // SAFETY: plain call; the name outlives it.
            let r = unsafe {
                CreateFileW(
                    &name,
                    GENERIC_READ.0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS,
                    None,
                )
            };
            match r {
                Ok(h) => break Handle(h),
                // No such pipe. (Only this code: "path not found" is what a
                // name that is not a pipe's gets, and that is not a service
                // that does not run.)
                Err(e) if is(&e, ERROR_FILE_NOT_FOUND) => return Err(Problem::NotRunning),
                Err(e) if is(&e, ERROR_PIPE_BUSY) => {
                    // SAFETY: plain call; the name outlives it.
                    if waited || !unsafe { WaitNamedPipeW(&name, ms(BUSY_WAIT)) }.as_bool() {
                        return Err(Problem::Busy);
                    }
                    waited = true;
                }
                Err(e) if is(&e, ERROR_ACCESS_DENIED) => return Err(Problem::Denied),
                Err(e) => return Err(Problem::Lost(e.message())),
            }
        };
        // Whose pipe is it? Asked before anything is read; and if it
        // cannot be told, nothing is read either.
        match pipe_owner(&pipe) {
            Ok(owner) if trusted(owner, trust) => {}
            Ok(_) => return Err(Problem::Untrusted),
            Err(e) => return Err(Problem::Lost(e.message())),
        }
        let ch = Channel::new(pipe).map_err(|e| Problem::Lost(e.message()))?;
        Ok(Connection {
            ch,
            frames: FrameReader::new(),
            buf: vec![0u8; 8192].into_boxed_slice(),
        })
    }

    /// The next snapshot. Waits at most `timeout` for it, and not past a
    /// request of `stop`. After an error the connection is of no use.
    pub fn read(&mut self, timeout: Duration, stop: Option<&Stop>) -> Result<Snapshot, Problem> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.frames.next_frame() {
                Some(Ok(s)) => return Ok(s),
                Some(Err(DecodeError::Version { theirs })) => {
                    return Err(Problem::Version { theirs });
                }
                Some(Err(e)) => return Err(Problem::Malformed(e.to_string())),
                None => {}
            }
            let n = self.read_some(deadline, stop)?;
            self.frames.push(&self.buf[..n]);
        }
    }

    fn read_some(&mut self, deadline: Instant, stop: Option<&Stop>) -> Result<usize, Problem> {
        let lost = |e: windows::core::Error| {
            if is(&e, ERROR_BROKEN_PIPE) {
                Problem::Lost("the service closed it".into())
            } else {
                Problem::Lost(e.message())
            }
        };
        self.ch.arm();
        // SAFETY: buffer and `ov` live in `self`; a pending read is
        // cancelled and waited for before either is freed (below, and in
        // the channel's drop).
        let started = unsafe {
            ReadFile(
                self.ch.pipe.0,
                Some(&mut self.buf),
                None,
                Some(&mut *self.ch.ov),
            )
        };
        match started {
            Ok(()) => {}
            Err(e) if is(&e, ERROR_IO_PENDING) || is(&e, ERROR_MORE_DATA) => {}
            Err(e) => return Err(lost(e)),
        }
        self.ch.busy = true;
        let mut handles = vec![self.ch.event.0];
        if let Some(s) = stop {
            handles.push(s.0.0);
        }
        let wait = ms(deadline.saturating_duration_since(Instant::now()));
        // SAFETY: the handles are open for the duration of the call.
        let r = unsafe { WaitForMultipleObjects(&handles, false, wait) };
        if r != WAIT_OBJECT_0 {
            self.ch.cancel();
            return Err(if r == WAIT_TIMEOUT {
                Problem::Silent
            } else if r.0 == WAIT_OBJECT_0.0 + 1 {
                Problem::Stopped
            } else {
                Problem::Lost("waiting failed".into())
            });
        }
        match self.ch.result(false) {
            Ok(0) => Err(Problem::Lost("the service closed it".into())),
            Ok(n) => Ok(n as usize),
            // A message longer than the buffer: the rest comes next.
            Err(e) if is(&e, ERROR_MORE_DATA) => Ok(self.buf.len()),
            Err(e) => Err(lost(e)),
        }
    }
}

/// Connect to `name`, read one snapshot, disconnect.
pub fn read_once(name: &str, timeout: Duration, trust: Trust) -> Result<Snapshot, Problem> {
    Connection::open(name, trust)?.read(timeout, None)
}

/// How long to wait before connection attempt number `attempt` (0: the
/// first after a failure), given why the last one failed.
pub fn retry_delay(attempt: u32, problem: &Problem) -> Duration {
    match problem {
        // Nothing this program can do changes that; look rarely.
        Problem::Version { .. } => Duration::from_secs(30),
        _ => match attempt {
            0 => Duration::from_millis(500),
            1 => Duration::from_secs(1),
            2 => Duration::from_secs(2),
            _ => Duration::from_secs(5),
        },
    }
}

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub pipe_name: String,
    /// A connected server that sends nothing for this long counts as gone.
    pub silence: Duration,
    /// Whose pipe to believe.
    pub trust: Trust,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            pipe_name: crate::PIPE_NAME.to_string(),
            // The server sends at least every IDLE_INTERVAL.
            silence: IDLE_INTERVAL * 3,
            trust: Trust::Service,
        }
    }
}

/// What a [`Client`] tells its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientEvent {
    Snapshot(Box<Snapshot>),
    /// There is no status, and why. Reported once per cause, not once per
    /// attempt.
    Problem(Problem),
}

/// A thread that keeps a connection to the service and hands every
/// snapshot to a callback. Stops when dropped.
pub struct Client {
    stop: Stop,
    thread: Option<JoinHandle<()>>,
}

impl Client {
    /// `sink` is called from the client's thread.
    pub fn spawn(
        options: ClientOptions,
        mut sink: impl FnMut(ClientEvent) + Send + 'static,
    ) -> io::Result<Client> {
        let stop = Stop::new()?;
        let stopper = stop.clone();
        let thread = std::thread::Builder::new()
            .name("status-client".into())
            .spawn(move || {
                let mut reported: Option<Problem> = None;
                let mut attempt = 0u32;
                loop {
                    let problem = match Connection::open(&options.pipe_name, options.trust) {
                        Ok(mut conn) => loop {
                            match conn.read(options.silence, Some(&stopper)) {
                                Ok(s) => {
                                    attempt = 0;
                                    reported = None;
                                    sink(ClientEvent::Snapshot(Box::new(s)));
                                }
                                Err(p) => break p,
                            }
                        },
                        Err(p) => p,
                    };
                    if problem == Problem::Stopped {
                        return;
                    }
                    if reported.as_ref() != Some(&problem) {
                        reported = Some(problem.clone());
                        sink(ClientEvent::Problem(problem.clone()));
                    }
                    if stopper.wait(retry_delay(attempt, &problem)) {
                        return;
                    }
                    attempt = attempt.saturating_add(1);
                }
            })?;
        Ok(Client {
            stop,
            thread: Some(thread),
        })
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(t) = self.thread.take() {
            self.stop.request();
            let _ = t.join();
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// For the tests of this module: a server end that is not a [`Server`].
#[cfg(test)]
fn raw_server(name: &str) -> Handle {
    // Blocking, default security.
    // SAFETY: plain call.
    let h = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(name),
            PIPE_ACCESS_OUTBOUND,
            PIPE_TYPE_MESSAGE | PIPE_WAIT,
            4,
            65536,
            0,
            0,
            None,
        )
    };
    assert!(!h.is_invalid(), "{}", windows::core::Error::from_thread());
    Handle(h)
}

#[cfg(test)]
fn raw_write(pipe: &Handle, bytes: &[u8]) -> bool {
    let mut n = 0u32;
    // SAFETY: a blocking write of a live buffer.
    if unsafe { WriteFile(pipe.0, Some(bytes), Some(&mut n), None) }.is_err() {
        // The client has left already (one that only looked at who owns
        // the pipe does, at once).
        return false;
    }
    assert_eq!(n as usize, bytes.len());
    true
}

#[cfg(test)]
fn raw_connect(pipe: &Handle) {
    // SAFETY: a blocking connect.
    match unsafe { ConnectNamedPipe(pipe.0, None) } {
        Ok(()) => {}
        Err(e) if is(&e, ERROR_PIPE_CONNECTED) => {}
        Err(e) => panic!("{e}"),
    }
}

#[cfg(test)]
#[path = "pipe_tests.rs"]
mod tests;
