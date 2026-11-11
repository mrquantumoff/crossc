//! `open_park`: what becomes of an OPEN_CHANNEL whose caller is parked
//! while the BRB is in bthport (crash review F1).
//!
//! Two parts: the decision tables of `open_park`, and a model of one
//! channel slot that is driven through every order of "caller cancelled"
//! (the framework's dequeue and the cancel callback, separately), "close
//! asked for" and "BRB ended" around the steps of the submitter. The model
//! does what `lib.rs` does with the kernel left out (park, check, send,
//! retrieve or take the kept request, decide, close) and makes its
//! decisions with the functions under test.

use std::vec::Vec;

use crate::open_park::{
    OnCancel, OpenDone, Outcome, WaiterStatus, on_cancel, outcome, send_after_park, waiter_kind, waiter_status,
};

const STATUS_CANCELLED: i32 = 0xC000_0120_u32 as i32;
const STATUS_IO_TIMEOUT: i32 = 0xC000_00B5_u32 as i32;

fn all_done() -> Vec<OpenDone> {
    (0u32..16)
        .map(|bits| OpenDone {
            success: bits & 1 != 0,
            have_waiter: bits & 2 != 0,
            close_requested: bits & 4 != 0,
            remote_disconnected: bits & 8 != 0,
        })
        .collect()
}

#[test]
fn an_undisturbed_open_is_handed_to_its_caller() {
    let d = OpenDone { success: true, have_waiter: true, close_requested: false, remote_disconnected: false };
    assert_eq!(outcome(d), Outcome::Reply);
    assert_eq!(waiter_kind(Outcome::Reply), WaiterStatus::Success);
}

#[test]
fn a_failed_open_frees_the_slot_whatever_else_happened() {
    for d in all_done().into_iter().filter(|d| !d.success) {
        assert_eq!(outcome(d), Outcome::Free, "{d:?}");
    }
    assert_eq!(waiter_kind(Outcome::Free), WaiterStatus::Brb);
    assert_eq!(waiter_status(Outcome::Free, STATUS_IO_TIMEOUT), STATUS_IO_TIMEOUT);
}

/// A channel bthport opened is never dropped on the floor: it either goes
/// to the caller or is closed again.
#[test]
fn a_channel_that_opened_is_replied_or_closed_never_just_freed() {
    for d in all_done().into_iter().filter(|d| d.success) {
        assert_ne!(outcome(d), Outcome::Free, "{d:?}");
    }
}

#[test]
fn a_reply_needs_a_waiting_caller_and_no_close_or_disconnect() {
    for d in all_done() {
        let reply = outcome(d) == Outcome::Reply;
        assert_eq!(reply, d.success && d.have_waiter && !d.close_requested && !d.remote_disconnected, "{d:?}");
    }
}

#[test]
fn a_late_channel_reports_cancelled_to_a_caller_that_is_still_there() {
    assert_eq!(waiter_kind(Outcome::CloseLate), WaiterStatus::Cancelled);
    assert_eq!(waiter_status(Outcome::CloseLate, 0), STATUS_CANCELLED);
}

#[test]
fn the_brb_is_not_sent_once_a_close_or_a_cancel_got_in_first() {
    assert!(send_after_park(false, false));
    assert!(!send_after_park(true, false));
    assert!(!send_after_park(false, true));
    assert!(!send_after_park(true, true));
}

/// A cancelled caller is kept for the OPEN's result only while that OPEN
/// is in flight and nobody asked for a close; otherwise it is completed at
/// once, so nothing is held when the park queue is purged on removal.
#[test]
fn a_cancelled_caller_is_kept_only_while_its_open_is_in_flight_and_not_closing() {
    assert_eq!(on_cancel(true, false), OnCancel::Hold);
    assert_eq!(on_cancel(true, true), OnCancel::CompleteNow);
    assert_eq!(on_cancel(false, false), OnCancel::CompleteNow);
    assert_eq!(on_cancel(false, true), OnCancel::CompleteNow);
}

// ---------------------------------------------------------------------------
// Model of one slot
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Opening,
    Open,
    Closing,
    Free,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Brb {
    NotSent,
    InBthport,
    Ended,
}

#[derive(Debug)]
struct Model {
    slot: Slot,
    close_requested: bool,
    /// The caller's request sits in the park queue.
    parked: bool,
    /// The framework has taken the cancelled request out of the queue; its
    /// cancel callback has not run yet.
    dequeued: bool,
    /// The cancel callback kept the request (`open_held`).
    held: bool,
    brb: Brb,
    /// Cancels issued for the BRB while it was in bthport.
    brb_cancels: u32,
    /// How the caller was completed, once per completion.
    completions: Vec<WaiterStatus>,
    close_brbs: u32,
    slot_releases: u32,
}

