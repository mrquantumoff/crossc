//! Byte ring for the PCM tap: index bookkeeping only.
//!
//! The storage is not owned here. The driver allocates it from nonpaged pool
//! and passes it to every call as a slice, under the audio spinlock; the host
//! tests pass a `Vec`. That keeps this file free of kernel calls, of `unsafe`
//! and of allocation, so everything in it runs under `cargo test` on the host
//! (`drivers/crossc-bth/host-tests`).
//!
//! No function here can panic: every slice access goes through `get` /
//! `get_mut`, and a storage slice whose length is not the ring's capacity is
//! refused (the call does nothing). In the driver a panic is a bugcheck.

/// Read position and fill level of a ring of `cap` bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ring {
    cap: usize,
    head: usize,
    len: usize,
}

impl Ring {
    pub const fn new() -> Self {
        Self { cap: 0, head: 0, len: 0 }
    }

    /// Bind to a storage of `cap` bytes (0 = no storage) and empty the ring.
    pub fn set_capacity(&mut self, cap: usize) {
        self.cap = cap;
        self.head = 0;
        self.len = 0;
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn free(&self) -> usize {
        self.cap - self.len
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Drop the oldest `n` bytes (all of them if `n` is larger).
    pub fn discard(&mut self, n: usize) {
        let n = n.min(self.len);
        if n == self.len {
            self.clear();
        } else {
            // n < len <= cap, head < cap: no overflow, and cap != 0 here.
            self.head = (self.head + n) % self.cap;
            self.len -= n;
        }
    }

    /// The two free runs behind the data, in write order: `(start, len)`
    /// of the first, and the length of the second (which starts at 0).
    fn write_runs(&self, n: usize) -> (usize, usize, usize) {
        // len < cap whenever n > 0 fits, so cap != 0.
        let tail = (self.head + self.len) % self.cap;
        let first = n.min(self.cap - tail);
        (tail, first, n - first)
    }

    /// Append `data`. Returns false, changing nothing, if it does not fit or
    /// `storage` is not this ring's storage.
    pub fn push(&mut self, storage: &mut [u8], data: &[u8]) -> bool {
        let n = data.len();
        if storage.len() != self.cap || n > self.free() {
            return false;
        }
        if n == 0 {
            return true;
        }
        let (tail, first, second) = self.write_runs(n);
        let (Some(a), Some(b)) = (data.get(..first), data.get(first..)) else {
            return false;
        };
        let Some(dst) = storage.get_mut(tail..tail + first) else {
            return false;
        };
        dst.copy_from_slice(a);
        let Some(dst) = storage.get_mut(..second) else {
            return false;
        };
        dst.copy_from_slice(b);
        self.len += n;
        true
    }

    /// Append `n` zero bytes. Same contract as [`Ring::push`].
    pub fn push_zeros(&mut self, storage: &mut [u8], n: usize) -> bool {
        if storage.len() != self.cap || n > self.free() {
            return false;
        }
        if n == 0 {
            return true;
        }
        let (tail, first, second) = self.write_runs(n);
        let Some(dst) = storage.get_mut(tail..tail + first) else {
            return false;
        };
        dst.fill(0);
        let Some(dst) = storage.get_mut(..second) else {
            return false;
        };
        dst.fill(0);
        self.len += n;
        true
    }

    /// Move the oldest bytes into `out` (as many as fit) and return how many.
    /// Returns 0, changing nothing, if `storage` is not this ring's storage.
    pub fn pop(&mut self, storage: &[u8], out: &mut [u8]) -> usize {
        let n = out.len().min(self.len);
        if storage.len() != self.cap || n == 0 {
            return 0;
        }
        let first = n.min(self.cap - self.head);
        let second = n - first;
        let (Some(a), Some(b)) = (storage.get(self.head..self.head + first), storage.get(..second)) else {
            return 0;
        };
        let Some(dst) = out.get_mut(..first) else {
            return 0;
        };
        dst.copy_from_slice(a);
        let Some(dst) = out.get_mut(first..n) else {
            return 0;
        };
        dst.copy_from_slice(b);
        self.discard(n);
        n
    }
}
