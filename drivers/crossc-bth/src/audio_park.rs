//! Keeping a queue of parked requests in line with the session.
//!
//! A WAIT_EVENT or READ_PCM that pends sits in a queue of its kind
//! (`audio.rs`) and is named by its ticket (`audio_core.rs`). At most one
//! ticket per kind is the parked one; a request with any other ticket is a
//! leftover of an ended session. [`service`] is the only code that takes
//! such requests out of their queue, and it is written against [`Parking`]
//! instead of the WDF calls so that the host tests can run this very loop
//! with a simulated queue and replay the interleavings that matter
//! (`host-tests/src/park_tests.rs`).
//!
//! No kernel calls, no `unsafe`, no allocation, no panics.
//!
//! # Why a request is checked again after it was put back
//! While one thread holds the parked request (it took it out and has not put
//! it back yet), every other thread sees an empty queue. A state change that
//! should complete the request, or a detach that should cancel it, finds
//! nothing and gives up. So whoever puts the request back asks afterwards,
//! under the session lock, whether it may still rest
//! ([`Parking::rests`]: it is the parked one and what it waits for has not
//! happened), and goes through the queue again if not. Any change is either
//! seen by that check, or made after it and then finds the request in the
//! queue (or held by a thread that will do the same check later).

use crate::audio_core::Ticket;

/// Most requests taken out of the queue in one round. The queue holds the
/// one parked request plus, rarely, a leftover, so this is never reached; it
/// only bounds the loop.
pub const MAX_SWEEP: u32 = 16;

/// Most rounds of one [`service`] call. A further round is needed only when
/// something changed while the round before held the parked request; the
/// bound keeps a timer DPC from spinning if rounds keep ending that way.
pub const MAX_ROUNDS: u32 = 4;

/// What to do with a request that was taken out of its queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict<R> {
    /// Complete it with this reply.
    Complete(R),
    /// It is the parked request and has nothing to report yet: put it back.
    Keep,
}

/// One queue of parked requests and the session it belongs to. No method is
/// called with the session lock held; the implementation takes it where it
/// says so.
pub trait Parking {
    /// A request the caller owns (taken out of the queue).
    type Request;
    /// How a request is completed.
    type Reply;

    /// Take the request at the head of the queue, with its ticket.
    fn retrieve(&mut self) -> Option<(Self::Request, Ticket)>;
    /// Under the lock: decide about a request that was taken out, and write
    /// its reply if it is to be completed. A ticket that is not the parked
    /// one must give `Complete` (with the reply for a cancelled request).
    fn settle(&mut self, request: &Self::Request, ticket: Ticket) -> Verdict<Self::Reply>;
    /// Complete a request.
    fn complete(&mut self, request: Self::Request, reply: Self::Reply);
    /// Complete a request as cancelled.
    fn cancel(&mut self, request: Self::Request);
    /// Put a request back at the head of the queue. From then on it is the
    /// queue's (it may be cancelled at once). Returns false if that failed;
    /// the request has then been failed and forgotten.
    fn requeue(&mut self, request: Self::Request, ticket: Ticket) -> bool;
    /// Under the lock: the request `ticket` may stay in the queue (it is the
    /// parked one and what it waits for has not happened).
    fn rests(&mut self, ticket: Ticket) -> bool;
}

/// Bring the queue in line with the session: every leftover in it is
/// cancelled, the parked request is completed if what it waits for has
/// happened and stays parked otherwise.
///
/// The whole queue is looked at, not only its head: a leftover can sit
/// behind the parked request (it was forwarded late). The parked request is
/// held meanwhile and put back at the end; see the module docs for the check
/// that follows.
pub fn service<P: Parking>(port: &mut P) {
    for _ in 0..MAX_ROUNDS {
        let mut kept: Option<(P::Request, Ticket)> = None;
        for _ in 0..MAX_SWEEP {
            let Some((request, ticket)) = port.retrieve() else {
                break;
            };
            match port.settle(&request, ticket) {
                Verdict::Complete(reply) => port.complete(request, reply),
                Verdict::Keep => {
                    // Only one ticket is parked at a time: if one is held
                    // already, that one has become a leftover since.
                    if let Some((old, _)) = kept.replace((request, ticket)) {
                        port.cancel(old);
                    }
                }
            }
        }
        let Some((request, ticket)) = kept else {
            return;
        };
        if !port.requeue(request, ticket) || port.rests(ticket) {
            return;
        }
    }
}

/// The request `ticket` has just been put into the queue for the first time.
/// Between the decision to park it and now, its session may have ended (the
/// detach found the queue still empty) or what it waits for may have
/// happened (the kick found the queue still empty).
pub fn parked<P: Parking>(port: &mut P, ticket: Ticket) {
    if !port.rests(ticket) {
        service(port);
    }
}
