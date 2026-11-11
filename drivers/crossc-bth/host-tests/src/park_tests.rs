//! `audio_park::service`, the loop the driver runs over its queues of parked
//! WAIT_EVENT / READ_PCM requests, against a simulated queue.
//!
//! The simulation follows what `audio.rs` does around the loop (decide under
//! the lock, forward, check; change the state, kick; detach, sweep; the
//! cancel callback). Other threads are simulated by *cuts*: on entry to a
//! chosen call the loop makes into its port, another thread's whole action is
//! run, including that thread's own pass over the queue. That is exactly the
//! situation the loop has to survive: somebody else looks at the queue while
//! the parked request is held outside it.

use std::collections::VecDeque;
use std::vec;
use std::vec::Vec;

use crossc_ioctl::audio::{PcmFormat, PcmHeader};

use crate::audio_core::{ReadDone, Session, Ticket, WaitDone};
use crate::audio_park::{MAX_ROUNDS, MAX_SWEEP, Parking, Verdict, parked, service};
use crate::ring_tests::Rng;

const F16: PcmFormat = PcmFormat { sample_rate: 48_000, channels: 2, container_bits: 16, valid_bits: 16 };
const RING_BYTES: usize = 64;
/// The handle that attaches (always the same one: the hardest case for
/// telling a leftover from the parked request).
const A: usize = 0x1000;
const S1: usize = 0x10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Wait,
    Read,
}
const KINDS: [Kind; 2] = [Kind::Wait, Kind::Read];

/// How a request ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Reply,
    Cancelled,
}

/// A call the loop makes into its port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum At {
    Retrieve,
    Settle,
    Complete,
    Cancel,
    Requeue,
    Rests,
}

/// Where another thread cuts in: on entry to...
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    /// ...the n-th port call of the current top-level action (0-based).
    Call(u32),
    /// ...the n-th call of this kind (0-based).
    On(At, u32),
}

/// What a thread does, from start to end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Do {
    /// What the parked request waits for happens (a state change / rendered
    /// frames), followed by the kick the driver gives after it.
    Wake,
    /// READ_PCM only: the waiting frames are gone again (the stream gave its
    /// ring storage up and took new storage).
    Drain,
    /// The session ends, as in `end_session`: detach, then sweep the queue.
    Detach,
    /// ATTACH (refused while attached).
    Attach,
    /// A request arrives and is told to park; it is not in the queue yet.
    Begin,
    /// The oldest such request reaches the queue, then the check after it.
    Forward,
    /// A request is cancelled: EvtIoCanceledOnQueue for the one at the head
    /// of the queue, or, if the queue is empty, a cancel of the one a thread
    /// holds (it takes effect when that thread puts it back).
    Cancel,
    /// A pass over the queue for no reason of its own (what the driver does
    /// after a detach, or after forwarding some other request).
    Sweep,
}
const ALL: [Do; 8] = [Do::Wake, Do::Drain, Do::Detach, Do::Attach, Do::Begin, Do::Forward, Do::Cancel, Do::Sweep];

/// A request owned by whoever holds this value (not `Clone`: the loop cannot
/// complete one twice without the simulation noticing).
#[derive(Debug)]
struct Req(Ticket);

struct Sim {
    kind: Kind,
    s: Session,
    storage: Vec<u8>,
    running: bool,
    queue: VecDeque<Ticket>,
    /// Told to park, not forwarded yet.
    limbo: VecDeque<Ticket>,
    /// Out of the queue, in some thread's hands.
    held: Vec<Ticket>,
    /// Of `held`: cancelled meanwhile.
    doomed: Vec<Ticket>,
    issued: Vec<Ticket>,
    ended: Vec<(Ticket, End)>,
    cuts: Vec<(Cut, Vec<Do>)>,
    calls: u32,
    calls_at: [u32; 6],
    requeue_fails: bool,
    /// Random cuts: generator and how many cuts are left for this action.
    rng: Option<Rng>,
    budget: u32,
}

impl Sim {
    /// Endpoint and stream present, ring installed, nobody attached.
    fn new(kind: Kind) -> Self {
        let mut s = Session::new();
        assert!(s.set_endpoint(true));
        assert!(s.stream_created(S1, F16));
        assert!(s.offer_ring(S1, 0, RING_BYTES));
        Self {
            kind,
            s,
            storage: vec![0u8; RING_BYTES],
            running: false,
            queue: VecDeque::new(),
            limbo: VecDeque::new(),
            held: Vec::new(),
            doomed: Vec::new(),
            issued: Vec::new(),
            ended: Vec::new(),
            cuts: Vec::new(),
            calls: 0,
            calls_at: [0; 6],
            requeue_fails: false,
            rng: None,
            budget: 0,
        }
    }

