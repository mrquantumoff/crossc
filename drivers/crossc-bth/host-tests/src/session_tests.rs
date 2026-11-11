use std::collections::VecDeque;
use std::vec;
use std::vec::Vec;

use crossc_ioctl::audio::{
    AUDIO_FLAG_ACX, AUDIO_FLAG_ATTACHED, AUDIO_FLAG_LINK, AudioState, PCM_FLAG_DISCONTINUITY, PcmFormat, PcmHeader,
    StreamState,
};

use crate::audio_core::{Detached, MIN_FRAME_BYTES, RING_MS, ReadDone, Refuse, Session, WaitDone, next_seq, ring_bytes};
use crate::ring_tests::Rng;

const F16: PcmFormat = PcmFormat { sample_rate: 48_000, channels: 2, container_bits: 16, valid_bits: 16 };
const F24: PcmFormat = PcmFormat { sample_rate: 48_000, channels: 2, container_bits: 32, valid_bits: 24 };
const F96: PcmFormat = PcmFormat { sample_rate: 96_000, channels: 2, container_bits: 32, valid_bits: 24 };
const HDR: usize = PcmHeader::SIZE;
/// File handles.
const A: usize = 0x1000;
const B: usize = 0x2000;
/// Streams.
const S1: usize = 0x10;
const S2: usize = 0x20;

/// Endpoint present, stream `S1` of `format` with a ring of `ring_frames`.
fn setup(format: PcmFormat, ring_frames: usize) -> (Session, Vec<u8>) {
    let mut s = Session::new();
    assert!(s.set_endpoint(true));
    assert!(s.stream_created(S1, format));
    assert!(s.install_ring(S1, ring_frames * format.frame_bytes()));
    let storage = vec![0u8; s.ring_capacity()];
    (s, storage)
}

/// A packet of `frames` 16-bit stereo frames whose content is their frame
/// index (`first`, `first + 1`, ...), so a reply can be checked sample by
/// sample.
fn packet16(first: u64, frames: usize) -> Vec<u8> {
    (0..frames as u64).flat_map(|i| ((first + i) as u32).to_le_bytes()).collect()
}

/// Split a READ_PCM reply and return (header, frame values).
fn parse16(reply: &[u8]) -> (PcmHeader, Vec<u32>) {
    let (h, data) = PcmHeader::split(reply).expect("well-formed reply");
    assert_eq!(data.len(), reply.len() - HDR, "reply length = header + frames");
    let vals = data.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
    (h, vals)
}

fn read_now(s: &mut Session, storage: &[u8], who: usize, cap: usize) -> Vec<u8> {
    let mut out = vec![0u8; cap];
    let n = s.begin_read(who, storage, &mut out).expect("read accepted");
    assert!(n > 0, "expected data, request was parked");
    out.truncate(n);
    out
}

#[test]
fn initial_state() {
    let s = Session::new();
    let st = s.state();
    assert_eq!(st.seq, 1);
    assert_eq!(st.flags, 0);
    assert_eq!(st.stream, StreamState::None);
    assert_eq!(st.format, PcmFormat::default());
    assert_eq!((st.frames_rendered, st.frames_dropped), (0, 0));
    assert_eq!(s.holder(), 0);
    assert!(!s.link());
    assert_eq!(Session::default().state(), st);
}

#[test]
fn seq_skips_zero() {
    assert_eq!(next_seq(1), 2);
    assert_eq!(next_seq(u32::MAX - 1), u32::MAX);
    assert_eq!(next_seq(u32::MAX), 1);
    assert_eq!(next_seq(0), 1);
}

#[test]
fn without_endpoint_everything_is_not_supported_but_state_works() {
    let mut s = Session::new();
    let mut out = [0u8; 64];
    assert_eq!(s.attach(A), Err(Refuse::NotSupported));
    assert_eq!(s.set_link(A, true), Err(Refuse::NotSupported));
    assert_eq!(s.begin_wait(A, 0), Err(Refuse::NotSupported));
    assert_eq!(s.begin_read(A, &[], &mut out), Err(Refuse::NotSupported));
    assert_eq!(s.detach(0), None);
    let st = s.state();
    assert_eq!(st.flags & AUDIO_FLAG_ACX, 0);
    assert_eq!(st.seq, 1);
}

#[test]
fn endpoint_flag_bumps_seq_once_per_change() {
    let mut s = Session::new();
    assert!(s.set_endpoint(true));
    assert_eq!(s.state().seq, 2);
    assert!(s.state().acx());
    assert!(!s.set_endpoint(true));
    assert_eq!(s.state().seq, 2);
    assert!(s.set_endpoint(false));
    assert_eq!(s.state().seq, 3);
    assert!(!s.state().acx());
}

