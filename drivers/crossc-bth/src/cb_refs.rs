//! Who still holds the block that bthport's indication callback is called
//! with (`lib.rs::CbBlock`): the device, and every channel registration
//! bthport has not released yet.
//!
//! bthport tells a profile driver how long it may call its callback
//! (Microsoft Learn, INDICATION_CODE): `IndicationAddReference` "indicates
//! to a profile driver to add a reference to its device object because it
//! may be called at any time", `IndicationReleaseReference` "to release a
//! reference to its device object and that it will no longer be called".
//! The block is freed by whoever drops the last of these references, and
//! never on a timeout: as long as bthport has a registration it has not
//! released, the memory its callback reads stays allocated.
//!
//! The count is one atomic word, so it can be used from the callback at
//! DISPATCH_LEVEL and from device cleanup at the same time.
//!
//! One rule comes from the caller: a registration is only added while the
//! device still holds the block. bthport registers a channel while it
//! processes an OPEN_CHANNEL BRB, and the driver sends those only from its
//! I/O queue, which is gone before the device's cleanup callback runs.
//!
//! No kernel calls, no `unsafe`, no allocation, no panics. Tested in
//! `host-tests/src/cb_tests.rs`.

use core::sync::atomic::{AtomicU32, Ordering};

/// Set while the device holds the block.
const DEVICE: u32 = 1 << 31;
/// The registration count never goes past this. Once reached it sticks, so
/// a miscount can only keep the block for ever, never free it early.
const STUCK: u32 = DEVICE - 1;

/// Result of dropping a reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Released {
    /// Somebody else still holds the block.
    Kept,
    /// That was the last reference: the caller frees the block.
    Last,
    /// There was no such reference to drop. Nothing was changed.
    Underflow,
}

/// The reference count of one callback block.
pub struct CbRefs(AtomicU32);

impl CbRefs {
    /// A block held by its device, with no registration.
    pub const fn new() -> Self {
        Self(AtomicU32::new(DEVICE))
    }

    /// bthport registered the callback (`IndicationAddReference`).
    pub fn add(&self) {
        let _ = self.0.try_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            if v & STUCK == STUCK { None } else { Some(v + 1) }
        });
    }

    /// bthport dropped a registration (`IndicationReleaseReference`).
    pub fn release(&self) -> Released {
        let r = self.0.try_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            let n = v & STUCK;
            if n == 0 || n == STUCK { None } else { Some(v - 1) }
        });
        match r {
            Ok(old) if old - 1 == 0 => Released::Last,
            Ok(_) => Released::Kept,
            Err(v) if v & STUCK == 0 => Released::Underflow,
            Err(_) => Released::Kept,
        }
    }

    /// The device is going away (once, from its cleanup callback).
    pub fn release_device(&self) -> Released {
        let r = self.0.try_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            if v & DEVICE == 0 { None } else { Some(v & !DEVICE) }
        });
        match r {
            Ok(old) if old == DEVICE => Released::Last,
            Ok(_) => Released::Kept,
            Err(_) => Released::Underflow,
        }
    }

    /// Registrations bthport has not released (for the log).
    pub fn registrations(&self) -> u32 {
        self.0.load(Ordering::SeqCst) & STUCK
    }
}

impl Default for CbRefs {
    fn default() -> Self {
        Self::new()
    }
}
