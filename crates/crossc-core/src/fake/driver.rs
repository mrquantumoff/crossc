//! A fake driver for session tests: the audio side of the contract (ATTACH,
//! link flag, stream state with sequence numbers, PCM chunks at the
//! endpoint's pace, discontinuities, detach), an L2CAP pipe whose signaling
//! channel is a [`FakeSink`] and whose media channel records what is written,
//! and the "are the buds connected" answer.
//!
//! Time is virtual: it only moves when the code under test waits. A test
//! scripts what happens when ([`FakeDriver::at`]) and then lets the session
//! run; everything is deterministic.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use avdtp::signal::Message;
use crossc_ioctl as ioc;
use crossc_ioctl::audio::{
    AUDIO_FLAG_ACX, AUDIO_FLAG_ATTACHED, AUDIO_FLAG_LINK, AudioState, PCM_FLAG_DISCONTINUITY,
    PcmFormat, PcmHeader, StreamState,
};

use super::FakeSink;
use crate::channel::{ChanError, SduChannel};
use crate::driver::{Driver, Interest, LinkProbe, PcmChunk, Wake};
use crate::pace::Clock;
use crate::stop::StopSignal;

/// Calls at one instant of virtual time before the fake declares a livelock.
const SPIN_LIMIT: u32 = 20_000;

type Action = Box<dyn FnOnce(&mut FakeState)>;

/// Everything the fake holds; tests read and change it through
/// [`FakeDriver::with`] and in scripted actions.
pub struct FakeState {
    pub now: Duration,
    /// `now` in microseconds, readable from anywhere (log timestamps).
    pub clock_us: Arc<AtomicU64>,
    pub stop: StopSignal,

    // -- driver identity ----------------------------------------------------
    pub protocol_version: u32,
    pub remote_addr: u64,
    pub addr_valid: bool,
    /// Fail GET_INFO with this Win32 code (a device that went away).
    pub info_error: Option<u32>,
    /// Fail ATTACH with this Win32 code (170 = busy).
    pub attach_error: Option<u32>,

    // -- audio --------------------------------------------------------------
    pub state: AudioState,
    /// The ATTACH request ended this way; reported by the next wait.
    detached: Option<ChanError>,
    /// Every SET_LINK, with its time.
    pub link_calls: Vec<(Duration, bool)>,
    /// Fail SET_LINK with this Win32 code.
    pub set_link_error: Option<u32>,
    /// Endpoint period: one PCM chunk of this length while running.
    pub period: Duration,
    /// Chunks the driver's ring holds for a slow reader. When it is full
    /// the oldest chunk makes room for the newest (the driver's ring is
    /// 250 ms; 16 chunks of 10 ms are in that region).
    pub ring_chunks: usize,
    pcm_next: Duration,
    /// The driver's ring: rendered PCM nobody has read yet.
    pcm_queue: VecDeque<PcmChunk>,
    /// A READ_PCM is outstanding (the reader waited for PCM and none was
    /// there); the next chunk completes it.
    read_armed: bool,
    /// The chunk a completed READ_PCM holds until the reader collects it.
    read_ahead: Option<PcmChunk>,
    discontinuity: bool,
    /// Chunks handed to the reader.
    pub chunks_read: u64,

    // -- bluetooth ----------------------------------------------------------
    /// What the link probe answers.
    pub connected: bool,
    pub probe_calls: u32,
    /// Make the link probe fail.
    pub probe_error: bool,

    // -- L2CAP --------------------------------------------------------------
    pub sink: FakeSink,
    next_channel: u32,
    pub signaling: Option<u32>,
    pub media: Option<u32>,
    /// OPEN_CHANNEL calls, successful or not, and when each was made.
    pub open_calls: u32,
    pub open_times: Vec<Duration>,
    /// Fail OPEN_CHANNEL with this Win32 code.
    pub open_error: Option<u32>,
    /// Fail the media channel's OPEN_CHANNEL only.
    pub media_open_error: Option<u32>,
    /// MTUs every OPEN_CHANNEL reports (default: what the real driver does,
    /// out_mtu echoing the requested in_mtu and in_mtu 65535).
    pub report_mtus: Option<(u16, u16)>,
    /// Media SDUs written, with the time of each.
    pub packets: Vec<Vec<u8>>,
    pub packet_times: Vec<Duration>,
    /// Fail media WRITEs from this packet index on with this Win32 code.
    pub write_error: Option<(usize, u32)>,
    /// The media channel is gone (WRITE fails with `Disconnected`).
    pub media_disconnected: bool,
    /// Virtual time each media WRITE takes.
    pub write_delay: Duration,
    /// Virtual time each signaling SDU we send takes (the air round trip
    /// of a request).
    pub request_delay: Duration,

