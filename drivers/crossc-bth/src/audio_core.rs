//! Audio session state of one device: everything the audio IOCTL contract
//! (`crossc_ioctl::audio`) says about sequence numbers, the attached handle,
//! the link flag, the stream and the PCM tap, as plain data and plain
//! functions.
//!
//! No kernel calls, no `unsafe`, no allocation, no panics. The driver
//! (`audio.rs`) keeps one [`Session`] per device and calls into it with its
//! audio spinlock held; the requests themselves live in WDF queues, this file
//! only remembers *which* one is parked. The same file is compiled into the
//! host test crate (`drivers/crossc-bth/host-tests`).
//!
//! # Parked requests
//! A WAIT_EVENT or READ_PCM that has to pend gets a [`Ticket`], a number that
//! is never used twice. The driver writes it into the request and names the
//! request by it from then on. The session remembers the ticket of the one
//! WAIT_EVENT and the one READ_PCM that are parked; a request with any other
//! ticket is a leftover (its session ended, also when the same handle
//! attached again since) and is refused as `Stale`. The handle a request came
//! through is only looked at when the request arrives.
//!
//! # The tap
//! * Frames are numbered by the endpoint's render count
//!   ([`AudioState::frames_rendered`]). The ring always holds the frames
//!   `rendered - ring_frames .. rendered`: one contiguous run that ends at the
//!   newest frame. Every way frames can be lost removes them from the *old*
//!   end (or empties the ring), so a gap is always in front of the ring and
//!   one flag is enough to report it on the next chunk.
//! * Nobody attached, or no ring storage: rendered frames are counted as
//!   dropped and the flag is set.
//! * Ring full (the reader fell behind): the oldest frames are dropped to
//!   make room, counted, flag set. The reader then gets the newest audio and
//!   the timer pass never waits.
//! * The ring belongs to the stream that feeds it and holds frames of that
//!   stream's format only; a new stream starts with an empty ring. So one
//!   chunk never mixes formats.

use crossc_ioctl::audio::{
    AUDIO_FLAG_ACX, AUDIO_FLAG_ATTACHED, AUDIO_FLAG_LINK, AUDIO_FLAGS_VOLUME, AudioState, PCM_FLAG_DISCONTINUITY,
    PcmFormat, PcmHeader, StreamState, VolumeState,
};

use crate::pcm_ring::Ring;

/// Length of the tap ring in milliseconds of the stream format.
pub const RING_MS: u32 = 250;

/// Smallest frame the contract allows (16-bit mono). Used to size-check a
/// READ_PCM buffer while no stream exists.
pub const MIN_FRAME_BYTES: usize = 2;

/// Bytes of ring storage for [`RING_MS`] of `format` (0 for an invalid format).
pub fn ring_bytes(format: &PcmFormat) -> usize {
    if !format.is_valid() {
        return 0;
    }
    let frames = (u64::from(format.sample_rate) * u64::from(RING_MS) / 1000).max(1);
    usize::try_from(frames).map_or(0, |f| f.saturating_mul(format.frame_bytes()))
}

/// Write a GET_STATE / WAIT_EVENT reply into `out`: as much of the
/// protocol 3 reply as fits, and at least the protocol 2 part (a v2 caller
/// passes a 40-byte buffer and gets exactly what protocol 2 sent). Returns
/// the reply length; `None` if `out` is shorter than the v2 reply.
pub fn write_state(out: &mut [u8], state: &AudioState) -> Option<usize> {
    let e = state.encode();
    if out.len() < AudioState::SIZE_V2 {
        return None;
    }
    let n = out.len().min(e.len());
    out.get_mut(..n)?.copy_from_slice(e.get(..n)?);
    Some(n)
}

/// The sequence number after `seq`: the next one, skipping 0 ("none" in
/// WAIT_EVENT).
pub fn next_seq(seq: u32) -> u32 {
    match seq.wrapping_add(1) {
        0 => 1,
        n => n,
    }
}

/// Why a request is refused. The driver maps these to the NTSTATUS values of
/// the contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refuse {
    /// No endpoint (ACX off or no circuit): `STATUS_NOT_SUPPORTED`.
    NotSupported,
    /// The caller's handle is not the attached one: `STATUS_ACCESS_DENIED`.
    Denied,
    /// Already attached / already one outstanding: `STATUS_DEVICE_BUSY`.
    Busy,
    /// Output buffer cannot hold the header and one frame: `STATUS_BUFFER_TOO_SMALL`.
    TooSmall,
}

/// What [`Session::detach`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Detached {
    /// The link flag was set, so the jack state changed.
    pub link_changed: bool,
}