#[test]
fn attach_is_exclusive_and_detach_clears_everything() {
    let mut s = Session::new();
    s.set_endpoint(true);
    assert_eq!(s.attach(0), Err(Refuse::Denied));
    let seq0 = s.state().seq;
    assert_eq!(s.attach(A), Ok(()));
    assert_eq!(s.holder(), A);
    assert_eq!(s.state().flags, AUDIO_FLAG_ACX | AUDIO_FLAG_ATTACHED);
    assert_ne!(s.state().seq, seq0);
    // Second ATTACH, from the same or another handle.
    assert_eq!(s.attach(B), Err(Refuse::Busy));
    assert_eq!(s.attach(A), Err(Refuse::Busy));
    // Other handles are denied, the state is unchanged by their attempts.
    let seq1 = s.state().seq;
    assert_eq!(s.set_link(B, true), Err(Refuse::Denied));
    assert_eq!(s.begin_wait(B, 0), Err(Refuse::Denied));
    assert_eq!(s.begin_read(B, &[], &mut [0u8; 64]), Err(Refuse::Denied));
    assert_eq!(s.detach(B), None);
    assert_eq!(s.state().seq, seq1);

    assert_eq!(s.set_link(A, true), Ok(true));
    assert!(s.link());
    assert_eq!(s.state().flags, AUDIO_FLAG_ACX | AUDIO_FLAG_ATTACHED | AUDIO_FLAG_LINK);
    let seq2 = s.state().seq;
    assert_eq!(s.detach(A), Some(Detached { link_changed: true }));
    assert_eq!(s.holder(), 0);
    assert!(!s.link());
    assert_eq!(s.state().flags, AUDIO_FLAG_ACX);
    assert_ne!(s.state().seq, seq2);
    // Idempotent.
    assert_eq!(s.detach(A), None);
    assert_eq!(s.detach(0), None);
    // The former holder is an outsider now.
    assert_eq!(s.set_link(A, true), Err(Refuse::Denied));
    // Somebody else can attach; "detach whoever" works; link was not set.
    assert_eq!(s.attach(B), Ok(()));
    assert_eq!(s.detach(0), Some(Detached { link_changed: false }));
}

#[test]
fn set_link_bumps_only_on_change() {
    let mut s = Session::new();
    s.set_endpoint(true);
    s.attach(A).unwrap();
    let seq = s.state().seq;
    assert_eq!(s.set_link(A, false), Ok(false));
    assert_eq!(s.state().seq, seq);
    assert_eq!(s.set_link(A, true), Ok(true));
    let seq = s.state().seq;
    assert_eq!(s.set_link(A, true), Ok(false));
    assert_eq!(s.state().seq, seq);
    assert_eq!(s.set_link(A, false), Ok(true));
    assert_ne!(s.state().seq, seq);
}

#[test]
fn every_state_change_gives_a_new_nonzero_seq() {
    let mut s = Session::new();
    let mut seen = vec![s.state().seq];
    let mut step = |s: &mut Session, what: &str| {
        let q = s.state().seq;
        assert_ne!(q, 0, "{what}");
        assert_ne!(q, *seen.last().unwrap(), "{what}: seq did not change");
        seen.push(q);
    };
    s.set_endpoint(true);
    step(&mut s, "endpoint");
    assert!(s.stream_created(S1, F24));
    step(&mut s, "stream created");
    assert!(s.stream_running(S1, true));
    step(&mut s, "run");
    s.attach(A).unwrap();
    step(&mut s, "attach");
    s.set_link(A, true).unwrap();
    step(&mut s, "link");
    assert!(s.stream_running(S1, false));
    step(&mut s, "pause");
    assert!(s.stream_created(S2, F96));
    step(&mut s, "format change");
    assert!(s.stream_destroyed(S2));
    step(&mut s, "stream destroyed");
    s.detach(A).unwrap();
    step(&mut s, "detach");
    s.set_endpoint(false);
    step(&mut s, "endpoint gone");
}

#[test]
fn counters_do_not_bump_seq() {
    let (mut s, mut st) = setup(F16, 100);
    s.attach(A).unwrap();
    let seq = s.state().seq;
    s.render(S1, &mut st, &packet16(0, 10));
    s.render_silence(S1, &mut st, 40);
    let _ = read_now(&mut s, &st, A, HDR + 400);
    assert_eq!(s.state().seq, seq);
    assert_eq!(s.state().frames_rendered, 20);
}

// ---- WAIT_EVENT -------------------------------------------------------------

#[test]
fn wait_completes_at_once_when_seq_differs() {
    let mut s = Session::new();
    s.set_endpoint(true);
    s.attach(A).unwrap();
    let now = s.state();
    assert_eq!(s.begin_wait(A, 0), Ok(Some(now)));
    assert_eq!(s.begin_wait(A, now.seq.wrapping_sub(1)), Ok(Some(now)));
    assert!(!s.wait_ready());
}

#[test]
fn wait_parks_until_the_state_changes() {
    let mut s = Session::new();
    s.set_endpoint(true);
    s.attach(A).unwrap();
    let seq = s.state().seq;
    assert_eq!(s.wait_ticket(), 0);
    assert_eq!(s.begin_wait(A, seq), Ok(None));
    let t = s.wait_ticket();
    assert_ne!(t, 0);
    assert!(!s.wait_ready());
    assert!(s.wait_rests(t));
    // One outstanding; a refused request does not change the ticket.
    assert_eq!(s.begin_wait(A, seq), Err(Refuse::Busy));
    assert_eq!(s.begin_wait(A, 0), Err(Refuse::Busy));
    assert_eq!(s.wait_ticket(), t);
    // Taken out of the queue too early: park it again.
    assert_eq!(s.finish_wait(t), WaitDone::NotReady);
    // Parking is not a state change.
    assert_eq!(s.state().seq, seq);
    // A change wakes it.
    assert!(s.stream_created(S1, F24));
    assert!(s.wait_ready());
    assert!(!s.wait_rests(t));
    let WaitDone::Done(st) = s.finish_wait(t) else { panic!("not done") };
    assert_ne!(st.seq, seq);
    assert_eq!(st, s.state());
    assert_eq!(st.stream, StreamState::Paused);
    assert_eq!(st.format, F24);
    // Completed: not parked any more, a new one is accepted under a new ticket.
    assert!(!s.wait_ready());
    assert_eq!(s.wait_ticket(), 0);
    assert!(!s.wait_rests(t));
    assert_eq!(s.finish_wait(t), WaitDone::Stale);
    assert_eq!(s.begin_wait(A, st.seq), Ok(None));
    assert!(s.wait_ticket() != 0 && s.wait_ticket() != t);
}