    // -- bookkeeping --------------------------------------------------------
    /// Driver calls in order ("attach", "open", "set_link(true)", ...).
    pub calls: Vec<String>,
    script: Vec<(Duration, u64, Action)>,
    script_seq: u64,
    spin: u32,
    spin_at: Duration,
}

/// The default endpoint format: 48 kHz stereo, 24 valid bits in 32.
pub const FMT_48K_24: PcmFormat = PcmFormat {
    sample_rate: 48000,
    channels: 2,
    container_bits: 32,
    valid_bits: 24,
};
pub const FMT_96K_24: PcmFormat = PcmFormat {
    sample_rate: 96000,
    channels: 2,
    container_bits: 32,
    valid_bits: 24,
};
pub const FMT_48K_16: PcmFormat = PcmFormat {
    sample_rate: 48000,
    channels: 2,
    container_bits: 16,
    valid_bits: 16,
};

fn os(code: u32, what: &'static str) -> ChanError {
    ChanError::Os { code, what }
}

impl FakeState {
    fn new() -> Self {
        let mut sink = FakeSink::new(672);
        sink.ssc_flags = 0x3E;
        FakeState {
            now: Duration::ZERO,
            clock_us: Arc::new(AtomicU64::new(0)),
            stop: StopSignal::new(),
            protocol_version: ioc::PROTOCOL_VERSION,
            remote_addr: 0xAABB_CCDD_EEFF,
            addr_valid: true,
            info_error: None,
            attach_error: None,
            state: AudioState {
                seq: 1,
                flags: AUDIO_FLAG_ACX,
                ..AudioState::default()
            },
            detached: None,
            link_calls: Vec::new(),
            set_link_error: None,
            period: Duration::from_millis(10),
            ring_chunks: 16,
            pcm_next: Duration::ZERO,
            pcm_queue: VecDeque::new(),
            read_armed: false,
            read_ahead: None,
            discontinuity: false,
            chunks_read: 0,
            connected: true,
            probe_calls: 0,
            probe_error: false,
            sink,
            next_channel: 0x101,
            signaling: None,
            media: None,
            open_calls: 0,
            open_times: Vec::new(),
            open_error: None,
            media_open_error: None,
            report_mtus: None,
            packets: Vec::new(),
            packet_times: Vec::new(),
            write_error: None,
            media_disconnected: false,
            write_delay: Duration::ZERO,
            request_delay: Duration::ZERO,
            calls: Vec::new(),
            script: Vec::new(),
            script_seq: 0,
            spin: 0,
            spin_at: Duration::ZERO,
        }
    }

    fn tick(&mut self, t: Duration) {
        self.now = self.now.max(t);
        self.clock_us
            .store(self.now.as_micros() as u64, Ordering::Relaxed);
    }

    fn bump(&mut self) {
        self.state.seq = self.state.seq.wrapping_add(1).max(1);
    }

    pub fn attached(&self) -> bool {
        self.state.attached()
    }
    pub fn link(&self) -> bool {
        self.state.link()
    }

    /// Turn the ACX endpoint off or on (the driver's kill switch).
    pub fn set_acx(&mut self, on: bool) {
        if on {
            self.state.flags |= AUDIO_FLAG_ACX;
        } else {
            self.state.flags &= !AUDIO_FLAG_ACX;
        }
        self.bump();
    }

    /// The audio engine changes the render stream: its state and, for a
    /// stream that exists, its format.
    pub fn set_stream(&mut self, stream: StreamState, format: PcmFormat) {
        let format = if stream == StreamState::None {
            PcmFormat::default()
        } else {
            format
        };
        if self.state.stream != StreamState::Running && stream == StreamState::Running {
            self.pcm_next = self.now + self.period;
        }
        if self.state.format != format && self.state.format.is_valid() {
            // A new stream never continues the old one.
            self.discontinuity = true;
        }
        self.state.stream = stream;
        self.state.format = format;
        self.bump();
    }

