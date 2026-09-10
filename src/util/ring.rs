//! A ring buffer of lines built for many readers and one hot writer.
//!
//! # Why not just a `Mutex<VecDeque<String>>`
//!
//! The writer here is a process pumping stdout as fast as the OS delivers it,
//! while the readers are UI-ish snapshots that only refresh now and then.
//! Taking a lock per line would put the child's output path behind whatever a
//! reader is doing, so writes instead go into a lock free
//! [`ThingBuf`] queue and are folded into the shared
//! [`VecDeque`] later, by whoever reads next.
//!
//! ```text
//!   write_line ──> ThingBuf queue ──drain on read──> RwLock<VecDeque> ──clone──> reader storage
//!    (no lock)      (bounded, drops oldest)            (shared truth)            (private copy)
//! ```
//!
//! # Allocation reuse
//!
//! Both the queue slots and the deque entries are recycled `String`s: pushing
//! a line clears an existing buffer and writes into it instead of allocating a
//! new one. That is why the write path takes `impl AsRef<str>` and copies,
//! rather than taking ownership of a `String`.
//!
//! # Bounds
//!
//! Everything is bounded and lossy by design — this is a tail, not a
//! transcript. The deque keeps the last `capacity` lines; the queue is allowed
//! to run `EXTRA_CAPACITY_THRESHOLD_LIMIT` lines ahead of it, and a writer that
//! finds it too full drops the oldest entries rather than blocking.

use std::{
    collections::{VecDeque, vec_deque::Iter},
    num::NonZeroUsize,
};

use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use thingbuf::ThingBuf;

/// How many pending lines a writer drops at once when the queue is congested.
const EXTRA_CAPACITY_CONSUME: usize = 4;
/// How far past `capacity` the queue may grow before writers start dropping.
const EXTRA_CAPACITY_THRESHOLD: usize = 64;
/// Hard slack of the queue over `capacity`, and the cap on how many lines a
/// single drain folds into the deque.
const EXTRA_CAPACITY_THRESHOLD_LIMIT: usize = 256;

/// The lines themselves, plus a cursor saying how many were ever written.
///
/// The same type is used for the shared buffer inside [`RingStr`] and for each
/// reader's private copy; syncing a reader is then just a matter of comparing
/// the two `offset`s and copying the difference.
pub struct RingStrStorage {
    buf: VecDeque<String>,
    /// Total lines ever pushed, not the current length. Wraps around, which in
    /// practice never happens for a `u128`.
    offset: u128,
}

/// Iterator over the lines of a storage, oldest first.
pub type RingStrLines<'a> = Iter<'a, String>;

impl RingStrStorage {
    /// Create an empty storage holding at most `capacity` lines.
    fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity),
            offset: 0,
        }
    }

    /// Push a line and count it, evicting the oldest one if full.
    fn push(&mut self, v: &String) {
        self.raw_push(v);
        self.offset = self.offset.wrapping_add(1);
    }

    /// Push a line without touching `offset`.
    ///
    /// Used while syncing, where the destination adopts the source's offset in
    /// one go instead of counting each copied line. Reuses the evicted
    /// `String`'s allocation once the deque is full.
    fn raw_push(&mut self, v: impl AsRef<str>) {
        if self.buf.capacity() == 0 {
            return;
        }
        if self.buf.len() >= self.buf.capacity() {
            let mut item = self
                .buf
                .pop_front()
                .expect("Could not pop from VecDeque even though the check above allows it");
            item.clear();
            item.push_str(v.as_ref());
            self.buf.push_back(item);
        } else {
            self.buf.push_back(v.as_ref().into());
        }
    }

    /// Push many lines without touching `offset`.
    fn raw_extend<I>(&mut self, other: I)
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        for i in other {
            self.raw_push(i);
        }
    }

    /// Catch this storage up with `other`.
    ///
    /// Copies only the lines written since the last sync, computed from the
    /// offset difference. If this storage fell behind by more than its own
    /// capacity — or by more than `other` still holds — everything it missed is
    /// gone anyway, so `other` is copied wholesale.
    fn sync_data(&mut self, other: &RingStrStorage) {
        if self.offset >= other.offset {
            return;
        }

        let other_len = other.buf.len() as u128;
        let n = other.offset - self.offset;
        let cap = self.buf.capacity() as u128;
        if n >= cap || n > other_len {
            self.raw_extend(other.buf.iter());
            self.offset = other.offset;
            return;
        }

        let skip = other.buf.len() as u128 - n;
        self.raw_extend(other.buf.iter().skip(skip as usize));
        self.offset = other.offset;
    }

    /// Iterate over the stored lines, oldest first.
    pub fn lines(&self) -> RingStrLines<'_> {
        self.buf.iter()
    }
}