#[test]
fn wait_of_a_dead_session_is_stale() {
    let mut s = Session::new();
    s.set_endpoint(true);
    s.attach(A).unwrap();
    let seq = s.state().seq;
    assert_eq!(s.begin_wait(A, seq), Ok(None));
    let old = s.wait_ticket();
    s.detach(A).unwrap();
    assert!(!s.wait_ready());
    assert!(!s.wait_rests(old));
    assert_eq!(s.finish_wait(old), WaitDone::Stale);
    // A's old request must not be handed B's state either.
    s.attach(B).unwrap();
    assert_eq!(s.finish_wait(old), WaitDone::Stale);
    assert_eq!(s.finish_wait(0), WaitDone::Stale);
    // B's own WAIT is not blocked by A's leftover.
    assert_eq!(s.begin_wait(B, s.state().seq), Ok(None));
    let new = s.wait_ticket();
    assert_ne!(new, old);
    // The leftover, found in the queue or cancelled there, is still told
    // apart from B's request and does not disturb it.
    assert_eq!(s.finish_wait(old), WaitDone::Stale);
    s.wait_gone(old);
    assert_eq!(s.wait_ticket(), new);
    assert_eq!(s.finish_wait(new), WaitDone::NotReady);
}

#[test]
fn cancelled_wait_frees_the_slot() {
    let mut s = Session::new();
    s.set_endpoint(true);
    s.attach(A).unwrap();
    let seq = s.state().seq;
    assert_eq!(s.begin_wait(A, seq), Ok(None));
    let t = s.wait_ticket();
    // The end of another request does not touch it.
    s.wait_gone(t + 1);
    s.wait_gone(0);
    assert_eq!(s.begin_wait(A, seq), Err(Refuse::Busy));
    s.wait_gone(t);
    assert_eq!(s.wait_ticket(), 0);
    assert_eq!(s.begin_wait(A, seq), Ok(None));
    assert!(s.wait_ticket() > t);
}

// ---- READ_PCM ---------------------------------------------------------------

#[test]
fn ring_size_is_250_ms() {
    assert_eq!(RING_MS, 250);
    assert_eq!(ring_bytes(&F16), 12_000 * 4);
    assert_eq!(ring_bytes(&F24), 12_000 * 8);
    assert_eq!(ring_bytes(&F96), 24_000 * 8);
    assert_eq!(ring_bytes(&PcmFormat::default()), 0);
    assert_eq!(ring_bytes(&PcmFormat { channels: 6, ..F24 }), 0);
    // Rounded down to whole frames, never more than offered.
    let mut s = Session::new();
    s.set_endpoint(true);
    assert!(s.stream_created(S1, F24));
    assert!(s.install_ring(S1, 8 * 10 + 5));
    assert_eq!(s.ring_capacity(), 80);
    assert!(!s.install_ring(S1, 7));
    assert!(!s.install_ring(S2, 800));
}

#[test]
fn read_buffer_must_hold_header_and_one_frame() {
    let mut s = Session::new();
    s.set_endpoint(true);
    s.attach(A).unwrap();
    // No stream: the smallest frame of the contract decides.
    assert_eq!(s.begin_read(A, &[], &mut [0u8; HDR - 1]), Err(Refuse::TooSmall));
    assert_eq!(s.begin_read(A, &[], &mut [0u8; HDR + MIN_FRAME_BYTES - 1]), Err(Refuse::TooSmall));
    assert_eq!(s.begin_read(A, &[], &mut [0u8; HDR + MIN_FRAME_BYTES]), Ok(0));
    s.read_gone(s.read_ticket());
    // With a stream: its frame size.
    assert!(s.stream_created(S1, F24));
    assert_eq!(s.begin_read(A, &[], &mut [0u8; HDR + 7]), Err(Refuse::TooSmall));
    assert_eq!(s.begin_read(A, &[], &mut [0u8; HDR + 8]), Ok(0));
}

#[test]
fn read_parks_then_gets_the_rendered_frames() {
    let (mut s, mut st) = setup(F16, 100);
    s.attach(A).unwrap();
    let mut out = vec![0u8; HDR + 400];
    assert_eq!(s.read_ticket(), 0);
    let seq = s.state().seq;
    assert_eq!(s.begin_read(A, &st, &mut out), Ok(0));
    let t = s.read_ticket();
    assert_ne!(t, 0);
    assert!(!s.read_ready());
    assert!(s.read_rests(t));
    // One outstanding; a refused request does not change the ticket.
    assert_eq!(s.begin_read(A, &st, &mut out), Err(Refuse::Busy));
    assert_eq!(s.read_ticket(), t);
    // Retrieved too early.
    assert_eq!(s.finish_read(t, &st, &mut out), ReadDone::Empty);
    // Parking is not a state change.
    assert_eq!(s.state().seq, seq);

    s.render(S1, &mut st, &packet16(0, 10));
    assert!(s.read_ready());
    assert!(!s.read_rests(t));
    let ReadDone::Done(n) = s.finish_read(t, &st, &mut out) else { panic!("not done") };
    assert_eq!(n, HDR + 40);
    let (h, vals) = parse16(&out[..n]);
    assert_eq!(h.first_frame, 0);
    assert_eq!(h.frames, 10);
    assert_eq!(h.format, F16);
    // First chunk of a session: the tap was off before.
    assert_eq!(h.flags, PCM_FLAG_DISCONTINUITY);
    assert_eq!(vals, (0..10).collect::<Vec<u32>>());
    assert!(!s.read_ready());
    assert_eq!(s.read_ticket(), 0);
    assert!(!s.read_rests(t));
    assert_eq!(s.finish_read(t, &st, &mut out), ReadDone::Stale);

    // Data already there: completes at once, no gap, continues the numbering.
    s.render(S1, &mut st, &packet16(10, 10));
    let reply = read_now(&mut s, &st, A, HDR + 400);
    let (h, vals) = parse16(&reply);
    assert_eq!((h.first_frame, h.frames, h.flags), (10, 10, 0));
    assert_eq!(vals, (10..20).collect::<Vec<u32>>());
    assert_eq!(s.state().frames_rendered, 20);
    assert_eq!(s.state().frames_dropped, 0);
}