/// Outcome of handing a parked WAIT_EVENT request its reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitDone {
    /// The request is not the parked one (a leftover): cancel it.
    Stale,
    /// The sequence number still equals the caller's: park it again.
    NotReady,
    Done(AudioState),
}

/// Outcome of handing a parked READ_PCM request its chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadDone {
    /// The request is not the parked one (a leftover): cancel it.
    Stale,
    /// No frame is available (any more): park it again.
    Empty,
    /// The buffer cannot hold one frame of the current format.
    TooSmall,
    /// Reply length in bytes (header + samples).
    Done(usize),
}

/// Opaque identity of a file handle or of a stream; 0 means "none".
pub type Id = usize;

/// Identity of one parked request; 0 means "none". Never reused.
pub type Ticket = u64;

#[derive(Clone, Copy, Debug)]
pub struct Session {
    seq: u32,
    /// The render circuit exists (AUDIO_FLAG_ACX).
    endpoint: bool,
    /// Handle whose ATTACH is pending.
    holder: Id,
    link: bool,
    /// Stream that feeds the tap.
    stream: Id,
    stream_state: StreamState,
    format: PcmFormat,
    rendered: u64,
    dropped: u64,
    /// Of `rendered`: frames the audio engine cannot have written (late timer
    /// pass), stored as silence.
    silence: u64,
    ring: Ring,
    discontinuity: bool,
    /// Last ticket handed out.
    tickets: Ticket,
    /// The parked WAIT_EVENT (0 = none) and the seq it waits to see change.
    wait_ticket: Ticket,
    wait_seq: u32,
    /// The parked READ_PCM (0 = none).
    read_ticket: Ticket,
    /// Protocol 3 volume report (`avrcp_vol::Volume::wire`): its
    /// `AUDIO_FLAG_*` bits and fields.
    volume_flags: u32,
    volume: VolumeState,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub const fn new() -> Self {
        Self {
            seq: 1,
            endpoint: false,
            holder: 0,
            link: false,
            stream: 0,
            stream_state: StreamState::None,
            format: PcmFormat { sample_rate: 0, channels: 0, container_bits: 0, valid_bits: 0 },
            rendered: 0,
            dropped: 0,
            silence: 0,
            ring: Ring::new(),
            discontinuity: true,
            tickets: 0,
            wait_ticket: 0,
            wait_seq: 0,
            read_ticket: 0,
            volume_flags: 0,
            volume: VolumeState { level: 0, left: 0, right: 0, avrcp: 0, gets: 0, sets: 0, events: 0 },
        }
    }

    /// A ticket no request had before. Not a state change: `seq` stays.
    fn issue(&mut self) -> Ticket {
        self.tickets = match self.tickets.wrapping_add(1) {
            0 => 1,
            n => n,
        };
        self.tickets
    }

    /// Any change of flags, stream state or format.
    fn bump(&mut self) {
        self.seq = next_seq(self.seq);
    }

    pub fn state(&self) -> AudioState {
        let mut flags = 0;
        if self.endpoint {
            flags |= AUDIO_FLAG_ACX;
        }
        if self.holder != 0 {
            flags |= AUDIO_FLAG_ATTACHED;
        }
        if self.link {
            flags |= AUDIO_FLAG_LINK;
        }
        AudioState {
            seq: self.seq,
            flags: flags | self.volume_flags,
            stream: self.stream_state,
            format: self.format,
            frames_rendered: self.rendered,
            frames_dropped: self.dropped,
            volume: self.volume,
        }
    }

    /// New volume report. Returns true (and bumps the sequence number) if
    /// the flags or a level changed; the AVRCP counters alone do not count,
    /// like the frame counters. Bits outside `AUDIO_FLAGS_VOLUME` are ignored.
    pub fn set_volume(&mut self, flags: u32, volume: VolumeState) -> bool {
        let flags = flags & AUDIO_FLAGS_VOLUME;
        let levels = |v: &VolumeState| (v.level, v.left, v.right, v.avrcp);
        let changed = flags != self.volume_flags || levels(&volume) != levels(&self.volume);
        self.volume_flags = flags;
        self.volume = volume;
        if changed {
            self.bump();
        }
        changed
    }

    /// The render circuit exists: the audio IOCTLs are supported.
    pub fn endpoint(&self) -> bool {
        self.endpoint
    }

    pub fn holder(&self) -> Id {
        self.holder
    }

    pub fn link(&self) -> bool {
        self.link
    }

    pub fn silence_frames(&self) -> u64 {
        self.silence
    }