    /// `frames` rendered frames never reach the reader (as when the reader
    /// fell behind): the next chunk carries the discontinuity flag.
    pub fn drop_frames(&mut self, frames: u64) {
        self.state.frames_rendered += frames;
        self.state.frames_dropped += frames;
        self.discontinuity = true;
    }

    /// Put a chunk of silence covering `frames` frames from `first_frame`
    /// into the driver's ring, as if it had been rendered earlier.
    pub fn push_pcm(&mut self, first_frame: u64, frames: u32) {
        let format = self.state.format;
        self.pcm_queue.push_back(PcmChunk {
            header: PcmHeader {
                first_frame,
                frames,
                flags: 0,
                format,
            },
            data: vec![0; frames as usize * format.frame_bytes()],
        });
    }

    /// The driver ends the session by itself (as when another handle
    /// cancelled it or the device is being removed).
    pub fn force_detach(&mut self, code: u32) {
        if self.attached() {
            self.detached = Some(os(code, "IOCTL"));
        }
        self.detach();
    }

    fn detach(&mut self) {
        self.state.flags &= !(AUDIO_FLAG_ATTACHED | AUDIO_FLAG_LINK);
        self.signaling = None;
        self.media = None;
        self.pcm_queue.clear();
        self.read_armed = false;
        self.read_ahead = None;
        self.bump();
    }

    /// The sink sends a command of its own on the signaling channel.
    pub fn sink_sends(&mut self, m: Message) {
        self.sink.inbox.lock().unwrap().push_back(m.encode_single());
    }

    /// The sink suspends the stream (SUSPEND for our SEID).
    pub fn sink_suspends(&mut self) {
        self.sink.started = false;
        self.sink_sends(avdtp::msg::suspend(0xC, &[crate::media::INT_SEID]));
    }

    /// The sink starts the stream itself (START for our SEID). Like a sink
    /// that follows AVDTP, it is then streaming and refuses a START of ours.
    pub fn sink_starts(&mut self) {
        self.sink.started = true;
        self.sink_sends(avdtp::msg::start(0xA, &[crate::media::INT_SEID]));
    }

    /// The sink closes the stream (CLOSE for our SEID).
    pub fn sink_closes(&mut self) {
        self.sink_forgets_stream();
        self.sink_sends(avdtp::msg::close(0xD, crate::media::INT_SEID));
    }

    /// The sink aborts the stream (ABORT for our SEID).
    pub fn sink_aborts(&mut self) {
        self.sink_forgets_stream();
        self.sink_sends(avdtp::msg::abort(0xE, crate::media::INT_SEID));
    }

    /// The sink reports its delay (DELAYREPORT for our SEID, 1/10 ms).
    pub fn sink_reports_delay(&mut self, delay: u16) {
        self.sink_sends(avdtp::msg::delay_report(0xB, crate::media::INT_SEID, delay));
    }

    fn sink_forgets_stream(&mut self) {
        self.sink.configured = None;
        self.sink.opened = false;
        self.sink.started = false;
    }

    /// The buds go away: both channels die and Windows reports them as not
    /// connected.
    pub fn buds_disconnect(&mut self) {
        self.connected = false;
        self.sink.disconnected = true;
        self.media_disconnected = true;
        self.sink_forgets_stream();
    }

    /// The buds are back (Windows reports them connected again).
    pub fn buds_connect(&mut self) {
        self.connected = true;
    }