#[test]
fn read_returns_as_many_whole_frames_as_fit() {
    let (mut s, mut st) = setup(F16, 100);
    s.attach(A).unwrap();
    s.render(S1, &mut st, &packet16(0, 10));
    // Room for 3 frames and 3 spare bytes.
    let reply = read_now(&mut s, &st, A, HDR + 15);
    assert_eq!(reply.len(), HDR + 12);
    let (h, vals) = parse16(&reply);
    assert_eq!((h.first_frame, h.frames), (0, 3));
    assert_eq!(vals, [0, 1, 2]);
    // Exactly one frame.
    let (h, vals) = parse16(&read_now(&mut s, &st, A, HDR + 4));
    assert_eq!((h.first_frame, h.frames, h.flags), (3, 1, 0));
    assert_eq!(vals, [3]);
    // The rest, with a large buffer.
    let (h, vals) = parse16(&read_now(&mut s, &st, A, HDR + 4096));
    assert_eq!((h.first_frame, h.frames, h.flags), (4, 6, 0));
    assert_eq!(vals, [4, 5, 6, 7, 8, 9]);
    assert_eq!(s.state().frames_dropped, 0);
}

#[test]
fn read_never_touches_bytes_behind_the_reply() {
    let (mut s, mut st) = setup(F16, 100);
    s.attach(A).unwrap();
    s.render(S1, &mut st, &packet16(0, 2));
    let mut out = vec![0x5Au8; HDR + 64];
    let n = s.begin_read(A, &st, &mut out).unwrap();
    assert_eq!(n, HDR + 8);
    assert!(out[n..].iter().all(|&b| b == 0x5A));
}

#[test]
fn nobody_attached_drops_and_flags_the_gap() {
    let (mut s, mut st) = setup(F16, 100);
    s.render(S1, &mut st, &packet16(0, 30));
    let state = s.state();
    assert_eq!((state.frames_rendered, state.frames_dropped), (30, 30));
    // Attach later: nothing of the past is delivered.
    s.attach(A).unwrap();
    let mut out = vec![0u8; HDR + 400];
    assert_eq!(s.begin_read(A, &st, &mut out), Ok(0));
    let t = s.read_ticket();
    s.render(S1, &mut st, &packet16(30, 10));
    let ReadDone::Done(n) = s.finish_read(t, &st, &mut out) else { panic!() };
    let (h, vals) = parse16(&out[..n]);
    assert_eq!((h.first_frame, h.frames, h.flags), (30, 10, PCM_FLAG_DISCONTINUITY));
    assert_eq!(vals, (30..40).collect::<Vec<u32>>());
    assert_eq!(s.state().frames_dropped, 30);
}

#[test]
fn slow_reader_loses_the_oldest_frames() {
    let (mut s, mut st) = setup(F16, 25);
    s.attach(A).unwrap();
    // Consume the "first chunk" flag.
    s.render(S1, &mut st, &packet16(0, 10));
    let (h, _) = parse16(&read_now(&mut s, &st, A, HDR + 400));
    assert_eq!(h.flags, PCM_FLAG_DISCONTINUITY);
    // 40 frames into a 25-frame ring without a read: the first 15 fall out.
    for p in 1..5u64 {
        s.render(S1, &mut st, &packet16(p * 10, 10));
    }
    assert_eq!(s.state().frames_rendered, 50);
    assert_eq!(s.state().frames_dropped, 15);
    let (h, vals) = parse16(&read_now(&mut s, &st, A, HDR + 400));
    assert_eq!((h.first_frame, h.frames, h.flags), (25, 25, PCM_FLAG_DISCONTINUITY));
    assert_eq!(vals, (25..50).collect::<Vec<u32>>());
    // After the gap was reported the flag is clear again.
    s.render(S1, &mut st, &packet16(50, 10));
    let (h, _) = parse16(&read_now(&mut s, &st, A, HDR + 400));
    assert_eq!((h.first_frame, h.flags), (50, 0));
    assert_eq!(s.state().frames_dropped, 15);
}

#[test]
fn packet_larger_than_the_ring_keeps_its_newest_part() {
    let (mut s, mut st) = setup(F16, 8);
    s.attach(A).unwrap();
    s.render(S1, &mut st, &packet16(0, 5));
    s.render(S1, &mut st, &packet16(5, 20));
    assert_eq!(s.state().frames_rendered, 25);
    assert_eq!(s.state().frames_dropped, 17);
    let (h, vals) = parse16(&read_now(&mut s, &st, A, HDR + 400));
    assert_eq!((h.first_frame, h.frames, h.flags), (17, 8, PCM_FLAG_DISCONTINUITY));
    assert_eq!(vals, (17..25).collect::<Vec<u32>>());
}

#[test]
fn trailing_partial_frame_is_ignored() {
    let (mut s, mut st) = setup(F16, 100);
    s.attach(A).unwrap();
    let mut p = packet16(0, 3);
    p.extend_from_slice(&[0xEE, 0xEE, 0xEE]);
    s.render(S1, &mut st, &p);
    s.render(S1, &mut st, &[1, 2, 3]); // less than one frame
    assert_eq!(s.state().frames_rendered, 3);
    let (h, vals) = parse16(&read_now(&mut s, &st, A, HDR + 400));
    assert_eq!(h.frames, 3);
    assert_eq!(vals, [0, 1, 2]);
}

