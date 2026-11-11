//! Fakes for tests: an AVDTP sink that answers like a pair of Galaxy Buds,
//! and (in [`driver`]) a whole fake driver around it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use avdtp::caps::Capability;
use avdtp::msg;
use avdtp::signal::{Message, MessageType, Reassembler, SignalId, fragment};

use crate::channel::{ChanError, SduChannel};
use crate::log::{Level, Log};

pub mod driver;

/// A [`Log`] that keeps every line, stamped with a clock in microseconds
/// (the fake driver's virtual time, see `driver::FakeState::clock_us`).
#[derive(Default)]
pub struct MemoryLog {
    pub clock_us: Arc<std::sync::atomic::AtomicU64>,
    pub lines: Mutex<Vec<(u64, Level, String)>>,
}

impl MemoryLog {
    pub fn new(clock_us: Arc<std::sync::atomic::AtomicU64>) -> Self {
        MemoryLog {
            clock_us,
            lines: Mutex::default(),
        }
    }
    /// All lines as text, one per line: `[seconds] LEVEL message`.
    pub fn text(&self) -> String {
        let lines = self.lines.lock().unwrap_or_else(|p| p.into_inner());
        lines
            .iter()
            .map(|(t, l, m)| {
                format!(
                    "[{:9.3}] {} {m}
",
                    *t as f64 / 1e6,
                    l.name()
                )
            })
            .collect()
    }
    /// (milliseconds, message) of every line containing `needle`.
    pub fn find(&self, needle: &str) -> Vec<(u64, String)> {
        let lines = self.lines.lock().unwrap_or_else(|p| p.into_inner());
        lines
            .iter()
            .filter(|(_, _, m)| m.contains(needle))
            .map(|(t, _, m)| (t / 1000, m.clone()))
            .collect()
    }
}

impl Log for MemoryLog {
    fn log(&self, level: Level, msg: &str) {
        let t = self.clock_us.load(std::sync::atomic::Ordering::Relaxed);
        self.lines
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((t, level, msg.to_string()));
    }
}

/// Packets a test (or the fake media channel) wants the fake sink to
/// send on its own; `recv` pops these before regular replies.
pub type Inbox = Arc<Mutex<VecDeque<Vec<u8>>>>;

/// Fake A2DP sink: answers DISCOVER / GET_ALL_CAPABILITIES like a Galaxy
/// Buds would (SBC, AAC, SSC SEPs), optionally injecting a peer command
/// and fragmenting responses.
pub struct FakeSink {
    pub mtu: usize,
    pub out: VecDeque<Vec<u8>>,
    pub sent: Vec<Vec<u8>>,
    pub inject_peer_command: bool,
    pub no_get_all: bool,
    /// Go silent (no replies at all) once a command for this SEID arrives.
    pub silent_from_seid: Option<u8>,
    pub silent: bool,
    /// Reject SET_CONFIGURATION with this error code.
    pub reject_set_configuration: Option<u8>,
    pub reject_close: bool,
    /// (acp, int) SEIDs of an accepted SET_CONFIGURATION.
    pub configured: Option<(u8, u8)>,
    /// The capability list of the accepted SET_CONFIGURATION.
    pub configured_caps: Vec<Capability>,
    pub opened: bool,
    pub started: bool,
    /// SSC capability octet advertised by SEP 3 (default 0x16; Buds 4 Pro: 0x3E).
    pub ssc_flags: u8,
    /// Send a DELAYREPORT (this value, 1/10 ms) right after accepting
    /// SET_CONFIGURATION with Delay Reporting, like a real sink.
    pub delay_after_set_config: Option<u16>,
    /// Send an ABORT for our SEID right after accepting SET_CONFIGURATION
    /// (and forget the configuration, like a real sink would).
    pub abort_after_set_config: bool,
    /// Reject OPEN with this error code.
    pub reject_open: Option<u8>,
    /// Reject START with this error code.
    pub reject_start: Option<u8>,
    /// Reject SUSPEND with this error code.
    pub reject_suspend: Option<u8>,
    /// The channel is gone: `send` and `recv` fail with `Disconnected`.
    pub disconnected: bool,
    /// SEP 3 exists (clear it for a sink without any SSC end point).
    pub ssc_sep: bool,
    /// Set this flag when a command with this signal id arrives (simulates
    /// Ctrl+C pressed while that step is in flight).
    pub stop_on: Option<(SignalId, Arc<std::sync::atomic::AtomicBool>)>,
    /// When a command with this signal id arrives, the sink first sends this
    /// command of its own (once): the two cross on the channel. A START sent
    /// this way leaves the sink streaming, like any START it sends.
    pub cross_with: Option<(SignalId, Message)>,
    /// Sink-initiated packets, see [`Inbox`].
    pub inbox: Inbox,
    /// Every complete command message received (in order).
    pub commands: Vec<msg::Command>,
    rx: Reassembler,
}