    fn chunk(&mut self) {
        let fmt = self.state.format;
        let frames = (u128::from(fmt.sample_rate) * self.period.as_nanos() / 1_000_000_000) as u32;
        let first = self.state.frames_rendered;
        self.state.frames_rendered += u64::from(frames);
        if !self.attached() {
            // Tap off: rendered and gone.
            self.state.frames_dropped += u64::from(frames);
            self.discontinuity = true;
            return;
        }
        let mut data = Vec::with_capacity(frames as usize * fmt.frame_bytes());
        for i in 0..u64::from(frames) {
            // A 440 Hz tone at -12 dBFS, phase-continuous across chunks.
            let t = (first + i) as f64 / f64::from(fmt.sample_rate);
            let v = 0.25 * (std::f64::consts::TAU * 440.0 * t).sin();
            for _ in 0..fmt.channels {
                if fmt.container_bits == 16 {
                    data.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes());
                } else {
                    data.extend_from_slice(&(((v * 8_388_607.0) as i32) << 8).to_le_bytes());
                }
            }
        }
        let mut chunk = PcmChunk {
            header: PcmHeader {
                first_frame: first,
                frames,
                flags: 0,
                format: fmt,
            },
            data,
        };
        if self.read_armed && self.read_ahead.is_none() && self.pcm_queue.is_empty() {
            // Completes the outstanding READ_PCM.
            self.read_armed = false;
            chunk.header.flags = self.take_flags();
            self.read_ahead = Some(chunk);
            return;
        }
        self.pcm_queue.push_back(chunk);
        while self.pcm_queue.len() > self.ring_chunks.max(1) {
            // The reader fell behind: the oldest audio makes room.
            if let Some(old) = self.pcm_queue.pop_front() {
                self.state.frames_dropped += u64::from(old.header.frames);
            }
            self.discontinuity = true;
        }
    }

    /// The flags of the chunk being handed to the reader: the discontinuity
    /// flag goes on the next chunk the reader gets after a loss.
    fn take_flags(&mut self) -> u32 {
        if std::mem::take(&mut self.discontinuity) {
            PCM_FLAG_DISCONTINUITY
        } else {
            0
        }
    }

    fn running(&self) -> bool {
        self.state.stream == StreamState::Running && self.state.format.is_valid()
    }

    /// Time of the next thing that happens by itself.
    fn next_event(&self) -> Option<Duration> {
        let script = self.script.iter().map(|e| e.0).min();
        let pcm = self.running().then_some(self.pcm_next);
        match (script, pcm) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Let everything happen that is due up to `self.now`, in time order
    /// (a PCM chunk before a scripted action at the same instant).
    fn settle(&mut self) {
        loop {
            let script = self
                .script
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| (e.0, e.1))
                .map(|(i, e)| (i, e.0));
            let pcm = self.running().then_some(self.pcm_next);
            match (script, pcm) {
                (_, Some(p)) if p <= self.now && script.is_none_or(|(_, s)| p <= s) => {
                    self.pcm_next = p + self.period;
                    self.chunk();
                }
                (Some((i, s)), _) if s <= self.now => {
                    let (_, _, action) = self.script.remove(i);
                    action(self);
                }
                _ => return,
            }
        }
    }

    /// Move virtual time forward to `t`, letting scripted things happen on
    /// the way.
    fn advance_to(&mut self, t: Duration) {
        while let Some(next) = self.next_event() {
            if next > t {
                break;
            }
            self.tick(next);
            self.settle();
        }
        self.tick(t);
        self.settle();
    }

    /// Count a call; fail the test if the code under test spins without
    /// virtual time ever advancing.
    fn call(&mut self, what: &str) {
        if self.spin_at != self.now {
            self.spin_at = self.now;
            self.spin = 0;
        }
        self.spin += 1;
        assert!(
            self.spin < SPIN_LIMIT,
            "livelock: {SPIN_LIMIT} driver calls at t={:?} without waiting (last: {what}; recent: {:?})",
            self.now,
            &self.calls[self.calls.len().saturating_sub(12)..]
        );
    }

    fn record(&mut self, what: impl Into<String>) {
        let what = what.into();
        self.call(&what);
        self.calls.push(what);
    }

    fn channel_kind(&self, id: u32) -> Option<bool> {
        if self.signaling == Some(id) {
            Some(true)
        } else if self.media == Some(id) {
            Some(false)
        } else {
            None
        }
    }
}

/// The fake driver handle; clones share one [`FakeState`].
#[derive(Clone)]
pub struct FakeDriver(Rc<RefCell<FakeState>>);

impl Default for FakeDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeDriver {
    /// A driver at protocol v2 with ACX on, buds connected and offering SSC
    /// caps 0x3E, no render stream yet.
    pub fn new() -> Self {
        FakeDriver(Rc::new(RefCell::new(FakeState::new())))
    }