#[test]
fn silence_is_rendered_not_dropped() {
    let (mut s, mut st) = setup(F16, 100);
    s.attach(A).unwrap();
    s.render(S1, &mut st, &packet16(0, 4));
    s.render_silence(S1, &mut st, 4 * 4 + 3);
    s.render(S1, &mut st, &packet16(8, 4));
    let state = s.state();
    assert_eq!((state.frames_rendered, state.frames_dropped), (12, 0));
    assert_eq!(s.silence_frames(), 4);
    let (h, vals) = parse16(&read_now(&mut s, &st, A, HDR + 400));
    assert_eq!((h.first_frame, h.frames), (0, 12));
    assert_eq!(vals, [0, 1, 2, 3, 0, 0, 0, 0, 8, 9, 10, 11]);
    // With nobody attached it is dropped like any other packet.
    s.detach(A).unwrap();
    s.render_silence(S1, &mut st, 40);
    let state = s.state();
    assert_eq!((state.frames_rendered, state.frames_dropped), (22, 10));
    assert_eq!(s.silence_frames(), 14);
}

#[test]
fn detach_discards_the_ring_and_the_parked_requests() {
    let (mut s, mut st) = setup(F16, 100);
    s.attach(A).unwrap();
    s.render(S1, &mut st, &packet16(0, 10));
    let _ = read_now(&mut s, &st, A, HDR + 8); // 2 frames delivered, 8 left
    let seq = s.state().seq;
    assert_eq!(s.begin_wait(A, seq), Ok(None));
    s.detach(A).unwrap();
    assert_eq!(s.state().frames_dropped, 8);
    assert!(!s.wait_ready() && !s.read_ready());
    assert_eq!((s.wait_ticket(), s.read_ticket()), (0, 0));
    // The next session starts clean and flagged.
    s.attach(B).unwrap();
    let mut out = vec![0u8; HDR + 400];
    assert_eq!(s.begin_read(B, &st, &mut out), Ok(0));
    let t = s.read_ticket();
    // A leftover READ, pulled out of the queue, is stale even while B has
    // one parked.
    assert_eq!(s.finish_read(t - 1, &st, &mut out), ReadDone::Stale);
    s.render(S1, &mut st, &packet16(10, 5));
    assert_eq!(s.finish_read(t - 1, &st, &mut out), ReadDone::Stale);
    assert_eq!(s.finish_read(0, &st, &mut out), ReadDone::Stale);
    let ReadDone::Done(n) = s.finish_read(t, &st, &mut out) else { panic!() };
    let (h, vals) = parse16(&out[..n]);
    assert_eq!((h.first_frame, h.frames, h.flags), (10, 5, PCM_FLAG_DISCONTINUITY));
    assert_eq!(vals, [10, 11, 12, 13, 14]);
}

#[test]
fn cancelled_read_frees_the_slot() {
    let (mut s, st) = setup(F16, 100);
    s.attach(A).unwrap();
    let mut out = vec![0u8; HDR + 400];
    assert_eq!(s.begin_read(A, &st, &mut out), Ok(0));
    let t = s.read_ticket();
    s.read_gone(t + 1);
    s.read_gone(0);
    assert_eq!(s.begin_read(A, &st, &mut out), Err(Refuse::Busy));
    s.read_gone(t);
    assert_eq!(s.read_ticket(), 0);
    assert_eq!(s.begin_read(A, &st, &mut out), Ok(0));
    assert!(s.read_ticket() > t);
}

#[test]
fn parked_read_too_small_for_the_stream_that_came() {
    let mut s = Session::new();
    s.set_endpoint(true);
    s.attach(A).unwrap();
    // Parked without a stream with room for one 16-bit stereo frame.
    let mut out = vec![0u8; HDR + 4];
    assert_eq!(s.begin_read(A, &[], &mut out), Ok(0));
    assert!(s.stream_created(S1, F24));
    assert!(s.install_ring(S1, 80));
    let mut st = vec![0u8; s.ring_capacity()];
    s.render(S1, &mut st, &[7u8; 16]);
    assert!(s.read_ready());
    assert_eq!(s.finish_read(s.read_ticket(), &st, &mut out), ReadDone::TooSmall);
    // Failed, not parked any more; the frames are still there for a proper read.
    assert!(!s.read_ready());
    let reply = read_now(&mut s, &st, A, HDR + 64);
    let (h, data) = PcmHeader::split(&reply).unwrap();
    assert_eq!((h.frames, h.format), (2, F24));
    assert_eq!(data, &[7u8; 16]);
}

// ---- stream / format ---------------------------------------------------------

#[test]
fn stream_states() {
    let mut s = Session::new();
    s.set_endpoint(true);
    assert!(!s.stream_created(0, F24));
    assert!(!s.stream_created(S1, PcmFormat::default()));
    assert!(!s.stream_created(S1, PcmFormat { container_bits: 24, valid_bits: 24, ..F24 }));
    assert_eq!(s.state().stream, StreamState::None);

    assert!(s.stream_created(S1, F24));
    assert_eq!((s.state().stream, s.state().format), (StreamState::Paused, F24));
    assert!(!s.stream_running(S1, false));
    assert!(s.stream_running(S1, true));
    assert_eq!(s.state().stream, StreamState::Running);
    assert!(!s.stream_running(S1, true));
    assert!(s.stream_running(S1, false));
    assert_eq!(s.state().stream, StreamState::Paused);
    assert!(s.stream_destroyed(S1));
    assert_eq!((s.state().stream, s.state().format), (StreamState::None, PcmFormat::default()));
    assert!(!s.stream_destroyed(S1));
    assert!(!s.stream_running(S1, true));
}