impl Clone for RingStrStorage {
    /// Clone preserving the *capacity*, not just the contents.
    ///
    /// The derived clone would size the new deque to the current length, and a
    /// reader's copy has to keep the same eviction bound as the original for
    /// [`sync_data`](RingStrStorage::sync_data) to stay correct.
    fn clone(&self) -> Self {
        let mut buf: VecDeque<String> = VecDeque::with_capacity(self.buf.capacity());
        for i in self.buf.iter() {
            buf.push_back(i.clone());
        }
        Self {
            buf,
            offset: self.offset,
        }
    }
}

/// A shared, bounded ring of lines with a non blocking write path.
///
/// See the [module docs](self) for the overall design.
pub struct RingStr {
    capacity: NonZeroUsize,
    /// The shared truth, only updated while draining the queue.
    buf: RwLock<RingStrStorage>,
    /// Staging area written by producers and drained by readers.
    queue: ThingBuf<String>,
}

impl RingStr {
    /// Create a ring retaining `capacity` lines.
    ///
    /// # Panics
    ///
    /// If `capacity` is zero.
    pub fn new(capacity: usize) -> Self {
        let buf = RingStrStorage::new(capacity);
        Self {
            capacity: NonZeroUsize::new(capacity).unwrap(),
            buf: RwLock::new(buf),
            queue: ThingBuf::new(capacity + EXTRA_CAPACITY_THRESHOLD_LIMIT),
        }
    }

    /// Take a fresh, fully synced private copy of the ring.
    pub fn create_storage(&self) -> RingStrStorage {
        let buf = self.sync_lock();
        buf.clone()
    }

    /// Catch an existing copy up with the ring.
    pub fn update_storage(&self, storage: &mut RingStrStorage) {
        let buf = self.sync_lock();
        storage.sync_data(&buf);
    }

    /// Fold pending writes into the shared buffer and return a read guard.
    ///
    /// This is the only place the queue is drained, so reading is what makes
    /// the ring advance. The write lock is taken first and then *downgraded*,
    /// which hands the caller a consistent view without a gap where another
    /// writer could slip in. At most `capacity + EXTRA_CAPACITY_THRESHOLD_LIMIT`
    /// lines are folded per call, bounding the time spent holding the lock.
    fn sync_lock(&self) -> RwLockReadGuard<'_, RingStrStorage> {
        let mut buf = self.buf.write();

        let mut limit = self.capacity.get() + EXTRA_CAPACITY_THRESHOLD_LIMIT;
        while let Some(line) = self.queue.pop_ref() {
            buf.push(&line);
            limit -= 1;
            if limit == 0 {
                break;
            }
        }
        RwLockWriteGuard::downgrade(buf)
    }

    /// Write a new line
    ///
    /// Never blocks and never fails: if nobody has read in a long time and the
    /// queue is congested, the oldest pending lines are dropped instead.
    pub fn write_line(&self, line: impl AsRef<str>) {
        self.use_line(|s| s.push_str(line.as_ref()));
    }

    /// Write a new line
    ///
    /// Fills a recycled slot in place through `writer`, avoiding an allocation
    /// per line. Makes room first when the queue has run more than
    /// `EXTRA_CAPACITY_THRESHOLD` lines ahead of the ring capacity.
    fn use_line(&self, writer: impl FnOnce(&mut String)) {
        let len = self.queue.len();
        let capacity = self.capacity.get();
        if len >= capacity + EXTRA_CAPACITY_THRESHOLD {
            for _ in 0..EXTRA_CAPACITY_CONSUME {
                if self.queue.len() > capacity {
                    self.queue.pop_ref();
                }
            }
        }
        if let Ok(mut line) = self.queue.push_ref() {
            line.clear();
            writer(&mut line);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn log() {
        let ring = RingStr::new(3);
        let mut storage = ring.create_storage();
        ring.write_line("oi");
        ring.write_line("tudo");
        ring.write_line("bem");
        ring.write_line("oi");
        ring.write_line("tudo");
        ring.write_line("bem");
        assert_ring(&storage, &[]);
        ring.update_storage(&mut storage);
        assert_ring(&storage, &["oi", "tudo", "bem"]);
    }

    fn assert_ring(storage: &RingStrStorage, expected: &[&str]) {
        let values: Vec<String> = storage.lines().cloned().collect();
        assert_eq!(&values, expected);
    }
}
