//! Limits on what a KS client may make the render endpoint allocate.
//!
//! The RT packet geometry (`EvtAcxStreamAllocateRtPackets`) comes from the
//! client's KSPROPERTY_RTAUDIO_BUFFER request. ACX passes it on after
//! checking only the packet count, a non-zero size and the block alignment
//! (Acx01000.sys 10.0.26100, `AfxRtStream::EvtRtAudioBufferCallback`), and
//! the device object is open to interactive users (see the INX), so the
//! driver has to bound both the size of a packet and the number of streams:
//! the packets are nonpaged pool.
//!
//! No kernel calls, no `unsafe`, no allocation, no panics. Tested in
//! `host-tests/src/rt_tests.rs`; `acx.rs` also checks at compile time that
//! the sizes Windows asks for pass for every offered format.
//!
//! # The window
//! A packet must hold between [`MIN_PACKET_MS`] and [`MAX_PACKET_MS`] of
//! audio at the stream's byte rate, and never more than
//! [`MAX_PACKET_BYTES`].
//!
//! * Upper end: WASAPI itself accepts no more. Microsoft Learn,
//!   IAudioClient::Initialize, AUDCLNT_E_BUFFER_SIZE_ERROR: "the buffer
//!   duration value requested by an exclusive-mode client is out of range.
//!   The requested duration value for pull mode must not be greater than
//!   5000 milliseconds; for push mode the duration value must not be greater
//!   than 2 seconds." Whether the engine turns a 5000 ms pull-mode buffer
//!   into two packets of 5000 ms or of 2500 ms was not measured; the window
//!   takes the larger reading.
//! * Lower end: the stream timer is re-armed once per packet. The audio
//!   engine's period is 10 ms and Windows audio periods are a few
//!   milliseconds at the least, so 1 ms leaves room and still keeps a
//!   client from driving the timer DPC with one-frame packets.
//!
//! What the audio engine is expected to ask for (not measured live): in
//! shared mode two packets of one 10 ms engine period each, i.e. 1920 bytes
//! at 48 kHz/16-bit stereo, 3840 bytes at 48 kHz in 32-bit containers and
//! 7680 bytes at 96 kHz; a timer-driven (push) client gets one packet that
//! ACX rounds up to whole pages, so at least 4096 bytes.
//!
//! # Worst case
//! [`MAX_STREAMS`] streams of two packets of [`MAX_PACKET_MS`] at the
//! largest offered format (768000 bytes/s): 4 x 2 x 3.84 MB = 30.72 MB of
//! nonpaged pool, plus one page of rounding per packet. Before, it was
//! 2 GiB per stream and any number of streams.

use core::sync::atomic::{AtomicU32, Ordering};

/// Shortest packet, in milliseconds of audio.
pub const MIN_PACKET_MS: u64 = 1;
/// Longest packet, in milliseconds of audio.
pub const MAX_PACKET_MS: u64 = 5_000;
/// Longest packet in bytes, whatever the byte rate claims to be.
pub const MAX_PACKET_BYTES: u64 = 8 << 20;
/// Streams that may exist on the device at the same time. Windows uses one
/// (the audio engine's, or an exclusive-mode client's instead of it); the
/// rest is headroom for the moment one is closed while the next is opened.
pub const MAX_STREAMS: u32 = 4;

/// Smallest and largest accepted packet size in bytes (both inclusive) for
/// a stream of `bytes_per_sec`. The smallest is rounded down, so a packet
/// of exactly [`MIN_PACKET_MS`] passes at 44.1 kHz too; it is never 0.
pub const fn packet_bounds(bytes_per_sec: u32) -> (u32, u32) {
    let bps = bytes_per_sec as u64;
    let mut min = bps * MIN_PACKET_MS / 1000;
    if min == 0 {
        min = 1;
    }
    let mut max = bps * MAX_PACKET_MS / 1000;
    if max > MAX_PACKET_BYTES {
        max = MAX_PACKET_BYTES;
    }
    if max < min {
        max = min;
    }
    // min <= u32::MAX / 1000 + 1; max <= MAX_PACKET_BYTES, or == min.
    (min as u32, max as u32)
}

/// Whether `packet_count` packets of `packet_size` bytes may be allocated
/// for a stream of `bytes_per_sec`.
pub const fn packet_ok(packet_count: u32, max_packets: u32, packet_size: u32, bytes_per_sec: u32) -> bool {
    let (min, max) = packet_bounds(bytes_per_sec);
    packet_count >= 1 && packet_count <= max_packets && packet_size >= min && packet_size <= max
}

/// Counts the streams that exist on one device.
pub struct StreamSlots(AtomicU32);

impl StreamSlots {
    /// No stream.
    pub const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    /// Take a slot if fewer than `max` are taken.
    pub fn try_acquire(&self, max: u32) -> bool {
        self.0.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| if n < max { Some(n + 1) } else { None }).is_ok()
    }

    /// Give a slot back. A release without an acquire changes nothing.
    pub fn release(&self) {
        let _ = self.0.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
    }

    /// Slots taken.
    pub fn in_use(&self) -> u32 {
        self.0.load(Ordering::SeqCst)
    }
}

impl Default for StreamSlots {
    fn default() -> Self {
        Self::new()
    }
}