#[test]
fn newest_stream_owns_the_tap_and_the_old_one_is_ignored() {
    let (mut s, mut st1) = setup(F16, 100);
    s.attach(A).unwrap();
    s.stream_running(S1, true);
    s.render(S1, &mut st1, &packet16(0, 10));

    // The format is switched: a new stream appears before the old one is gone.
    assert!(s.stream_created(S2, F24));
    assert_eq!(s.ring_capacity(), 0, "the old stream's storage is no longer used");
    assert_eq!(s.state().frames_dropped, 10, "frames of the old format are lost");
    assert_eq!((s.state().stream, s.state().format), (StreamState::Paused, F24));
    assert!(s.install_ring(S2, 80 * 8));
    let mut st2 = vec![0u8; s.ring_capacity()];

    // The old stream's late events change nothing.
    let before = s.state();
    s.render(S1, &mut st2, &packet16(10, 10));
    s.render_silence(S1, &mut st2, 40);
    assert!(!s.stream_running(S1, false));
    assert!(!s.install_ring(S1, 400));
    assert!(!s.stream_destroyed(S1));
    assert_eq!(s.state(), before);

    // Frames of the new format only, flagged, numbered after the old ones.
    s.render(S2, &mut st2, &[0x11u8; 24]);
    let reply = read_now(&mut s, &st2, A, HDR + 4096);
    let (h, data) = PcmHeader::split(&reply).unwrap();
    assert_eq!(h.format, F24);
    assert_eq!((h.first_frame, h.frames, h.flags), (10, 3, PCM_FLAG_DISCONTINUITY));
    assert_eq!(data, &[0x11u8; 24]);
}

#[test]
fn removing_the_ring_drops_its_frames_and_reads_park() {
    let (mut s, mut st) = setup(F16, 100);
    s.attach(A).unwrap();
    s.render(S1, &mut st, &packet16(0, 10));
    s.remove_ring();
    assert_eq!(s.ring_capacity(), 0);
    assert_eq!(s.state().frames_dropped, 10);
    let mut out = vec![0u8; HDR + 400];
    assert_eq!(s.begin_read(A, &[], &mut out), Ok(0));
    // Rendering without storage is dropped, the read stays parked.
    s.render(S1, &mut [], &packet16(10, 10));
    assert!(!s.read_ready());
    assert_eq!(s.state().frames_dropped, 20);
    // Storage of the wrong length is treated the same.
    assert!(s.install_ring(S1, 400));
    s.render(S1, &mut vec![0u8; 399], &packet16(20, 10));
    assert_eq!(s.state().frames_dropped, 30);
    assert!(!s.read_ready());
}

#[test]
fn pause_keeps_the_ring_for_the_reader() {
    let (mut s, mut st) = setup(F16, 100);
    s.attach(A).unwrap();
    s.stream_running(S1, true);
    s.render(S1, &mut st, &packet16(0, 10));
    s.stream_running(S1, false);
    let (h, vals) = parse16(&read_now(&mut s, &st, A, HDR + 400));
    assert_eq!(h.frames, 10);
    assert_eq!(vals.len(), 10);
    assert_eq!(s.state().frames_dropped, 0);
    // Resumed: the numbering continues without a gap.
    s.stream_running(S1, true);
    s.render(S1, &mut st, &packet16(10, 10));
    let (h, _) = parse16(&read_now(&mut s, &st, A, HDR + 400));
    assert_eq!((h.first_frame, h.flags), (10, 0));
}

// ---- rules a mutation run found unpinned -------------------------------------

#[test]
fn detach_forgets_a_parked_read_too() {
    let (mut s, st) = setup(F16, 16);
    s.attach(A).unwrap();
    let mut out = vec![0u8; HDR + 64];
    assert_eq!(s.begin_read(A, &st, &mut out), Ok(0));
    assert_ne!(s.read_ticket(), 0);
    s.detach(A).unwrap();
    assert_eq!(s.read_ticket(), 0);
    // The next session's first READ_PCM is not refused as "one outstanding".
    s.attach(B).unwrap();
    assert_eq!(s.begin_read(B, &st, &mut out), Ok(0));
}

/// One chunk of 16-bit stereo, read at once.
fn chunk(s: &mut Session, storage: &[u8]) -> PcmHeader {
    parse16(&read_now(s, storage, A, HDR + 400)).0
}

#[test]
fn a_new_stream_flags_the_gap_even_if_the_reader_had_drained_the_ring() {
    let (mut s, mut st) = setup(F16, 16);
    s.attach(A).unwrap();
    s.render(S1, &mut st, &packet16(0, 4));
    assert_eq!(chunk(&mut s, &st).flags, PCM_FLAG_DISCONTINUITY);
    s.render(S1, &mut st, &packet16(4, 4));
    assert_eq!(chunk(&mut s, &st).flags, 0);
    // The ring is empty, nothing is lost; the stream is replaced all the same.
    assert!(s.stream_destroyed(S1));
    assert!(s.stream_created(S2, F16));
    assert!(s.install_ring(S2, 64));
    assert_eq!(s.state().frames_dropped, 0);
    s.render(S2, &mut st, &packet16(8, 4));
    let h = chunk(&mut s, &st);
    assert_eq!((h.first_frame, h.frames, h.flags), (8, 4, PCM_FLAG_DISCONTINUITY));
}

#[test]
fn frames_lost_with_the_ring_storage_are_flagged() {
    let (mut s, mut st) = setup(F16, 16);
    s.attach(A).unwrap();
    s.render(S1, &mut st, &packet16(0, 4));
    let _ = chunk(&mut s, &st);
    // Unread frames are in the ring when the stream gives its storage up
    // (ReleaseHardware) and takes new storage (PrepareHardware).
    s.render(S1, &mut st, &packet16(4, 4));
    s.remove_ring();
    assert!(s.install_ring(S1, 64));
    s.render(S1, &mut st, &packet16(8, 4));
    let h = chunk(&mut s, &st);
    assert_eq!((h.first_frame, h.frames, h.flags), (8, 4, PCM_FLAG_DISCONTINUITY));
    assert_eq!(s.state().frames_dropped, 4);
}