impl FakeSink {
    pub fn new(mtu: usize) -> Self {
        Self {
            mtu,
            out: VecDeque::new(),
            sent: Vec::new(),
            inject_peer_command: false,
            no_get_all: false,
            silent_from_seid: None,
            silent: false,
            reject_set_configuration: None,
            reject_close: false,
            configured: None,
            configured_caps: Vec::new(),
            opened: false,
            started: false,
            ssc_flags: 0x16,
            delay_after_set_config: None,
            abort_after_set_config: false,
            reject_open: None,
            reject_start: None,
            reject_suspend: None,
            disconnected: false,
            ssc_sep: true,
            stop_on: None,
            cross_with: None,
            inbox: Inbox::default(),
            commands: Vec::new(),
            rx: Reassembler::new(),
        }
    }
    fn reply(&mut self, m: Message) {
        for p in fragment(&m, self.mtu).unwrap() {
            self.out.push_back(p);
        }
    }
}

pub const SBC: &[u8] = &[0x01, 0x00, 0x07, 0x06, 0x00, 0x00, 0x21, 0x15, 0x02, 0x35];
pub const AAC: &[u8] = &[
    0x01, 0x00, 0x07, 0x08, 0x00, 0x02, 0x80, 0x01, 0x8C, 0x84, 0xE2, 0x00, 0x08, 0x00,
];
pub const SSC: &[u8] = &[
    0x01, 0x00, 0x04, 0x02, 0x02, 0x00, 0x07, 0x09, 0x00, 0xFF, 0x75, 0x00, 0x00, 0x00, 0x03, 0x01,
    0x16, 0x08, 0x00,
];

/// SEP 3's capabilities with the given SSC flags octet.
pub fn ssc_caps(flags: u8) -> Vec<u8> {
    let mut v = SSC.to_vec();
    v[16] = flags;
    v
}