impl Model {
    /// `ioctl_open` up to and including the claim of the slot.
    fn claimed() -> Self {
        Model {
            slot: Slot::Opening,
            close_requested: false,
            parked: false,
            dequeued: false,
            held: false,
            brb: Brb::NotSent,
            brb_cancels: 0,
            completions: Vec::new(),
            close_brbs: 0,
            slot_releases: 0,
        }
    }

    /// Retrieve from the park queue, else take `open_held` (same lock hold
    /// as the state change that follows in every caller).
    fn take_waiter(&mut self) -> bool {
        core::mem::replace(&mut self.parked, false) || core::mem::replace(&mut self.held, false)
    }

    fn release(&mut self) {
        assert!(!self.held, "slot released with a kept request: {self:?}");
        self.slot = Slot::Free;
        self.close_requested = false;
        self.slot_releases += 1;
    }

    /// `begin_close` for this incarnation.
    fn begin_close(&mut self) {
        match self.slot {
            Slot::Opening => {
                self.close_requested = true;
                if core::mem::replace(&mut self.held, false) {
                    self.completions.push(WaiterStatus::Cancelled);
                }
                if self.brb == Brb::InBthport {
                    self.brb_cancels += 1;
                }
            }
            Slot::Open => {
                self.slot = Slot::Closing;
                self.close_brbs += 1;
            }
            Slot::Closing | Slot::Free => {}
        }
    }

    /// `WdfRequestForwardToIoQueue`.
    fn park(&mut self) {
        self.parked = true;
    }

    /// The submitter after the forward: give up or send.
    fn check_and_send(&mut self) {
        if send_after_park(self.close_requested, self.held) {
            self.brb = Brb::InBthport;
        } else {
            // open_abort: retrieve, then open_failed takes `open_held` as it
            // frees the slot.
            let parked = core::mem::replace(&mut self.parked, false);
            let held = core::mem::replace(&mut self.held, false);
            self.release();
            if parked || held {
                self.completions.push(WaiterStatus::Cancelled);
            }
        }
    }

    /// The framework takes the cancelled request out of the park queue;
    /// only possible while it is parked.
    fn cancel_dequeue(&mut self) {
        if core::mem::replace(&mut self.parked, false) {
            self.dequeued = true;
        }
    }

    /// `evt_open_canceled`, for a request `cancel_dequeue` took out.
    fn cancel_callback(&mut self) {
        if !core::mem::replace(&mut self.dequeued, false) {
            return;
        }
        let in_flight = self.slot == Slot::Opening && !self.held;
        match on_cancel(in_flight, self.close_requested) {
            OnCancel::Hold => {
                self.held = true;
                if self.brb == Brb::InBthport {
                    self.brb_cancels += 1;
                }
            }
            OnCancel::CompleteNow => {
                self.begin_close();
                self.completions.push(WaiterStatus::Cancelled);
            }
        }
    }

    /// `open_complete`; only if the BRB was sent.
    fn brb_ends(&mut self, success: bool) {
        if self.brb != Brb::InBthport {
            return;
        }
        self.brb = Brb::Ended;
        let waiter = self.take_waiter();
        let o = outcome(OpenDone {
            success,
            have_waiter: waiter,
            close_requested: self.close_requested,
            remote_disconnected: false,
        });
        match o {
            Outcome::Reply => self.slot = Slot::Open,
            Outcome::CloseLate => {
                self.slot = Slot::Open;
                self.begin_close();
            }
            Outcome::Free => self.release(),
        }
        if waiter {
            self.completions.push(waiter_kind(o));
        }
    }

    /// bthport completes the CLOSE_CHANNEL.
    fn close_ends(&mut self) {
        if self.slot == Slot::Closing {
            self.release();
        }
    }