    /// Attached, one request parked and in the queue.
    fn with_parked(kind: Kind) -> (Self, Ticket) {
        let mut sim = Self::new(kind);
        sim.act(Do::Attach);
        let t = sim.begin().expect("parks");
        sim.act(Do::Forward);
        assert_eq!(sim.queue, [t]);
        (sim, t)
    }

    fn parked_ticket(&self) -> Ticket {
        match self.kind {
            Kind::Wait => self.s.wait_ticket(),
            Kind::Read => self.s.read_ticket(),
        }
    }

    fn ready(&self) -> bool {
        match self.kind {
            Kind::Wait => self.s.wait_ready(),
            Kind::Read => self.s.read_ready(),
        }
    }

    fn may_rest(&self, t: Ticket) -> bool {
        match self.kind {
            Kind::Wait => self.s.wait_rests(t),
            Kind::Read => self.s.read_rests(t),
        }
    }

    fn gone(&mut self, t: Ticket) {
        match self.kind {
            Kind::Wait => self.s.wait_gone(t),
            Kind::Read => self.s.read_gone(t),
        }
    }

    /// The IOCTL handler up to the decision: `Some(ticket)` if the request
    /// is to be parked.
    fn begin(&mut self) -> Option<Ticket> {
        let t = match self.kind {
            Kind::Wait => match self.s.begin_wait(A, self.s.state().seq) {
                Ok(None) => self.s.wait_ticket(),
                Ok(Some(_)) => panic!("seq is current, cannot complete at once"),
                Err(_) => return None,
            },
            Kind::Read => match self.s.begin_read(A, &self.storage, &mut [0u8; PcmHeader::SIZE + 64]) {
                Ok(0) => self.s.read_ticket(),
                Ok(_) | Err(_) => return None,
            },
        };
        assert!(t != 0 && !self.issued.contains(&t), "ticket {t} is new");
        self.issued.push(t);
        self.limbo.push_back(t);
        Some(t)
    }

    /// The request `t` reaches its queue; then the check `audio::park` makes.
    fn forward(&mut self, t: Ticket) {
        let i = self.limbo.iter().position(|&x| x == t).expect("in limbo");
        self.limbo.remove(i);
        self.queue.push_back(t);
        parked(self, t);
    }

    /// EvtIoCanceledOnQueue for the request `t`, which has left the queue.
    fn canceled_on_queue(&mut self, t: Ticket) {
        self.gone(t);
        self.ended.push((t, End::Cancelled));
    }

    fn act(&mut self, what: Do) {
        match what {
            Do::Wake => {
                match self.kind {
                    Kind::Wait => {
                        self.running = !self.running;
                        assert!(self.s.stream_running(S1, self.running));
                    }
                    Kind::Read => self.s.render(S1, &mut self.storage, &[0x55; 8]),
                }
                if self.ready() {
                    service(self);
                }
            }
            Do::Drain => {
                self.s.remove_ring();
                assert!(self.s.offer_ring(S1, 0, RING_BYTES));
            }
            Do::Detach => {
                if self.s.detach(0).is_some() {
                    service(self);
                }
            }
            Do::Attach => {
                let _ = self.s.attach(A);
            }
            Do::Begin => {
                let _ = self.begin();
            }
            Do::Forward => {
                if let Some(&t) = self.limbo.front() {
                    self.forward(t);
                }
            }
            Do::Cancel => {
                if let Some(t) = self.queue.pop_front() {
                    self.canceled_on_queue(t);
                } else if let Some(&t) = self.held.first()
                    && !self.doomed.contains(&t)
                {
                    self.doomed.push(t);
                }
            }
            Do::Sweep => service(self),
        }
    }

    /// A top-level action: the cuts given for it are counted from here.
    fn top(&mut self, what: Do, cuts: &[(Cut, &[Do])]) {
        self.calls = 0;
        self.calls_at = [0; 6];
        self.cuts = cuts.iter().map(|(c, d)| (*c, d.to_vec())).collect();
        self.act(what);
        self.cuts.clear();
    }