impl SduChannel for FakeSink {
    fn send(&mut self, sdu: &[u8]) -> Result<(), ChanError> {
        if self.disconnected {
            return Err(ChanError::Disconnected);
        }
        self.sent.push(sdu.to_vec());
        let Some(m) = self
            .rx
            .push(sdu)
            .map_err(|e| ChanError::Protocol(e.to_string()))?
        else {
            return Ok(());
        };
        if m.message_type != MessageType::Command {
            return Ok(()); // our reply to the injected command
        }
        if self.inject_peer_command {
            self.inject_peer_command = false;
            self.out.push_back(msg::discover(0xF).encode_single());
        }
        if let Some((_, own)) = self.cross_with.take_if(|(sigid, _)| *sigid == m.signal) {
            self.started |= own.signal == SignalId::Start;
            self.out.push_back(own.encode_single());
        }
        let cmd = msg::parse_command(&m).unwrap();
        self.commands.push(cmd.clone());
        if let Some((sigid, flag)) = &self.stop_on
            && *sigid == m.signal
        {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let msg::Command::GetAllCapabilities { acp_seid }
        | msg::Command::GetCapabilities { acp_seid } = cmd
            && self.silent_from_seid == Some(acp_seid)
        {
            self.silent = true;
        }
        if self.silent {
            return Ok(());
        }
        match cmd {
            msg::Command::SetConfiguration {
                acp_seid,
                int_seid,
                config,
            } => match self.reject_set_configuration {
                Some(e) => self.reply(Message::new(
                    m.label,
                    MessageType::ResponseReject,
                    m.signal,
                    vec![0x07, e],
                )),
                None => {
                    self.configured = Some((acp_seid, int_seid));
                    let dr = config.contains(&Capability::DelayReporting);
                    self.configured_caps = config;
                    self.reply(msg::accept(m.label, m.signal, Vec::new()));
                    if self.abort_after_set_config {
                        self.out
                            .push_back(msg::abort(0x8, int_seid).encode_single());
                        self.configured = None;
                    }
                    if let (true, Some(d)) = (dr, self.delay_after_set_config) {
                        self.out
                            .push_back(msg::delay_report(0x9, int_seid, d).encode_single());
                    }
                }
            },
            msg::Command::Open { .. } if self.reject_open.is_some() => {
                let e = self.reject_open.unwrap();
                self.reply(msg::reject(m.label, m.signal, e))
            }
            msg::Command::Open { acp_seid }
                if self.configured.is_some_and(|(a, _)| a == acp_seid) =>
            {
                self.opened = true;
                self.reply(msg::accept(m.label, m.signal, Vec::new()))
            }
            msg::Command::Start { ref acp_seids } if self.reject_start.is_some() => {
                let seid = acp_seids.first().copied().unwrap_or(0);
                let e = self.reject_start.unwrap();
                self.reply(Message::new(
                    m.label,
                    MessageType::ResponseReject,
                    m.signal,
                    vec![seid << 2, e],
                ))
            }
            msg::Command::Suspend { ref acp_seids } if self.reject_suspend.is_some() => {
                let seid = acp_seids.first().copied().unwrap_or(0);
                let e = self.reject_suspend.unwrap();
                self.reply(Message::new(
                    m.label,
                    MessageType::ResponseReject,
                    m.signal,
                    vec![seid << 2, e],
                ))
            }
            msg::Command::Suspend { ref acp_seids }
                if self.started && self.configured.is_some_and(|(a, _)| acp_seids == &[a]) =>
            {
                self.started = false;
                self.reply(msg::accept(m.label, m.signal, Vec::new()))
            }
            // START on a stream that is not OPEN (not opened yet, or already
            // streaming), SUSPEND on one that is not streaming: BAD_STATE.
            msg::Command::Start { ref acp_seids } if !self.opened || self.started => {
                let seid = acp_seids.first().copied().unwrap_or(0);
                self.reply(Message::new(
                    m.label,
                    MessageType::ResponseReject,
                    m.signal,
                    vec![seid << 2, 0x31],
                ))
            }
            msg::Command::Suspend { ref acp_seids } => {
                let seid = acp_seids.first().copied().unwrap_or(0);
                self.reply(Message::new(
                    m.label,
                    MessageType::ResponseReject,
                    m.signal,
                    vec![seid << 2, 0x31],
                ))
            }
            msg::Command::Start { ref acp_seids }
                if self.opened && self.configured.is_some_and(|(a, _)| acp_seids == &[a]) =>
            {
                self.started = true;
                self.reply(msg::accept(m.label, m.signal, Vec::new()))
            }
            // OPEN on an unconfigured SEP: BAD_STATE.
            msg::Command::Open { .. } => self.reply(msg::reject(m.label, m.signal, 0x31)),
            msg::Command::Close { .. } if self.reject_close => {
                self.reply(msg::reject(m.label, m.signal, 0x31))
            }
            msg::Command::Close { .. } | msg::Command::Abort { .. } => {
                self.opened = false;
                self.started = false;
                self.configured = None;
                self.reply(msg::accept(m.label, m.signal, Vec::new()))
            }
            msg::Command::Discover => self.reply(msg::accept(
                m.label,
                m.signal,
                vec![0x04, 0x08, 0x08, 0x08, 0x0E, 0x08],
            )),
            msg::Command::GetAllCapabilities { .. } if self.no_get_all => {
                self.reply(msg::general_reject(m.label, m.signal))
            }
            msg::Command::GetAllCapabilities { acp_seid }
            | msg::Command::GetCapabilities { acp_seid } => {
                let p = match acp_seid {
                    1 => SBC,
                    2 => AAC,
                    3 if self.ssc_sep => &ssc_caps(self.ssc_flags)[..],
                    _ => {
                        self.reply(msg::reject(m.label, m.signal, 0x12));
                        return Ok(());
                    }
                };
                self.reply(msg::accept(m.label, m.signal, p.to_vec()))
            }
            _ => self.reply(msg::reject(m.label, m.signal, 0x19)),
        }
        Ok(())
    }
    fn recv(&mut self, _t: Duration) -> Result<Vec<u8>, ChanError> {
        if self.disconnected {
            return Err(ChanError::Disconnected);
        }
        if let Some(p) = self.inbox.lock().unwrap().pop_front() {
            return Ok(p);
        }
        self.out.pop_front().ok_or(ChanError::Timeout)
    }
    fn out_mtu(&self) -> usize {
        self.mtu
    }
}