    /// Read or change the state.
    pub fn with<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }

    /// Script `action` to happen at virtual time `secs`.
    pub fn at(&self, secs: f64, action: impl FnOnce(&mut FakeState) + 'static) {
        let mut s = self.0.borrow_mut();
        s.script_seq += 1;
        let seq = s.script_seq;
        s.script
            .push((Duration::from_secs_f64(secs), seq, Box::new(action)));
    }

    /// Script the stop request at virtual time `secs`.
    pub fn stop_at(&self, secs: f64) {
        self.at(secs, |s| s.stop.request());
    }

    pub fn now(&self) -> Duration {
        self.0.borrow().now
    }

    pub fn stop(&self) -> StopSignal {
        self.0.borrow().stop.clone()
    }

    /// The link probe that goes with this driver.
    pub fn link_probe(&self) -> FakeLink {
        FakeLink(self.clone())
    }

    /// The clock that goes with this driver.
    pub fn clock(&self) -> FakeClock {
        FakeClock(self.clone())
    }
}

impl Driver for FakeDriver {
    fn reopen(&self) -> Result<(), ChanError> {
        let mut s = self.0.borrow_mut();
        s.record("reopen");
        if s.attached() {
            s.detach();
        }
        match s.info_error {
            Some(code) => Err(os(code, "CreateFile")),
            None => Ok(()),
        }
    }

    fn info(&self) -> Result<ioc::InfoReply, ChanError> {
        let mut s = self.0.borrow_mut();
        s.record("info");
        if let Some(code) = s.info_error {
            return Err(os(code, "DeviceIoControl"));
        }
        Ok(ioc::InfoReply {
            protocol_version: s.protocol_version,
            driver_version: 1 << 16,
            remote_addr: s.remote_addr,
            channels_open: u16::from(s.signaling.is_some()) + u16::from(s.media.is_some()),
            channels_max: ioc::MAX_CHANNELS as u16,
            flags: ioc::INFO_FLAG_D0
                | if s.addr_valid {
                    ioc::INFO_FLAG_ADDR_VALID
                } else {
                    0
                },
        })
    }

    fn audio_state(&self) -> Result<AudioState, ChanError> {
        let mut s = self.0.borrow_mut();
        s.call("get_state");
        if s.protocol_version < 2 {
            return Err(os(1, "DeviceIoControl"));
        }
        Ok(s.state)
    }

    fn attach(&self) -> Result<(), ChanError> {
        let mut s = self.0.borrow_mut();
        s.record("attach");
        if s.protocol_version < 2 {
            return Err(os(1, "DeviceIoControl"));
        }
        if let Some(code) = s.attach_error {
            return Err(os(code, "DeviceIoControl"));
        }
        if s.attached() {
            return Err(os(170, "DeviceIoControl"));
        }
        s.detached = None;
        s.state.flags |= AUDIO_FLAG_ATTACHED;
        s.bump();
        Ok(())
    }

    fn detach(&self) {
        let mut s = self.0.borrow_mut();
        s.record("detach");
        if s.attached() {
            s.detach();
        }
        s.detached = None;
    }

    fn set_link(&self, connected: bool) -> Result<(), ChanError> {
        let mut s = self.0.borrow_mut();
        s.record(format!("set_link({connected})"));
        if !s.attached() {
            return Err(os(5, "DeviceIoControl"));
        }
        if let Some(code) = s.set_link_error {
            return Err(os(code, "DeviceIoControl"));
        }
        let now = s.now;
        s.link_calls.push((now, connected));
        if s.link() != connected {
            s.state.flags ^= AUDIO_FLAG_LINK;
            s.bump();
        }
        Ok(())
    }

    fn open_channel(
        &self,
        req: ioc::OpenChannelRequest,
        timeout: Duration,
    ) -> Result<ioc::OpenChannelReply, ChanError> {
        let mut s = self.0.borrow_mut();
        s.record("open");
        s.open_calls += 1;
        let now = s.now;
        s.open_times.push(now);
        if !s.connected {
            // Nobody answers the page.
            let t = s.now + timeout;
            s.advance_to(t);
            return Err(os(121, "IOCTL"));
        }
        if let Some(code) = s.open_error {
            return Err(os(code, "IOCTL"));
        }
        let signaling = s.signaling.is_none();
        if !signaling && let Some(code) = s.media_open_error {
            return Err(os(code, "IOCTL"));
        }
        let id = s.next_channel;
        s.next_channel += 0x100;
        if signaling {
            // A new AVDTP session: the sink starts from scratch.
            s.signaling = Some(id);
            s.sink.disconnected = false;
            s.sink.out.clear();
            s.sink.inbox.lock().unwrap().clear();
            s.sink_forgets_stream();
        } else {
            s.media = Some(id);
            s.media_disconnected = false;
        }
        let (out_mtu, in_mtu) = s
            .report_mtus
            .unwrap_or((req.effective_in_mtu(), req.effective_out_mtu_max()));
        Ok(ioc::OpenChannelReply {
            channel_id: id,
            out_mtu,
            in_mtu,
        })
    }