    /// Entry of a port call: let the other threads in.
    fn enter(&mut self, at: At) {
        let (n, n_at) = (self.calls, self.calls_at[at as usize]);
        self.calls += 1;
        self.calls_at[at as usize] += 1;
        while let Some(i) = self.cuts.iter().position(|(c, _)| *c == Cut::Call(n) || *c == Cut::On(at, n_at)) {
            let (_, actions) = self.cuts.remove(i);
            for a in actions {
                self.act(a);
            }
        }
        if self.budget > 0 {
            let Some(rng) = self.rng.as_mut() else { return };
            if rng.below(3) == 0 {
                let what = ALL[rng.below(ALL.len())];
                self.budget -= 1;
                self.act(what);
            }
        }
    }

    fn release(&mut self, t: Ticket) {
        let i = self.held.iter().position(|&x| x == t).expect("request is held");
        self.held.remove(i);
    }

    /// Nothing is in flight: the queue must agree with the session.
    fn check(&self) {
        assert!(self.held.is_empty(), "requests still held: {:?}", self.held);
        let parked = self.parked_ticket();
        for &t in &self.queue {
            assert_eq!(t, parked, "a leftover stayed in the queue (queue {:?})", self.queue);
            assert!(self.may_rest(t), "request {t} can be completed but stayed in the queue");
        }
        assert!(self.queue.len() <= 1, "queue {:?}", self.queue);
        if parked != 0 {
            assert!(self.queue.contains(&parked) || self.limbo.contains(&parked), "the parked request {parked} is nowhere");
        }
        for &t in &self.issued {
            let ends = self.ended.iter().filter(|e| e.0 == t).count();
            let waits = usize::from(self.queue.contains(&t)) + usize::from(self.limbo.contains(&t));
            assert_eq!(ends + waits, 1, "request {t}: ended {ends} time(s), waiting in {waits} place(s)");
        }
    }

    /// Let every request that is on its way reach the queue, then check.
    fn settle_and_check(&mut self) {
        while let Some(&t) = self.limbo.front() {
            self.forward(t);
        }
        self.check();
    }
}

impl Parking for Sim {
    type Request = Req;
    type Reply = End;

    fn retrieve(&mut self) -> Option<(Req, Ticket)> {
        self.enter(At::Retrieve);
        let t = self.queue.pop_front()?;
        self.held.push(t);
        Some((Req(t), t))
    }

    fn settle(&mut self, request: &Req, ticket: Ticket) -> Verdict<End> {
        self.enter(At::Settle);
        assert_eq!(request.0, ticket);
        assert!(self.held.contains(&ticket));
        match self.kind {
            Kind::Wait => match self.s.finish_wait(ticket) {
                WaitDone::Stale => Verdict::Complete(End::Cancelled),
                WaitDone::NotReady => Verdict::Keep,
                WaitDone::Done(_) => Verdict::Complete(End::Reply),
            },
            Kind::Read => match self.s.finish_read(ticket, &self.storage, &mut [0u8; PcmHeader::SIZE + 64]) {
                ReadDone::Stale => Verdict::Complete(End::Cancelled),
                ReadDone::Empty => Verdict::Keep,
                ReadDone::Done(_) => Verdict::Complete(End::Reply),
                ReadDone::TooSmall => panic!("the buffer holds a frame"),
            },
        }
    }

    fn complete(&mut self, request: Req, reply: End) {
        self.enter(At::Complete);
        self.release(request.0);
        self.doomed.retain(|&t| t != request.0);
        self.ended.push((request.0, reply));
    }

    fn cancel(&mut self, request: Req) {
        self.enter(At::Cancel);
        self.release(request.0);
        self.doomed.retain(|&t| t != request.0);
        self.ended.push((request.0, End::Cancelled));
    }

    fn requeue(&mut self, request: Req, ticket: Ticket) -> bool {
        self.enter(At::Requeue);
        assert_eq!(request.0, ticket);
        self.release(ticket);
        if self.requeue_fails {
            // As `audio::requeue`: forget it and fail it.
            self.gone(ticket);
            self.ended.push((ticket, End::Cancelled));
            return false;
        }
        if let Some(i) = self.doomed.iter().position(|&t| t == ticket) {
            // Cancelled while it was held: the queue cancels it right away.
            self.doomed.remove(i);
            self.canceled_on_queue(ticket);
        } else {
            self.queue.push_front(ticket);
        }
        true
    }

    fn rests(&mut self, ticket: Ticket) -> bool {
        self.enter(At::Rests);
        self.may_rest(ticket)
    }
}

// ---- the plain cases -------------------------------------------------------------