#[test]
fn frames_rendered_without_ring_storage_are_flagged() {
    let (mut s, mut st) = setup(F16, 16);
    s.attach(A).unwrap();
    s.render(S1, &mut st, &packet16(0, 4));
    let _ = chunk(&mut s, &st);
    // The ring is empty when its storage goes; the packet rendered without
    // storage is the only loss.
    s.remove_ring();
    assert_eq!(s.state().frames_dropped, 0);
    s.render(S1, &mut [], &packet16(4, 4));
    assert!(s.install_ring(S1, 64));
    s.render(S1, &mut st, &packet16(8, 4));
    let h = chunk(&mut s, &st);
    assert_eq!((h.first_frame, h.frames, h.flags), (8, 4, PCM_FLAG_DISCONTINUITY));
    assert_eq!(s.state().frames_dropped, 4);
}

// ---- tickets ------------------------------------------------------------------

/// The same handle attaches again while a request of its former session is
/// still on its way to the queue: that request is a leftover, although its
/// handle is the attached one.
#[test]
fn a_request_of_the_former_session_of_the_same_handle_is_a_leftover() {
    let (mut s, mut st) = setup(F16, 16);
    let mut out = vec![0u8; HDR + 64];
    s.attach(A).unwrap();
    assert_eq!(s.begin_wait(A, s.state().seq), Ok(None));
    assert_eq!(s.begin_read(A, &st, &mut out), Ok(0));
    let (old_wait, old_read) = (s.wait_ticket(), s.read_ticket());
    assert_ne!(old_wait, old_read);
    s.detach(A).unwrap();
    s.attach(A).unwrap();

    // Not parked for the new session: no reason to leave them in a queue...
    assert!(!s.wait_rests(old_wait) && !s.read_rests(old_read));
    // ...and the new session's own requests are accepted, not "busy".
    assert_eq!(s.begin_wait(A, s.state().seq), Ok(None));
    assert_eq!(s.begin_read(A, &st, &mut out), Ok(0));
    let (wait, read) = (s.wait_ticket(), s.read_ticket());
    assert!(![0, old_wait, old_read].contains(&wait) && ![0, old_wait, old_read].contains(&read));

    // The leftovers are refused when taken out of the queue, also once the
    // new requests could be completed, and their end frees nothing.
    assert!(s.set_link(A, true).unwrap());
    s.render(S1, &mut st, &packet16(0, 4));
    assert!(s.wait_ready() && s.read_ready());
    assert_eq!(s.finish_wait(old_wait), WaitDone::Stale);
    assert_eq!(s.finish_read(old_read, &st, &mut out), ReadDone::Stale);
    s.wait_gone(old_wait);
    s.read_gone(old_read);
    assert_eq!((s.wait_ticket(), s.read_ticket()), (wait, read));
    assert_eq!(s.begin_wait(A, 0), Err(Refuse::Busy));
    assert_eq!(s.begin_read(A, &st, &mut out), Err(Refuse::Busy));

    // The new requests get their replies.
    assert!(matches!(s.finish_wait(wait), WaitDone::Done(_)));
    assert_eq!(s.finish_read(read, &st, &mut out), ReadDone::Done(HDR + 16));
}

#[test]
fn tickets_are_never_reused_and_do_not_bump_seq() {
    let (mut s, st) = setup(F16, 16);
    let mut out = vec![0u8; HDR + 64];
    let mut seen = Vec::new();
    for round in 0..50 {
        s.attach(A).unwrap();
        let seq = s.state().seq;
        assert_eq!(s.begin_wait(A, seq), Ok(None));
        assert_eq!(s.begin_read(A, &st, &mut out), Ok(0));
        assert_eq!(s.state().seq, seq);
        for t in [s.wait_ticket(), s.read_ticket()] {
            assert!(t != 0 && !seen.contains(&t), "round {round}: ticket {t}");
            seen.push(t);
        }
        if round % 2 == 0 {
            s.wait_gone(s.wait_ticket());
            s.read_gone(s.read_ticket());
            assert_eq!(s.state().seq, seq);
        }
        s.detach(A).unwrap();
    }
}

// ---- model test --------------------------------------------------------------