    pub fn ring_capacity(&self) -> usize {
        self.ring.capacity()
    }

    fn frame_bytes(&self) -> usize {
        self.format.frame_bytes()
    }

    fn ring_frames(&self) -> usize {
        match self.frame_bytes() {
            0 => 0,
            fb => self.ring.len() / fb,
        }
    }

    /// Empty the ring; what was in it was never handed to a reader.
    fn drop_ring(&mut self) {
        if self.ring.is_empty() {
            return;
        }
        let frames = self.ring_frames();
        if frames > 0 {
            self.dropped += frames as u64;
            self.discontinuity = true;
        }
        self.ring.clear();
    }

    // ---- endpoint / session ------------------------------------------------

    /// The render circuit was added / removed. Returns true if that changed
    /// the state.
    pub fn set_endpoint(&mut self, present: bool) -> bool {
        if self.endpoint == present {
            return false;
        }
        self.endpoint = present;
        self.bump();
        true
    }

    /// The caller must be the attached handle.
    fn gate(&self, who: Id) -> Result<(), Refuse> {
        if !self.endpoint {
            Err(Refuse::NotSupported)
        } else if who == 0 || who != self.holder {
            Err(Refuse::Denied)
        } else {
            Ok(())
        }
    }

    /// ATTACH from handle `who`.
    pub fn attach(&mut self, who: Id) -> Result<(), Refuse> {
        if !self.endpoint {
            return Err(Refuse::NotSupported);
        }
        if who == 0 {
            return Err(Refuse::Denied);
        }
        if self.holder != 0 {
            return Err(Refuse::Busy);
        }
        self.holder = who;
        // The tap was off until now: the first chunk starts after a gap.
        self.discontinuity = true;
        self.bump();
        Ok(())
    }

    /// End the session of `who` (0 = whoever is attached). `None` if that
    /// handle is not attached. Clears the link flag, turns the tap off and
    /// forgets the parked WAIT_EVENT / READ_PCM (the caller cancels them).
    pub fn detach(&mut self, who: Id) -> Option<Detached> {
        if self.holder == 0 || (who != 0 && who != self.holder) {
            return None;
        }
        let link_changed = self.link;
        self.holder = 0;
        self.link = false;
        self.drop_ring();
        self.discontinuity = true;
        self.wait_ticket = 0;
        self.read_ticket = 0;
        self.bump();
        Some(Detached { link_changed })
    }

    /// SET_LINK. `Ok(true)` if the flag changed (jack state changed).
    pub fn set_link(&mut self, who: Id, connected: bool) -> Result<bool, Refuse> {
        self.gate(who)?;
        if self.link == connected {
            return Ok(false);
        }
        self.link = connected;
        self.bump();
        Ok(true)
    }

    // ---- WAIT_EVENT ---------------------------------------------------------

    /// WAIT_EVENT with the caller's last seen `seq`. `Ok(Some(state))`:
    /// complete now. `Ok(None)`: park the request under [`Session::wait_ticket`].
    pub fn begin_wait(&mut self, who: Id, seq: u32) -> Result<Option<AudioState>, Refuse> {
        self.gate(who)?;
        if self.wait_ticket != 0 {
            return Err(Refuse::Busy);
        }
        if seq != self.seq {
            return Ok(Some(self.state()));
        }
        self.wait_ticket = self.issue();
        self.wait_seq = seq;
        Ok(None)
    }

    /// Ticket of the parked WAIT_EVENT, 0 if none.
    pub fn wait_ticket(&self) -> Ticket {
        self.wait_ticket
    }

    /// A parked WAIT_EVENT can be completed.
    pub fn wait_ready(&self) -> bool {
        self.wait_ticket != 0 && self.wait_seq != self.seq
    }

    /// The WAIT_EVENT request `ticket` can stay in its queue: it is the
    /// parked one and what it waits for has not happened.
    pub fn wait_rests(&self, ticket: Ticket) -> bool {
        ticket != 0 && ticket == self.wait_ticket && !self.wait_ready()
    }

    /// The WAIT_EVENT request `ticket` was taken out of its queue.
    pub fn finish_wait(&mut self, ticket: Ticket) -> WaitDone {
        if ticket == 0 || ticket != self.wait_ticket {
            return WaitDone::Stale;
        }
        if !self.wait_ready() {
            return WaitDone::NotReady;
        }
        self.wait_ticket = 0;
        WaitDone::Done(self.state())
    }