#[test]
fn a_parked_request_stays_until_it_is_woken() {
    for kind in KINDS {
        let (mut sim, t) = Sim::with_parked(kind);
        sim.top(Do::Sweep, &[]);
        assert_eq!(sim.queue, [t]);
        assert!(sim.ended.is_empty());
        sim.check();
        sim.top(Do::Wake, &[]);
        assert_eq!(sim.ended, [(t, End::Reply)]);
        assert!(sim.queue.is_empty());
        sim.check();
    }
}

#[test]
fn woken_before_it_reached_the_queue() {
    for kind in KINDS {
        let mut sim = Sim::new(kind);
        sim.act(Do::Attach);
        let t = sim.begin().unwrap();
        // The kick finds an empty queue.
        sim.top(Do::Wake, &[]);
        assert!(sim.ended.is_empty());
        // The check after the forward completes it.
        sim.forward(t);
        assert_eq!(sim.ended, [(t, End::Reply)]);
        sim.check();
    }
}

#[test]
fn detached_before_it_reached_the_queue() {
    for kind in KINDS {
        let mut sim = Sim::new(kind);
        sim.act(Do::Attach);
        let t = sim.begin().unwrap();
        sim.top(Do::Detach, &[]);
        assert!(sim.ended.is_empty());
        sim.forward(t);
        assert_eq!(sim.ended, [(t, End::Cancelled)]);
        sim.check();
    }
}

#[test]
fn detach_cancels_the_parked_request() {
    for kind in KINDS {
        let (mut sim, t) = Sim::with_parked(kind);
        sim.top(Do::Detach, &[]);
        assert_eq!(sim.ended, [(t, End::Cancelled)]);
        sim.check();
    }
}

// ---- review findings ---------------------------------------------------------------

/// A thread holds the parked request (it found it not ready) when the state
/// changes; the kick of that change finds the queue empty. The request must
/// not go back to sleep.
#[test]
fn a_wakeup_while_the_request_is_held_is_not_lost() {
    for kind in KINDS {
        for cut in [Cut::On(At::Retrieve, 1), Cut::On(At::Requeue, 0), Cut::On(At::Rests, 0)] {
            let (mut sim, t) = Sim::with_parked(kind);
            sim.top(Do::Sweep, &[(cut, &[Do::Wake])]);
            assert_eq!(sim.ended, [(t, End::Reply)], "{kind:?} {cut:?}");
            assert!(sim.queue.is_empty(), "{kind:?} {cut:?}");
            sim.check();
        }
    }
}

/// The same with a detach: its sweep finds the queue empty, so the thread
/// that puts the request back has to notice that it is a leftover now.
#[test]
fn a_detach_while_the_request_is_held_is_not_lost() {
    for kind in KINDS {
        for cut in [Cut::On(At::Retrieve, 1), Cut::On(At::Requeue, 0), Cut::On(At::Rests, 0)] {
            let (mut sim, t) = Sim::with_parked(kind);
            sim.top(Do::Sweep, &[(cut, &[Do::Detach])]);
            assert_eq!(sim.ended, [(t, End::Cancelled)], "{kind:?} {cut:?}");
            assert!(sim.queue.is_empty(), "{kind:?} {cut:?}");
            sim.check();
        }
    }
}

/// READ_PCM: the frames a kick saw are gone when the request is looked at
/// (`Empty`), and new frames arrive while it is held.
#[test]
fn a_read_that_found_the_ring_empty_again_still_gets_the_next_frames() {
    let (mut sim, t) = Sim::with_parked(Kind::Read);
    // Frames arrive; on the way to the request they are lost again, and
    // while the request is being put back more arrive.
    sim.top(Do::Wake, &[(Cut::On(At::Retrieve, 0), &[Do::Drain]), (Cut::On(At::Requeue, 0), &[Do::Wake])]);
    assert_eq!(sim.ended, [(t, End::Reply)]);
    sim.check();
}

/// A request of an ended session that reaches the queue *behind* the new
/// session's parked request is cancelled all the same, and the parked one is
/// left alone.
#[test]
fn a_leftover_behind_the_parked_request_is_cancelled() {
    for kind in KINDS {
        let mut sim = Sim::new(kind);
        sim.act(Do::Attach);
        let old = sim.begin().unwrap();
        sim.act(Do::Detach);
        sim.act(Do::Attach);
        let new = sim.begin().unwrap();
        sim.forward(new);
        assert_eq!(sim.queue, [new]);
        sim.forward(old);
        assert_eq!(sim.ended, [(old, End::Cancelled)], "{kind:?}");
        assert_eq!(sim.queue, [new], "{kind:?}");
        sim.check();
        sim.top(Do::Wake, &[]);
        assert_eq!(sim.ended, [(old, End::Cancelled), (new, End::Reply)], "{kind:?}");
        sim.check();
    }
}

