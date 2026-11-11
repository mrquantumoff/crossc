//! The decisions of the OPEN_CHANNEL path that do not need the kernel, so
//! that the host tests can go through every combination and every order of
//! events (`host-tests/src/open_tests.rs`). `lib.rs` makes them under the
//! device lock and then acts on them.
//!
//! No kernel calls, no `unsafe`, no allocation, no panics.
//!
//! # The arrangement (crash review F1)
//! The caller's OPEN_CHANNEL request is parked in a manual queue that is not
//! power-managed, one per channel slot, and the BRB is sent on a request of
//! the driver's own (`lib.rs::ioctl_open`). The framework waits, without a
//! timeout, for every request of a power-managed queue that the driver
//! still owns before the device may leave D0 or be removed
//! (`FxIoQueue::StopProcessingForPower`; Driver Verifier breaks into the
//! debugger after 60 s), and bthport does not cancel an OPEN it is working
//! on. `EvtIoStop` cannot get around that wait: the framework skips the
//! callback for a request that was cancelled earlier, and an
//! acknowledgement does not end the wait of a purge. With the request
//! parked, nothing of an OPEN is in the power-managed queue.
//!
//! # Who completes the caller
//! Whoever takes the request out of the park queue, or out of the slot's
//! `open_held` after a cancel, and nobody else:
//! * the completion routine of the BRB (or the path that gives the OPEN up
//!   before it is sent) retrieves it from the queue, or takes it from
//!   `open_held`;
//! * the queue's cancel callback gets it when the caller cancels, its
//!   handle is closed, or the device is removed. While the OPEN is still in
//!   flight and no close was asked for, it does NOT complete it: it keeps it
//!   in `open_held` and only cancels the BRB ([`on_cancel`]). The caller
//!   then gets what bthport makes of the OPEN, a late success included,
//!   exactly as when its own request carried the BRB (before crash review
//!   F1): crossc-core cancels an OPEN on its timeout and then waits for the
//!   result ("it may have completed with data anyway"), and must not be
//!   able to start a retry OPEN while the old one is still in bthport;
//! * whoever asks for a close of an `Opening` slot (`begin_close`: CLOSE
//!   ioctl, handle closed, D0Exit, removal, remote disconnect) completes a
//!   request kept in `open_held` with STATUS_CANCELLED at once, because the
//!   channel would be closed again anyway. So nothing is held once D0Exit or
//!   SelfManagedIoFlush has run, both of which come before the framework
//!   purges the (not power-managed) park queues on removal and waits for
//!   the requests the driver owns there (`PnpEventRemovedCommonCode`,
//!   `FxIoStopProcessingForPowerPurgeNonManaged`).
//!
//! A slot has one OPEN at a time and stays `Opening` until that OPEN's BRB
//! has ended, and the retrieve (and the take of `open_held`) comes before,
//! or in the same lock hold as, the release of the slot, so the request
//! found in a slot's queue or `open_held` is always the one of the current
//! OPEN.
//!
//! One window is left on purpose: the framework has taken a cancelled
//! request out of the queue but its cancel callback has not got the device
//! lock yet when the BRB ends. The completion routine then finds no caller
//! (`have_waiter` false), a channel that opened is closed again, and the
//! callback, seeing the slot is no longer `Opening`, completes the request
//! with STATUS_CANCELLED. Nothing waits on anything in that case.

/// How an OPEN_CHANNEL BRB ended, and what the slot looked like then (read
/// under the device lock, after the retrieve from the park queue).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenDone {
    /// bthport completed the BRB with a success status: it has a channel.
    pub success: bool,
    /// The caller's request was still parked and has been retrieved.
    pub have_waiter: bool,
    /// A close was asked for while the slot was `Opening` (handle closed,
    /// D0Exit, removal, remote disconnect). A cancel of the caller alone
    /// does not ask for one: the caller is kept for the result.
    pub close_requested: bool,
    /// bthport indicated a remote disconnect for the slot.
    pub remote_disconnected: bool,
}

/// What becomes of the slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The channel is open and its id goes to the caller.
    Reply,
    /// bthport has a channel nobody is going to get: close it again
    /// (CLOSE_CHANNEL), then free the slot.
    CloseLate,
    /// bthport has no channel: free the slot.
    Free,
}

/// What the caller's request, if it is still there, is completed with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaiterStatus {
    /// STATUS_SUCCESS with the reply.
    Success,
    /// STATUS_CANCELLED.
    Cancelled,
    /// The status bthport completed the BRB with (a failure).
    Brb,
}

/// The decision. A channel id is handed out only to a caller that is still
/// waiting, and only if nobody asked for the slot to be closed meanwhile;
/// every other channel bthport opened is closed again, so none is left
/// that the driver does not track.
pub const fn outcome(d: OpenDone) -> Outcome {
    if !d.success {
        Outcome::Free
    } else if d.have_waiter && !d.close_requested && !d.remote_disconnected {
        Outcome::Reply
    } else {
        Outcome::CloseLate
    }
}

/// The completion of the caller for `o`.
pub const fn waiter_kind(o: Outcome) -> WaiterStatus {
    match o {
        Outcome::Reply => WaiterStatus::Success,
        Outcome::CloseLate => WaiterStatus::Cancelled,
        Outcome::Free => WaiterStatus::Brb,
    }
}

/// NTSTATUS of a caller that gets no reply: STATUS_CANCELLED for a channel
/// that is closed again, the BRB's own failure status otherwise.
pub const fn waiter_status(o: Outcome, brb_status: i32) -> i32 {
    /// STATUS_CANCELLED.
    const CANCELLED: i32 = 0xC000_0120_u32 as i32;
    match waiter_kind(o) {
        WaiterStatus::Cancelled => CANCELLED,
        WaiterStatus::Success | WaiterStatus::Brb => brb_status,
    }
}

/// After the caller's request has been parked: is the BRB still to be sent?
/// Not if a close or a cancel got in between claiming the slot and now
/// (`close_requested`, or the cancelled request kept in `open_held`; both
/// read under the lock): then the OPEN is given up without bothering
/// bthport.
pub const fn send_after_park(close_requested: bool, caller_held: bool) -> bool {
    !close_requested && !caller_held
}

/// What the park queue's cancel callback does with the caller's request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnCancel {
    /// Keep it in the slot's `open_held` and cancel only the BRB. The
    /// completion routine, `open_abort` or `begin_close` completes it.
    Hold,
    /// Complete it with STATUS_CANCELLED now (and start a close of the
    /// incarnation it waited for, if that is still there).
    CompleteNow,
}

/// The decision of the cancel callback, made under the device lock.
/// `in_flight`: the slot is still `Opening` for the incarnation the request
/// waited for (and holds no other request). A request is kept only while
/// its OPEN is in flight and nobody has asked for a close: then it is going
/// to get the OPEN's own result, as before the park queue. Otherwise the
/// result is a cancel anyway, and waiting for it would only hold up
/// whoever is waiting for the request (the purge of the queue on removal).
pub const fn on_cancel(in_flight: bool, close_requested: bool) -> OnCancel {
    if in_flight && !close_requested { OnCancel::Hold } else { OnCancel::CompleteNow }
}