    fn close_channel(&self, id: u32) -> Result<(), ChanError> {
        let mut s = self.0.borrow_mut();
        match s.channel_kind(id) {
            Some(true) => {
                s.record("close(signaling)");
                s.signaling = None;
                Ok(())
            }
            Some(false) => {
                s.record("close(media)");
                s.media = None;
                Ok(())
            }
            None => {
                s.record("close(stale)");
                Err(os(6, "IOCTL"))
            }
        }
    }

    fn write(&self, id: u32, sdu: &[u8]) -> Result<(), ChanError> {
        let mut s = self.0.borrow_mut();
        s.call("write");
        match s.channel_kind(id) {
            Some(true) => {
                let r = s.sink.send(sdu);
                if r.is_ok() && s.request_delay > Duration::ZERO {
                    let t = s.now + s.request_delay;
                    s.advance_to(t);
                }
                r
            }
            Some(false) => {
                if s.media_disconnected {
                    return Err(ChanError::Disconnected);
                }
                if let Some((from, code)) = s.write_error
                    && s.packets.len() >= from
                {
                    return Err(os(code, "IOCTL"));
                }
                let now = s.now;
                s.packets.push(sdu.to_vec());
                s.packet_times.push(now);
                let t = now + s.write_delay;
                s.advance_to(t);
                Ok(())
            }
            None => Err(os(6, "IOCTL")),
        }
    }

    fn read(&self, id: u32, timeout: Duration) -> Result<Vec<u8>, ChanError> {
        let mut s = self.0.borrow_mut();
        s.call("read");
        match s.channel_kind(id) {
            Some(true) => match s.sink.recv(timeout) {
                Err(ChanError::Timeout) => {
                    let t = s.now + timeout;
                    s.advance_to(t);
                    Err(ChanError::Timeout)
                }
                r => r,
            },
            Some(false) => {
                let t = s.now + timeout;
                s.advance_to(t);
                Err(ChanError::Timeout)
            }
            None => Err(os(6, "IOCTL")),
        }
    }

    fn flush_pcm(&self) {
        let mut s = self.0.borrow_mut();
        s.call("flush_pcm");
        // Like the real one: the outstanding READ_PCM is cancelled and what
        // it had already been given is discarded. The driver's ring is not
        // touched (the contract has no way to empty it), and the driver
        // does not count that chunk as dropped: it was delivered.
        s.read_armed = false;
        s.read_ahead = None;
    }

    fn wait(&self, interest: &Interest, timeout: Duration) -> Wake {
        let mut s = self.0.borrow_mut();
        s.call("wait");
        let deadline = s.now + timeout;
        s.settle();
        loop {
            if s.stop.is_set() {
                return Wake::Stop;
            }
            if let Some(e) = s.detached.take() {
                return Wake::Detached(e);
            }
            if let Some(id) = interest.signaling {
                match s.channel_kind(id) {
                    Some(true) => {
                        if s.sink.disconnected {
                            return Wake::Signaling(Err(ChanError::Disconnected));
                        }
                        let ready =
                            !s.sink.out.is_empty() || !s.sink.inbox.lock().unwrap().is_empty();
                        if ready {
                            return Wake::Signaling(s.sink.recv(Duration::ZERO));
                        }
                    }
                    _ => return Wake::Signaling(Err(os(6, "IOCTL"))),
                }
            }
            if s.attached() {
                if let Some(seq) = interest.seq
                    && seq != s.state.seq
                {
                    return Wake::State(s.state);
                }
                if interest.pcm {
                    let next = match s.read_ahead.take() {
                        Some(c) => Some(c),
                        None => match s.pcm_queue.pop_front() {
                            Some(mut c) => {
                                c.header.flags |= s.take_flags();
                                Some(c)
                            }
                            None => None,
                        },
                    };
                    match next {
                        Some(c) => {
                            s.chunks_read += 1;
                            return Wake::Pcm(Ok(c));
                        }
                        // The READ_PCM goes out and stays pending.
                        None => s.read_armed = true,
                    }
                }
            } else if interest.seq.is_some() || interest.pcm {
                return Wake::StateError(os(5, "DeviceIoControl"));
            }
            if s.now >= deadline {
                return Wake::Timeout;
            }
            let next = s.next_event().map_or(deadline, |t| t.min(deadline));
            s.tick(next);
            s.settle();
        }
    }
}

