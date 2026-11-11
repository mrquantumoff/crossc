//! The seam between the session logic and the driver: everything the session
//! does to the device goes through [`Driver`], so the same logic runs against
//! the real device (`crate::real::RealDriver`, Windows) and against a fake
//! in tests.
//!
//! The session is single-threaded. Where it must wait for several things at
//! once (the pending ATTACH completing, an audio state change, the next PCM
//! chunk, an SDU on the signaling channel, a stop request, a timer) it calls
//! [`Driver::wait`] with what it is interested in; request/response exchanges
//! on a channel use the blocking [`Driver::read`] / [`Driver::write`] through
//! a [`DriverChannel`].

use std::time::Duration;

use crossc_ioctl as ioc;
use crossc_ioctl::audio::{AudioState, PcmHeader};

use crate::channel::{ChanError, SduChannel};

/// One READ_PCM reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcmChunk {
    pub header: PcmHeader,
    /// `header.frames` frames of interleaved PCM in `header.format`.
    pub data: Vec<u8>,
}

/// What a [`Driver::wait`] call should wake up for, besides the stop
/// request, the end of the ATTACH request and the timeout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interest {
    /// An SDU arriving on this channel (the signaling channel).
    pub signaling: Option<u32>,
    /// The audio state's sequence number differing from this one.
    pub seq: Option<u32>,
    /// The next PCM chunk.
    pub pcm: bool,
}

/// Why [`Driver::wait`] returned.
#[derive(Debug)]
pub enum Wake {
    Timeout,
    /// The stop request was raised.
    Stop,
    /// The ATTACH request ended: the driver detached this session (it has
    /// closed the session's channels and cancelled its audio requests).
    Detached(ChanError),
    /// The audio state changed.
    State(AudioState),
    /// Waiting for the audio state failed.
    StateError(ChanError),
    /// An SDU arrived on the channel of interest, or the channel failed.
    Signaling(Result<Vec<u8>, ChanError>),
    /// The next PCM chunk, or reading PCM failed.
    Pcm(Result<PcmChunk, ChanError>),
}

/// The driver as the session uses it. Handles are cheap to clone and all
/// clones refer to the same device.
pub trait Driver: Clone {
    /// (Re)open the device, dropping every pending request first.
    fn reopen(&self) -> Result<(), ChanError>;
    /// GET_INFO.
    fn info(&self) -> Result<ioc::InfoReply, ChanError>;
    /// AUDIO_GET_STATE.
    fn audio_state(&self) -> Result<AudioState, ChanError>;
    /// Send AUDIO_ATTACH and leave it pending. `Err` if the driver refuses
    /// it outright. Its later completion is reported by [`Self::wait`] as
    /// [`Wake::Detached`].
    fn attach(&self) -> Result<(), ChanError>;
    /// Cancel the pending ATTACH and every other pending audio request.
    /// Harmless when not attached.
    fn detach(&self);
    /// AUDIO_SET_LINK.
    fn set_link(&self, connected: bool) -> Result<(), ChanError>;
    /// OPEN_CHANNEL (blocks until the L2CAP channel is up, or `timeout`).
    fn open_channel(
        &self,
        req: ioc::OpenChannelRequest,
        timeout: Duration,
    ) -> Result<ioc::OpenChannelReply, ChanError>;
    /// CLOSE_CHANNEL.
    fn close_channel(&self, id: u32) -> Result<(), ChanError>;
    /// WRITE one SDU.
    fn write(&self, id: u32, sdu: &[u8]) -> Result<(), ChanError>;
    /// READ one SDU, waiting up to `timeout` ([`ChanError::Timeout`]). An SDU
    /// that arrives after the timeout is kept for the next `read` or `wait`.
    fn read(&self, id: u32, timeout: Duration) -> Result<Vec<u8>, ChanError>;
    /// Throw away PCM that was read ahead but not yet handed out, so the
    /// next chunk is fresh audio.
    fn flush_pcm(&self);
    /// Block until something of `interest` happens, the stop request is
    /// raised, the ATTACH request ends, or `timeout` passes.
    fn wait(&self, interest: &Interest, timeout: Duration) -> Wake;
}

/// "Are the buds connected to this PC right now?" The session opens L2CAP
/// only while the answer is yes, so it never pages the buds itself.
pub trait LinkProbe {
    /// `addr` is the remote BD_ADDR as GET_INFO reports it.
    fn connected(&mut self, addr: u64) -> Result<bool, String>;
}

/// One L2CAP channel of a [`Driver`] as an [`SduChannel`]. Closed on drop.
pub struct DriverChannel<D: Driver> {
    drv: D,
    pub id: u32,
    /// MTUs as the driver reported them (see `rtp::media_mtu_limit` for how
    /// far they can be trusted).
    pub out_mtu: u16,
    pub in_mtu: u16,
    /// Largest SDU [`SduChannel::out_mtu`] admits to.
    send_mtu: usize,
    closed: bool,
}

impl<D: Driver> DriverChannel<D> {
    /// Open a channel. `send_mtu_cap` bounds what [`SduChannel::out_mtu`]
    /// reports (the driver's own figure is not reliable).
    pub fn open(
        drv: &D,
        req: ioc::OpenChannelRequest,
        timeout: Duration,
        send_mtu_cap: usize,
    ) -> Result<Self, ChanError> {
        let r = drv.open_channel(req, timeout)?;
        let reported = match r.out_mtu {
            0 | 0xFFFF => usize::from(ioc::L2CAP_DEFAULT_MTU),
            m => usize::from(m),
        };
        Ok(DriverChannel {
            drv: drv.clone(),
            id: r.channel_id,
            out_mtu: r.out_mtu,
            in_mtu: r.in_mtu,
            send_mtu: reported
                .min(send_mtu_cap)
                .max(usize::from(ioc::L2CAP_MIN_MTU)),
            closed: false,
        })
    }

    pub fn close(&mut self) -> Result<(), ChanError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.drv.close_channel(self.id)
    }
}

impl<D: Driver> Drop for DriverChannel<D> {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl<D: Driver> SduChannel for DriverChannel<D> {
    fn send(&mut self, sdu: &[u8]) -> Result<(), ChanError> {
        self.drv.write(self.id, sdu)
    }
    fn recv(&mut self, timeout: Duration) -> Result<Vec<u8>, ChanError> {
        self.drv.read(self.id, timeout)
    }
    fn out_mtu(&self) -> usize {
        self.send_mtu
    }
}