    fn apply(&mut self, ev: Ev, success: bool) {
        match ev {
            Ev::Park => self.park(),
            Ev::CheckAndSend => self.check_and_send(),
            Ev::BrbEnds => self.brb_ends(success),
            Ev::Dequeue => self.cancel_dequeue(),
            Ev::Callback => self.cancel_callback(),
            Ev::Close => self.begin_close(),
        }
        // Nothing is ever kept once a close was asked for (removal purges
        // the park queue after D0Exit / flush asked for every close).
        assert!(!(self.held && self.close_requested), "{ev:?}: kept a request with a close asked for: {self:?}");
        assert!(!self.held || self.slot == Slot::Opening, "{ev:?}: kept a request outside Opening: {self:?}");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ev {
    Park,
    CheckAndSend,
    BrbEnds,
    /// The caller cancels: the framework takes the request out of the queue.
    Dequeue,
    /// The queue's cancel callback runs (gets the device lock).
    Callback,
    Close,
}

fn permutations(items: &[Ev]) -> Vec<Vec<Ev>> {
    if items.is_empty() {
        return std::vec![Vec::new()];
    }
    let mut out = Vec::new();
    for i in 0..items.len() {
        let mut rest = items.to_vec();
        let first = rest.remove(i);
        for mut p in permutations(&rest) {
            p.insert(0, first);
            out.push(p);
        }
    }
    out
}

fn pos(t: &[Ev], e: Ev) -> Option<usize> {
    t.iter().position(|&x| x == e)
}

/// Every timeline: the three steps of the submitter in their order, with a
/// cancel (dequeue, then the callback, anywhere after the request was
/// parked) and a close each absent or present anywhere.
fn timelines() -> Vec<Vec<Ev>> {
    let mut out: Vec<Vec<Ev>> = Vec::new();
    for cancel in [false, true] {
        for close in [false, true] {
            let mut items = std::vec![Ev::Park, Ev::CheckAndSend, Ev::BrbEnds];
            if cancel {
                items.extend([Ev::Dequeue, Ev::Callback]);
            }
            if close {
                items.push(Ev::Close);
            }
            for t in permutations(&items) {
                let p = |e| pos(&t, e);
                if !(p(Ev::Park) < p(Ev::CheckAndSend) && p(Ev::CheckAndSend) < p(Ev::BrbEnds)) {
                    continue;
                }
                if cancel && !(p(Ev::Park) < p(Ev::Dequeue) && p(Ev::Dequeue) < p(Ev::Callback)) {
                    continue;
                }
                if !out.contains(&t) {
                    out.push(t);
                }
            }
        }
    }
    out
}

/// The owner closes the channel after it was handed out: an ordinary close.
fn closed_afterwards(t: &[Ev]) -> bool {
    t[pos(t, Ev::BrbEnds).unwrap()..].contains(&Ev::Close)
}

fn run_prefix(t: &[Ev], success: bool) -> Model {
    let mut m = Model::claimed();
    for &ev in t {
        m.apply(ev, success);
    }
    m
}

fn run(t: &[Ev], success: bool) -> Model {
    let mut m = run_prefix(t, success);
    m.close_ends();
    m
}

#[test]
fn the_model_covers_the_plain_cases() {
    let m = run(&[Ev::Park, Ev::CheckAndSend, Ev::BrbEnds], true);
    assert_eq!((m.slot, m.completions.as_slice(), m.close_brbs), (Slot::Open, &[WaiterStatus::Success][..], 0));
    let m = run(&[Ev::Park, Ev::CheckAndSend, Ev::BrbEnds], false);
    assert_eq!((m.slot, m.completions.as_slice(), m.close_brbs), (Slot::Free, &[WaiterStatus::Brb][..], 0));
    // 1 plain + 4 with a close + 6 with a cancel + 36 with both.
    assert_eq!(timelines().len(), 47);
}

#[test]
fn in_every_order_the_caller_is_completed_exactly_once() {
    for t in timelines() {
        for success in [true, false] {
            let m = run(&t, success);
            assert_eq!(m.completions.len(), 1, "{t:?} success={success}: {m:?}");
            assert!(!m.parked && !m.dequeued && !m.held, "{t:?}: a request was left behind: {m:?}");
        }
    }
}

#[test]
fn in_every_order_the_slot_ends_free_or_in_its_callers_hands_and_is_released_at_most_once() {
    for t in timelines() {
        for success in [true, false] {
            let m = run(&t, success);
            let kept = m.completions == [WaiterStatus::Success] && !closed_afterwards(&t);
            assert_eq!(m.slot, if kept { Slot::Open } else { Slot::Free }, "{t:?} success={success}: {m:?}");
            assert_eq!(m.slot_releases, u32::from(!kept), "{t:?} success={success}: {m:?}");
        }
    }
}

/// The verifier's case (crossc-core's timeout): the caller cancels while
/// the BRB is in bthport, which does not honour the cancel, and the OPEN
/// succeeds late. The caller gets the channel, as when its own request
/// carried the BRB; nothing is closed behind its back.
#[test]
fn a_caller_cancelled_while_the_brb_is_in_bthport_still_gets_a_late_success() {
    let t = [Ev::Park, Ev::CheckAndSend, Ev::Dequeue, Ev::Callback, Ev::BrbEnds];
    // Kept, not completed, until the BRB ends; the BRB was asked to cancel.
    let m = run_prefix(&t[..4], true);
    assert!(m.completions.is_empty() && m.held && m.brb_cancels == 1, "{m:?}");
    let m = run(&t, true);
    assert_eq!((m.slot, m.completions.as_slice(), m.close_brbs), (Slot::Open, &[WaiterStatus::Success][..], 0));
    // bthport honoured the cancel (or the OPEN failed): the BRB's status.
    let m = run(&t, false);
    assert_eq!((m.slot, m.completions.as_slice(), m.close_brbs), (Slot::Free, &[WaiterStatus::Brb][..], 0));
}

/// In general: whenever the cancel callback runs while the BRB is in
/// bthport and no close comes before the BRB ends, the caller gets the
/// OPEN's own result.
#[test]
fn a_cancel_alone_never_changes_the_result_of_an_open_that_was_sent() {
    let mut checked = 0;
    for t in timelines() {
        let (Some(cb), Some(send)) = (pos(&t, Ev::Callback), pos(&t, Ev::CheckAndSend)) else { continue };
        let brb = pos(&t, Ev::BrbEnds).unwrap();
        if !(send < cb && cb < brb) || t[..brb].contains(&Ev::Close) {
            continue;
        }
        let m = run(&t, true);
        assert_eq!(m.completions, [WaiterStatus::Success], "{t:?}: {m:?}");
        let m = run(&t, false);
        assert_eq!(m.completions, [WaiterStatus::Brb], "{t:?}: {m:?}");
        checked += 1;
    }
    assert!(checked > 0);
}

/// A close of a slot whose cancelled caller is kept completes that caller
/// at the close, not when bthport gets round to ending the OPEN, and the
/// channel, if it still opens, is closed again.
#[test]
fn a_close_completes_a_kept_caller_at_once_and_its_late_channel_is_closed() {
    let mut checked = 0;
    for t in timelines() {
        let (Some(cb), Some(close)) = (pos(&t, Ev::Callback), pos(&t, Ev::Close)) else { continue };
        let send = pos(&t, Ev::CheckAndSend).unwrap();
        let brb = pos(&t, Ev::BrbEnds).unwrap();
        if !(send < cb && cb < close && close < brb) {
            continue;
        }
        let m = run_prefix(&t[..=close], true);
        assert_eq!(m.completions, [WaiterStatus::Cancelled], "{t:?}: {m:?}");
        let m = run(&t, true);
        assert_eq!((m.slot, m.close_brbs), (Slot::Free, 1), "{t:?}: {m:?}");
        checked += 1;
    }
    assert!(checked > 0);
}

/// The window left on purpose: the request is out of the queue but its
/// callback has not got the lock when the BRB ends. The caller is
/// cancelled and the channel closed again; nobody waits.
#[test]
fn a_cancel_whose_callback_comes_after_the_brb_ended_gets_cancelled() {
    let t = [Ev::Park, Ev::CheckAndSend, Ev::Dequeue, Ev::BrbEnds, Ev::Callback];
    let m = run(&t, true);
    assert_eq!((m.slot, m.completions.as_slice(), m.close_brbs), (Slot::Free, &[WaiterStatus::Cancelled][..], 1));
    let m = run(&t, false);
    assert_eq!((m.slot, m.completions.as_slice(), m.close_brbs), (Slot::Free, &[WaiterStatus::Cancelled][..], 0));
}

#[test]
fn a_channel_that_opened_gets_exactly_one_close_unless_its_caller_keeps_it() {
    for t in timelines() {
        let m = run(&t, true);
        let opened = m.brb == Brb::Ended;
        let kept = m.completions == [WaiterStatus::Success] && !closed_afterwards(&t);
        assert_eq!(m.close_brbs, u32::from(opened && !kept), "{t:?}: {m:?}");
        // A failed or never-sent OPEN leaves nothing to close.
        let m = run(&t, false);
        assert_eq!(m.close_brbs, 0, "{t:?}: {m:?}");
    }
}

#[test]
fn a_close_or_cancel_before_the_send_keeps_the_brb_from_bthport() {
    for t in timelines() {
        let send_at = pos(&t, Ev::CheckAndSend).unwrap();
        let disturbed = t[..send_at].iter().any(|e| matches!(e, Ev::Callback | Ev::Close));
        let m = run(&t, true);
        assert_eq!(m.brb == Brb::NotSent, disturbed, "{t:?}: {m:?}");
    }
}

#[test]
fn a_close_or_cancel_while_the_brb_is_in_bthport_cancels_it() {
    for t in timelines() {
        let send_at = pos(&t, Ev::CheckAndSend).unwrap();
        let brb_at = pos(&t, Ev::BrbEnds).unwrap();
        if t[..send_at].iter().any(|e| matches!(e, Ev::Callback | Ev::Close)) {
            continue;
        }
        let between = t[send_at..brb_at].iter().filter(|e| matches!(e, Ev::Callback | Ev::Close)).count() as u32;
        let m = run(&t, true);
        assert_eq!(m.brb_cancels, between, "{t:?}: {m:?}");
    }
}