/// The handle detaches and attaches again while one of its requests is on
/// its way to the queue. The request is cancelled although its handle is the
/// attached one, and the handle's next request is neither refused nor left
/// behind.
#[test]
fn a_request_parked_across_a_reattach_of_its_handle_is_cancelled() {
    for kind in KINDS {
        let mut sim = Sim::new(kind);
        sim.act(Do::Attach);
        let old = sim.begin().unwrap();
        sim.act(Do::Detach);
        sim.act(Do::Attach);
        sim.forward(old);
        assert_eq!(sim.ended, [(old, End::Cancelled)], "{kind:?}");
        let new = sim.begin().expect("the new session's request is accepted");
        sim.forward(new);
        assert_eq!(sim.queue, [new]);
        sim.top(Do::Wake, &[]);
        assert_eq!(sim.ended, [(old, End::Cancelled), (new, End::Reply)], "{kind:?}");
        sim.check();
        // And the one after it.
        let next = sim.begin().expect("accepted");
        sim.forward(next);
        sim.top(Do::Wake, &[]);
        assert_eq!(sim.ended.last(), Some(&(next, End::Reply)), "{kind:?}");
        sim.check();
    }
}

/// While a thread holds the parked request, the session ends, a new one
/// starts and parks its own request. The thread finds that one too: the one
/// it holds is the leftover.
#[test]
fn the_request_held_becomes_a_leftover_when_a_new_one_is_parked() {
    for kind in KINDS {
        let (mut sim, old) = Sim::with_parked(kind);
        sim.top(Do::Sweep, &[(Cut::On(At::Retrieve, 1), &[Do::Detach, Do::Attach, Do::Begin, Do::Forward])]);
        let new = sim.parked_ticket();
        assert!(new != 0 && new != old);
        assert_eq!(sim.ended, [(old, End::Cancelled)], "{kind:?}");
        assert_eq!(sim.queue, [new], "{kind:?}");
        sim.check();
    }
}

/// A cancel that arrives while a thread holds the request takes effect when
/// the request is put back; the slot is free afterwards.
#[test]
fn a_cancel_while_the_request_is_held() {
    for kind in KINDS {
        let (mut sim, t) = Sim::with_parked(kind);
        sim.top(Do::Sweep, &[(Cut::On(At::Requeue, 0), &[Do::Cancel])]);
        assert_eq!(sim.ended, [(t, End::Cancelled)], "{kind:?}");
        sim.check();
        let next = sim.begin().expect("slot is free");
        sim.forward(next);
        assert_eq!(sim.queue, [next]);
        sim.check();
    }
}

#[test]
fn a_request_that_cannot_be_put_back_is_failed_and_forgotten() {
    for kind in KINDS {
        let (mut sim, t) = Sim::with_parked(kind);
        sim.requeue_fails = true;
        sim.top(Do::Sweep, &[]);
        assert_eq!(sim.ended, [(t, End::Cancelled)], "{kind:?}");
        assert_eq!(sim.parked_ticket(), 0);
        // One round only, and the request is not asked about again.
        assert_eq!(sim.calls_at[At::Retrieve as usize], 2);
        assert_eq!(sim.calls_at[At::Rests as usize], 0);
        sim.check();
    }
}

// ---- bounds ------------------------------------------------------------------------

/// A port that never agrees with the loop.
struct Restless {
    queued: u32,
    endless: bool,
    retrieves: u32,
    completes: u32,
    requeues: u32,
}

impl Parking for Restless {
    type Request = ();
    type Reply = ();
    fn retrieve(&mut self) -> Option<((), Ticket)> {
        self.retrieves += 1;
        if self.endless {
            return Some(((), 7));
        }
        self.queued = self.queued.checked_sub(1)?;
        Some(((), 7))
    }
    fn settle(&mut self, _: &(), _: Ticket) -> Verdict<()> {
        if self.endless { Verdict::Complete(()) } else { Verdict::Keep }
    }
    fn complete(&mut self, _: (), _: ()) {
        self.completes += 1;
    }
    fn cancel(&mut self, _: ()) {
        panic!("one request only");
    }
    fn requeue(&mut self, _: (), _: Ticket) -> bool {
        self.requeues += 1;
        self.queued += 1;
        true
    }
    fn rests(&mut self, _: Ticket) -> bool {
        false
    }
}