    /// The WAIT_EVENT request `ticket` is gone (cancelled, or parking
    /// failed). The end of a leftover does not touch the request parked now.
    pub fn wait_gone(&mut self, ticket: Ticket) {
        if ticket != 0 && ticket == self.wait_ticket {
            self.wait_ticket = 0;
        }
    }

    // ---- READ_PCM -----------------------------------------------------------

    /// Move as many whole frames as fit from the ring into `out`, behind a
    /// header. Returns the reply length, 0 if there is no frame or no room.
    fn fill(&mut self, storage: &[u8], out: &mut [u8]) -> usize {
        let fb = self.frame_bytes();
        let avail = self.ring_frames();
        if fb == 0 || avail == 0 || out.len() < PcmHeader::SIZE {
            return 0;
        }
        let room = (out.len() - PcmHeader::SIZE) / fb;
        let frames = avail.min(room).min(u32::MAX as usize);
        if frames == 0 {
            return 0;
        }
        let bytes = frames * fb;
        let header = PcmHeader {
            first_frame: self.rendered.saturating_sub(avail as u64),
            frames: frames as u32,
            flags: if self.discontinuity { PCM_FLAG_DISCONTINUITY } else { 0 },
            format: self.format,
        };
        let Some((head, body)) = out.split_at_mut_checked(PcmHeader::SIZE) else {
            return 0;
        };
        let Some(body) = body.get_mut(..bytes) else {
            return 0;
        };
        if self.ring.pop(storage, body) != bytes {
            // Storage mismatch: nothing was removed, report nothing.
            return 0;
        }
        head.copy_from_slice(&header.encode());
        self.discontinuity = false;
        PcmHeader::SIZE + bytes
    }

    /// Smallest READ_PCM buffer acceptable right now.
    fn read_min(&self) -> usize {
        PcmHeader::SIZE + if self.format.is_valid() { self.frame_bytes() } else { MIN_FRAME_BYTES }
    }

    /// READ_PCM. `Ok(n)` with `n > 0`: `out[..n]` is the reply, complete now.
    /// `Ok(0)`: park the request under [`Session::read_ticket`].
    pub fn begin_read(&mut self, who: Id, storage: &[u8], out: &mut [u8]) -> Result<usize, Refuse> {
        self.gate(who)?;
        if self.read_ticket != 0 {
            return Err(Refuse::Busy);
        }
        if out.len() < self.read_min() {
            return Err(Refuse::TooSmall);
        }
        let n = self.fill(storage, out);
        if n == 0 {
            self.read_ticket = self.issue();
        }
        Ok(n)
    }

    /// Ticket of the parked READ_PCM, 0 if none.
    pub fn read_ticket(&self) -> Ticket {
        self.read_ticket
    }

    /// A parked READ_PCM can be completed.
    pub fn read_ready(&self) -> bool {
        self.read_ticket != 0 && self.ring_frames() > 0
    }

    /// The READ_PCM request `ticket` can stay in its queue: it is the parked
    /// one and no frame is waiting for it.
    pub fn read_rests(&self, ticket: Ticket) -> bool {
        ticket != 0 && ticket == self.read_ticket && !self.read_ready()
    }

    /// The READ_PCM request `ticket` was taken out of its queue.
    pub fn finish_read(&mut self, ticket: Ticket, storage: &[u8], out: &mut [u8]) -> ReadDone {
        if ticket == 0 || ticket != self.read_ticket {
            return ReadDone::Stale;
        }
        if self.ring_frames() == 0 {
            return ReadDone::Empty;
        }
        if out.len() < self.read_min() {
            // Parked while there was no stream, and the stream that came has
            // bigger frames than the buffer.
            self.read_ticket = 0;
            return ReadDone::TooSmall;
        }
        match self.fill(storage, out) {
            0 => ReadDone::Empty,
            n => {
                self.read_ticket = 0;
                ReadDone::Done(n)
            }
        }
    }

    /// The READ_PCM request `ticket` is gone (cancelled, or parking failed).
    /// The end of a leftover does not touch the request parked now.
    pub fn read_gone(&mut self, ticket: Ticket) {
        if ticket != 0 && ticket == self.read_ticket {
            self.read_ticket = 0;
        }
    }

    // ---- stream -------------------------------------------------------------

    /// A stream was created. The newest stream feeds the tap; an older one
    /// that still exists is ignored from here on. Returns false (and changes
    /// nothing) for a format the contract cannot describe.
    pub fn stream_created(&mut self, id: Id, format: PcmFormat) -> bool {
        if id == 0 || !format.is_valid() {
            return false;
        }
        self.remove_ring();
        self.stream = id;
        self.stream_state = StreamState::Paused;
        self.format = format;
        self.discontinuity = true;
        self.bump();
        true
    }