/// Random interleaving of render / silence / read / attach / detach against
/// a straightforward model. Checks, after every step:
/// * rendered = delivered + dropped + frames still in the ring,
/// * delivered frames are exactly the frames the model expects, in order,
///   with their original content,
/// * the discontinuity flag is set exactly when the chunk does not continue
///   the previous chunk of the same session,
/// * the header numbering matches the content.
#[test]
fn tap_matches_a_model_under_random_load() {
    let mut rng = Rng(0xC0FF_EE00_1234_5678);
    for ring_frames in [1usize, 3, 16, 100] {
        let (mut s, mut st) = setup(F16, ring_frames);
        // Model: frames in the ring (index, is_silence), newest at the back.
        let mut ring: VecDeque<(u64, bool)> = VecDeque::new();
        let mut rendered = 0u64;
        let mut dropped = 0u64;
        let mut delivered = 0u64;
        let mut attached = false;
        // End of the last chunk delivered in this session, if any.
        let mut last_end: Option<u64> = None;
        let mut lost_since_last = true;

        for _ in 0..30_000 {
            match rng.below(10) {
                0..=3 => {
                    let frames = 1 + rng.below(ring_frames + 4);
                    let silent = rng.below(8) == 0;
                    if silent {
                        s.render_silence(S1, &mut st, frames * 4);
                    } else {
                        s.render(S1, &mut st, &packet16(rendered, frames));
                    }
                    for i in 0..frames as u64 {
                        if attached {
                            ring.push_back((rendered + i, silent));
                        } else {
                            dropped += 1;
                            lost_since_last = true;
                        }
                    }
                    rendered += frames as u64;
                    while ring.len() > ring_frames {
                        ring.pop_front();
                        dropped += 1;
                        lost_since_last = true;
                    }
                }
                4..=7 => {
                    if !attached {
                        assert_eq!(s.begin_read(A, &st, &mut [0u8; 64]), Err(Refuse::Denied));
                        continue;
                    }
                    let room = 1 + rng.below(ring_frames + 3);
                    let mut out = vec![0u8; HDR + room * 4 + rng.below(4)];
                    let n = s.begin_read(A, &st, &mut out).unwrap();
                    if ring.is_empty() {
                        assert_eq!(n, 0);
                        s.read_gone(s.read_ticket());
                        continue;
                    }
                    let want = room.min(ring.len());
                    assert_eq!(n, HDR + want * 4);
                    let (h, vals) = parse16(&out[..n]);
                    let first = ring.front().unwrap().0;
                    assert_eq!(h.first_frame, first);
                    assert_eq!(h.frames as usize, want);
                    assert_eq!(h.format, F16);
                    let expect_flag = lost_since_last || last_end != Some(first);
                    assert_eq!(h.flags == PCM_FLAG_DISCONTINUITY, expect_flag, "at frame {first}");
                    for v in vals {
                        let (idx, silent) = ring.pop_front().unwrap();
                        assert_eq!(v, if silent { 0 } else { idx as u32 });
                    }
                    delivered += want as u64;
                    last_end = Some(first + want as u64);
                    lost_since_last = false;
                }
                8 => {
                    if attached {
                        assert!(s.detach(A).is_some());
                        dropped += ring.len() as u64;
                        ring.clear();
                        attached = false;
                    } else {
                        assert_eq!(s.detach(A), None);
                    }
                }
                _ => {
                    if attached {
                        assert_eq!(s.attach(A), Err(Refuse::Busy));
                    } else {
                        s.attach(A).unwrap();
                        attached = true;
                        last_end = None;
                        lost_since_last = true;
                    }
                }
            }
            let state: AudioState = s.state();
            assert_eq!(state.frames_rendered, rendered);
            assert_eq!(state.frames_dropped, dropped);
            assert_eq!(rendered, delivered + dropped + ring.len() as u64);
            assert!(!s.read_ready());
        }
    }
}

#[test]
fn volume_report_bumps_seq_but_counters_do_not() {
    use crossc_ioctl::audio::{AUDIO_FLAG_AVRCP, AUDIO_FLAG_HW_VOLUME, VolumeState};
    let mut s = Session::new();
    s.set_endpoint(true);
    let seq = s.state().seq;
    let v = VolumeState { level: -100, left: -100, right: -200, avrcp: 120, gets: 1, sets: 0, events: 1 };
    assert!(s.set_volume(AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP, v));
    let st = s.state();
    assert_ne!(st.seq, seq);
    assert_eq!(st.flags, AUDIO_FLAG_ACX | AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP);
    assert_eq!(st.volume, v);
    // Counters only: stored, no bump.
    assert!(!s.set_volume(AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP, VolumeState { gets: 9, ..v }));
    assert_eq!((s.state().seq, s.state().volume.gets), (st.seq, 9));
    // Non-volume bits cannot be smuggled in.
    assert!(s.set_volume(AUDIO_FLAG_LINK | AUDIO_FLAG_HW_VOLUME, v));
    assert_eq!(s.state().flags, AUDIO_FLAG_ACX | AUDIO_FLAG_HW_VOLUME);
    assert!(!s.state().link());
    // The encoded reply keeps the v2 bytes a v2 caller reads.
    let e = s.state().encode();
    assert_eq!(AudioState::decode(&e[..AudioState::SIZE_V2]).unwrap().seq, s.state().seq);
}

#[test]
fn state_replies_fit_v2_and_v3_buffers() {
    use crate::audio_core::write_state;
    use crossc_ioctl::audio::{AUDIO_FLAG_HW_VOLUME, VolumeState};
    let mut s = Session::new();
    s.set_endpoint(true);
    s.set_volume(AUDIO_FLAG_HW_VOLUME, VolumeState { level: -5, left: -5, right: -6, avrcp: 126, ..Default::default() });
    let st = s.state();
    let full = st.encode();
    // A v2 caller: 40 bytes, exactly the v2 reply.
    let mut b40 = [0xAA; 40];
    assert_eq!(write_state(&mut b40, &st), Some(40));
    assert_eq!(b40[..], full[..40]);
    let v2 = AudioState::decode(&b40).unwrap();
    assert_eq!((v2.seq, v2.flags, v2.volume), (st.seq, st.flags, VolumeState::default()));
    // Something in between: what fits.
    let mut b50 = [0xAA; 50];
    assert_eq!(write_state(&mut b50, &st), Some(50));
    assert_eq!(AudioState::decode(&b50).unwrap().volume.level, -5);
    // A v3 caller, and a bigger buffer: the full 64 bytes, nothing more.
    let mut b80 = [0xAA; 80];
    assert_eq!(write_state(&mut b80, &st), Some(AudioState::SIZE));
    assert_eq!(AudioState::decode(&b80[..64]), Ok(st));
    assert_eq!(b80[64], 0xAA);
    // Shorter than v2: refused (STATUS_BUFFER_TOO_SMALL).
    assert_eq!(write_state(&mut [0; 39], &st), None);
}