/// [`LinkProbe`] answering from the fake's `connected` flag.
pub struct FakeLink(FakeDriver);

impl LinkProbe for FakeLink {
    fn connected(&mut self, addr: u64) -> Result<bool, String> {
        let mut s = self.0.0.borrow_mut();
        s.call("probe");
        s.probe_calls += 1;
        assert_eq!(addr, s.remote_addr, "probed the address GET_INFO gave");
        if s.probe_error {
            return Err("no radio".into());
        }
        Ok(s.connected)
    }
}

/// The fake's virtual time as a [`Clock`].
#[derive(Clone)]
pub struct FakeClock(FakeDriver);

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        self.0.now()
    }
    fn sleep_until(&mut self, t: Duration) {
        self.0.0.borrow_mut().advance_to(t);
    }
    fn describe(&self) -> &'static str {
        "fake driver time"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> Interest {
        Interest::default()
    }

    #[test]
    fn time_only_moves_in_waits_and_scripts_fire_in_order() {
        let f = FakeDriver::new();
        f.at(2.0, |s| s.calls.push("b".into()));
        f.at(1.0, |s| s.calls.push("a".into()));
        f.at(2.0, |s| s.calls.push("c".into()));
        assert!(matches!(
            f.wait(&none(), Duration::from_millis(500)),
            Wake::Timeout
        ));
        assert_eq!(f.now(), Duration::from_millis(500));
        assert!(matches!(
            f.wait(&none(), Duration::from_secs(5)),
            Wake::Timeout
        ));
        assert_eq!(f.now(), Duration::from_millis(5500));
        assert_eq!(f.with(|s| s.calls.clone()), ["a", "b", "c"]);
        f.stop_at(6.0);
        assert!(matches!(
            f.wait(&none(), Duration::from_secs(5)),
            Wake::Stop
        ));
        assert_eq!(f.now(), Duration::from_secs(6));
    }

    #[test]
    fn audio_side_follows_the_contract() {
        let f = FakeDriver::new();
        assert!(f.set_link(true).is_err(), "attached handle only");
        f.attach().unwrap();
        assert!(matches!(f.attach(), Err(ChanError::Os { code: 170, .. })));
        let seq = f.audio_state().unwrap().seq;
        f.set_link(true).unwrap();
        let st = f.audio_state().unwrap();
        assert!(st.link() && st.attached() && st.acx());
        assert_ne!(st.seq, seq, "the sequence number moves with the flags");
        // A state change wakes a waiter that has the old sequence number.
        f.at(1.0, |s| s.set_stream(StreamState::Running, FMT_48K_24));
        let want = Interest {
            seq: Some(st.seq),
            ..none()
        };
        let Wake::State(now) = f.wait(&want, Duration::from_secs(9)) else {
            panic!("no state event");
        };
        assert_eq!((now.stream, now.format), (StreamState::Running, FMT_48K_24));
        assert_eq!(f.now(), Duration::from_secs(1));
        // PCM arrives one period at a time, contiguous, in the stream format.
        let pcm = Interest {
            seq: Some(now.seq),
            pcm: true,
            ..none()
        };
        let mut next = 0;
        for k in 1..=5u32 {
            let Wake::Pcm(Ok(c)) = f.wait(&pcm, Duration::from_secs(9)) else {
                panic!("no PCM");
            };
            assert_eq!(f.now(), Duration::from_millis(1000 + 10 * u64::from(k)));
            assert_eq!((c.header.first_frame, c.header.frames), (next, 480));
            assert_eq!(c.header.flags, 0);
            assert_eq!(c.data.len(), 480 * 8);
            next += 480;
        }
        // A reader that stays away loses audio and is told so.
        f.with(|s| s.ring_chunks = 2);
        assert!(matches!(
            f.wait(&none(), Duration::from_millis(100)),
            Wake::Timeout
        ));
        let Wake::Pcm(Ok(a)) = f.wait(&pcm, Duration::ZERO) else {
            panic!()
        };
        let Wake::Pcm(Ok(b)) = f.wait(&pcm, Duration::ZERO) else {
            panic!()
        };
        // Ten chunks were rendered meanwhile; the ring kept the newest two,
        // and the first one handed out says that audio is missing before it.
        assert_eq!(
            (a.header.first_frame, a.header.flags),
            (480 * 13, PCM_FLAG_DISCONTINUITY)
        );
        assert_eq!((b.header.first_frame, b.header.flags), (480 * 14, 0));
        let Wake::Pcm(Ok(c)) = f.wait(&pcm, Duration::from_secs(1)) else {
            panic!()
        };
        assert_eq!((c.header.first_frame, c.header.flags), (480 * 15, 0));
        assert_eq!(f.audio_state().unwrap().frames_dropped, 480 * 8);
        // A read that was waiting gets the next chunk as it is rendered, and
        // flush_pcm discards what such a read already holds, nothing more.
        assert!(matches!(f.wait(&pcm, Duration::ZERO), Wake::Timeout));
        assert!(matches!(
            f.wait(&none(), Duration::from_millis(30)),
            Wake::Timeout
        ));
        f.flush_pcm();
        let Wake::Pcm(Ok(d)) = f.wait(&pcm, Duration::ZERO) else {
            panic!()
        };
        assert_eq!(d.header.first_frame, 480 * 17, "16 went with the flush");
        assert_eq!(f.audio_state().unwrap().frames_dropped, 480 * 8);
        // Detach: the wait reports it, the link flag is gone.
        f.at(3.0, |s| s.force_detach(995));
        assert!(matches!(
            f.wait(&none(), Duration::from_secs(9)),
            Wake::Detached(_)
        ));
        let st = f.audio_state().unwrap();
        assert!(!st.attached() && !st.link());
    }

    #[test]
    fn pipe_side_is_a_sink_and_a_packet_recorder() {
        let f = FakeDriver::new();
        let req = ioc::OpenChannelRequest {
            psm: 0x19,
            in_mtu: 1021,
            out_mtu_max: 0,
            flags: 0,
        };
        let sig = f.open_channel(req, Duration::from_secs(5)).unwrap();
        assert_eq!(
            (sig.out_mtu, sig.in_mtu),
            (1021, 65535),
            "like the real one"
        );
        f.write(sig.channel_id, &avdtp::msg::discover(1).encode_single())
            .unwrap();
        let reply = f.read(sig.channel_id, Duration::from_secs(5)).unwrap();
        assert_eq!(reply[0] & 0x03, 0x02, "an accept");
        assert!(matches!(
            f.read(sig.channel_id, Duration::from_secs(5)),
            Err(ChanError::Timeout)
        ));
        assert_eq!(
            f.now(),
            Duration::from_secs(5),
            "a timed-out read takes time"
        );
        let media = f.open_channel(req, Duration::from_secs(5)).unwrap();
        f.write(media.channel_id, &[1, 2, 3]).unwrap();
        assert_eq!(f.with(|s| s.packets.clone()), vec![vec![1, 2, 3]]);
        // The sink speaks up by itself.
        f.with(|s| s.sink_reports_delay(3400));
        let want = Interest {
            signaling: Some(sig.channel_id),
            ..none()
        };
        assert!(matches!(
            f.wait(&want, Duration::from_secs(1)),
            Wake::Signaling(Ok(_))
        ));
        // The buds go away.
        f.with(|s| s.buds_disconnect());
        assert!(matches!(
            f.write(media.channel_id, &[4]),
            Err(ChanError::Disconnected)
        ));
        assert!(matches!(
            f.wait(&want, Duration::from_secs(1)),
            Wake::Signaling(Err(ChanError::Disconnected))
        ));
        f.close_channel(media.channel_id).unwrap();
        f.close_channel(sig.channel_id).unwrap();
        assert!(f.close_channel(sig.channel_id).is_err());
        assert!(
            f.open_channel(req, Duration::from_secs(5)).is_err(),
            "nobody to connect to"
        );
        assert_eq!(f.with(|s| s.open_calls), 3);
        assert!(!f.link_probe().connected(0xAABB_CCDD_EEFF).unwrap());
    }

    #[test]
    #[should_panic(expected = "livelock")]
    fn a_spinning_caller_is_caught() {
        let f = FakeDriver::new();
        loop {
            let _ = f.wait(&none(), Duration::ZERO);
        }
    }
}