#[test]
fn the_loop_is_bounded() {
    // A request that never rests: a fixed number of rounds, and it ends in
    // the queue, not in the loop's hands.
    let mut p = Restless { queued: 1, endless: false, retrieves: 0, completes: 0, requeues: 0 };
    service(&mut p);
    assert_eq!(p.requeues, MAX_ROUNDS);
    assert_eq!(p.retrieves, 2 * MAX_ROUNDS);
    assert_eq!(p.queued, 1);
    // A queue that never runs dry: one round of a fixed length.
    let mut p = Restless { queued: 0, endless: true, retrieves: 0, completes: 0, requeues: 0 };
    service(&mut p);
    assert_eq!((p.retrieves, p.completes, p.requeues), (MAX_SWEEP, MAX_SWEEP, 0));
}

// ---- every interleaving of one and two foreign actions -------------------------------

/// Starting points.
const SETUPS: [&str; 5] = ["parked", "on its way", "leftover on its way", "attached, idle", "detached"];

fn setup(kind: Kind, name: &str) -> Sim {
    let mut sim = Sim::new(kind);
    match name {
        "parked" => return Sim::with_parked(kind).0,
        "on its way" => {
            sim.act(Do::Attach);
            sim.begin().unwrap();
        }
        "leftover on its way" => {
            // The new session's request is parked already.
            sim.act(Do::Attach);
            sim.begin().unwrap();
            sim.act(Do::Detach);
            sim.act(Do::Attach);
            let new = sim.begin().unwrap();
            sim.forward(new);
        }
        "attached, idle" => sim.act(Do::Attach),
        "detached" => {}
        _ => panic!("unknown setup {name}"),
    }
    sim
}

/// Port calls of one action that a cut can be placed on. More than any
/// action without cuts makes.
const CALLS: u32 = 8;

#[test]
fn every_single_and_double_cut_leaves_the_queue_in_line_with_the_session() {
    let mut cases = 0u32;
    for kind in KINDS {
        for name in SETUPS {
            for top in ALL {
                for c1 in 0..CALLS {
                    for d1 in ALL {
                        // One cut, then the same with every second cut.
                        for second in core::iter::once(None).chain((0..CALLS).flat_map(|c| ALL.map(|d| Some((c, d))))) {
                            let mut sim = setup(kind, name);
                            let one = [d1];
                            let two = [second.map_or(Do::Sweep, |s| s.1)];
                            let mut cuts: Vec<(Cut, &[Do])> = vec![(Cut::Call(c1), &one)];
                            if let Some((c2, _)) = second {
                                cuts.push((Cut::Call(c2), &two));
                            }
                            sim.top(top, &cuts);
                            sim.settle_and_check();
                            // Whatever happened, the queue still works.
                            sim.act(Do::Attach);
                            if let Some(t) = sim.begin() {
                                sim.forward(t);
                                sim.top(Do::Wake, &[]);
                                assert!(sim.ended.contains(&(t, End::Reply)), "{kind:?} {name} {top:?} {cuts:?}");
                            }
                            sim.check();
                            cases += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 2 * 5 * 8 * CALLS * 8 * (1 + CALLS * 8));
}

/// Long random runs: every action may be cut into by up to
/// `MAX_ROUNDS - 1` others (each of which runs its own pass over the queue),
/// which is as many disturbances as the loop promises to absorb.
#[test]
fn random_interleavings_leave_the_queue_in_line_with_the_session() {
    for kind in KINDS {
        for seed in 1..=40u64 {
            let mut sim = Sim::new(kind);
            sim.rng = Some(Rng(0x9E37_79B9_7F4A_7C15 ^ (seed << 32) ^ seed));
            for _ in 0..3_000 {
                let what = {
                    let rng = sim.rng.as_mut().unwrap();
                    ALL[rng.below(ALL.len())]
                };
                sim.budget = MAX_ROUNDS - 1;
                sim.top(what, &[]);
                sim.budget = 0;
                sim.check();
            }
            sim.settle_and_check();
            assert!(sim.ended.iter().any(|e| e.1 == End::Reply), "{kind:?} seed {seed}: nothing was ever completed");
            assert!(sim.ended.iter().any(|e| e.1 == End::Cancelled), "{kind:?} seed {seed}: nothing was ever cancelled");
        }
    }
}