    /// The stream is gone. Returns true if it was the one feeding the tap.
    pub fn stream_destroyed(&mut self, id: Id) -> bool {
        if id == 0 || id != self.stream {
            return false;
        }
        self.remove_ring();
        self.stream = 0;
        self.stream_state = StreamState::None;
        self.format = PcmFormat::default();
        self.bump();
        true
    }

    /// Run / Pause of the stream. Returns true if the state changed.
    pub fn stream_running(&mut self, id: Id, running: bool) -> bool {
        if id == 0 || id != self.stream {
            return false;
        }
        let new = if running { StreamState::Running } else { StreamState::Paused };
        if self.stream_state == new {
            return false;
        }
        self.stream_state = new;
        self.bump();
        true
    }

    /// Stream `id` offers `bytes` of ring storage. Returns true if the ring
    /// now uses it; its length is then [`Session::ring_capacity`] (`bytes`
    /// rounded down to whole frames).
    pub fn install_ring(&mut self, id: Id, bytes: usize) -> bool {
        if id == 0 || id != self.stream {
            return false;
        }
        let fb = self.frame_bytes();
        if fb == 0 || bytes < fb {
            return false;
        }
        self.drop_ring();
        self.ring.set_capacity(bytes - bytes % fb);
        true
    }

    /// Stop using the ring storage (its frames are lost).
    pub fn remove_ring(&mut self) {
        self.drop_ring();
        self.ring.set_capacity(0);
    }

    /// Whole frames in `bytes`, and their byte length, for a packet of
    /// stream `id`; `None` if that stream does not feed the tap.
    fn packet(&self, id: Id, bytes: usize) -> Option<(usize, usize)> {
        let fb = self.frame_bytes();
        if id == 0 || id != self.stream || fb == 0 || bytes < fb {
            return None;
        }
        let frames = bytes / fb;
        Some((frames, frames * fb))
    }

    /// Count `frames` as rendered and make room for them. `None`: they are
    /// dropped (tap off). `Some(skip)`: append the packet minus its first
    /// `skip` bytes.
    fn admit(&mut self, storage: &[u8], frames: usize, bytes: usize) -> Option<usize> {
        self.rendered += frames as u64;
        let cap = self.ring.capacity();
        let fb = self.frame_bytes();
        if self.holder == 0 || cap == 0 || storage.len() != cap {
            self.dropped += frames as u64;
            self.discontinuity = true;
            return None;
        }
        // cap, the fill level and `bytes` are all multiples of the frame size.
        let skip = bytes.saturating_sub(cap);
        if skip > 0 {
            // One packet is larger than the whole ring: keep its newest part.
            self.dropped += (skip / fb) as u64;
            self.discontinuity = true;
        }
        let need = bytes - skip;
        let free = self.ring.free();
        if need > free {
            let over = need - free;
            self.ring.discard(over);
            self.dropped += (over / fb) as u64;
            self.discontinuity = true;
        }
        Some(skip)
    }

    /// `bytes` of admitted audio could not be stored (cannot happen: `admit`
    /// made room). Keep the numbering right anyway: the ring must end at the
    /// newest rendered frame, so empty it.
    fn lost(&mut self, bytes: usize) {
        self.drop_ring();
        if let Some(frames) = bytes.checked_div(self.frame_bytes()) {
            self.dropped += frames as u64;
        }
        self.discontinuity = true;
    }

    /// The audio engine finished a packet of stream `id`. A trailing partial
    /// frame is ignored.
    pub fn render(&mut self, id: Id, storage: &mut [u8], packet: &[u8]) {
        let Some((frames, bytes)) = self.packet(id, packet.len()) else {
            return;
        };
        if let Some(skip) = self.admit(storage, frames, bytes) {
            let stored = packet.get(skip..bytes).is_some_and(|data| self.ring.push(storage, data));
            if !stored {
                self.lost(bytes - skip);
            }
        }
    }

    /// A packet period of stream `id` passed that the audio engine cannot
    /// have filled: `bytes` of silence take its place, so the frame count
    /// keeps following the endpoint's clock.
    pub fn render_silence(&mut self, id: Id, storage: &mut [u8], bytes: usize) {
        let Some((frames, bytes)) = self.packet(id, bytes) else {
            return;
        };
        self.silence += frames as u64;
        if let Some(skip) = self.admit(storage, frames, bytes)
            && !self.ring.push_zeros(storage, bytes - skip)
        {
            self.lost(bytes - skip);
        }
    }
}
