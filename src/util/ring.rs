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
    /// The eviction bound. Kept separately because `VecDeque::with_capacity`
    /// only promises *at least* what was asked for, and every storage derived
    /// from the same ring has to evict at exactly the same point for the
    /// offset arithmetic in [`sync_data`](RingStrStorage::sync_data) to line
    /// up.
    capacity: NonZeroUsize,
    /// Total lines ever pushed, not the current length. Wraps around, which in
    /// practice never happens for a `u128`.
    offset: u128,
}

/// Iterator over the lines of a storage, oldest first.
pub type RingStrLines<'a> = Iter<'a, String>;

impl RingStrStorage {
    /// Create an empty storage holding at most `capacity` lines.
    fn new(capacity: NonZeroUsize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity.get()),
            capacity,
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
        if self.buf.len() >= self.capacity.get() {
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
    /// Both sides must belong to the same [`RingStr`]; see
    /// [`RingStr::update_storage`] for what happens when they do not.
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
        let cap = self.capacity.get() as u128;
        if n >= cap || n > other_len {
            self.raw_extend(other.buf.iter());
            self.offset = other.offset;
            return;
        }

        let skip = other.buf.len() as u128 - n;
        self.raw_extend(other.buf.iter().skip(skip as usize));
        self.offset = other.offset;
    }

    /// Discard this storage's contents and take `other`'s instead.
    ///
    /// Unlike [`sync_data`](RingStrStorage::sync_data) this makes no
    /// assumption that the two share a history: the cursor is overwritten
    /// rather than advanced, so it is the way to move a storage between
    /// rings.
    ///
    /// The storage keeps **its own** eviction bound. The point of reusing a
    /// storage is that its buffer was allocated once and stays allocated, so
    /// the deque is never grown here; if the other ring holds a wider window,
    /// only the newest lines that fit are taken.
    ///
    /// Lines are refilled in place rather than dropped and reallocated. The
    /// exception is a move onto a ring holding fewer lines than this storage
    /// currently does: the surplus `String`s are freed, since a `VecDeque`
    /// has no way to keep them around as spare slots.
    fn reset_from(&mut self, other: &RingStrStorage) {
        // Bounded by our own capacity, so `push_back` below can never grow
        // the deque: it was built with room for `self.capacity` lines.
        let take = other.buf.len().min(self.capacity.get());
        let skip = other.buf.len() - take;

        // Shed any surplus slots. Growing is not possible, only filling.
        while self.buf.len() > take {
            self.buf.pop_front();
        }

        let mut source = other.buf.iter().skip(skip);
        for slot in self.buf.iter_mut() {
            let line = source
                .next()
                .expect("source is at least as long as the slots kept above");
            slot.clear();
            slot.push_str(line);
        }
        for line in source {
            debug_assert!(
                self.buf.len() < self.buf.capacity(),
                "reset_from grew the deque; the allocation was supposed to be permanent"
            );
            self.buf.push_back(line.clone());
        }

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
        let mut buf: VecDeque<String> = VecDeque::with_capacity(self.capacity.get());
        for i in self.buf.iter() {
            buf.push_back(i.clone());
        }
        Self {
            buf,
            capacity: self.capacity,
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
    /// If `capacity` is zero. A ring that retains nothing has no use, and
    /// rejecting it here is what lets every storage assume a non zero
    /// eviction bound instead of checking for one on the write path.
    pub fn new(capacity: usize) -> Self {
        let capacity = NonZeroUsize::new(capacity).expect("a RingStr needs a non-zero capacity");
        let buf = RingStrStorage::new(capacity);
        Self {
            capacity,
            buf: RwLock::new(buf),
            queue: ThingBuf::new(capacity.get() + EXTRA_CAPACITY_THRESHOLD_LIMIT),
        }
    }

    /// Take a fresh, fully synced private copy of the ring.
    pub fn create_storage(&self) -> RingStrStorage {
        let buf = self.sync_lock();
        buf.clone()
    }

    /// Catch an existing copy up with the ring.
    ///
    /// `storage` must have come from *this* ring, via
    /// [`create_storage`](RingStr::create_storage). Cursors are only
    /// meaningful within the ring that issued them: syncing a foreign storage
    /// splices in unrelated lines, or silently does nothing forever if the
    /// other ring happens to be behind. This is a precondition, not something
    /// the ring checks — use
    /// [`force_update_storage`](RingStr::force_update_storage) to move a
    /// storage between rings on purpose.
    pub fn update_storage(&self, storage: &mut RingStrStorage) {
        let buf = self.sync_lock();
        storage.sync_data(&buf);
    }

    /// Re-point an existing storage at this ring, discarding what it held.
    ///
    /// [`update_storage`](RingStr::update_storage) requires a storage this
    /// ring issued, because a cursor only means something within its own
    /// ring. This is the supported way to break that tie: the storage's
    /// contents and cursor are replaced with this ring's, so a buffer can be
    /// moved from one ring to another — following a view as it switches
    /// between logs, say.
    ///
    /// Unlike [`create_storage`](RingStr::create_storage) this allocates
    /// nothing: the storage keeps the buffer, and the eviction bound, it was
    /// built with. Moving onto a ring with a wider window therefore yields
    /// only the newest lines that fit.
    pub fn force_update_storage(&self, storage: &mut RingStrStorage) {
        let buf = self.sync_lock();
        storage.reset_from(&buf);
    }

    /// Fold pending writes into the shared buffer and return a read guard.
    ///
    /// This is the only place the queue is drained, so reading is what makes
    /// the ring advance. When there is something to fold, the write lock is
    /// taken and then *downgraded*, which hands the caller a consistent view
    /// without a gap where another writer could slip in. At most
    /// `capacity + EXTRA_CAPACITY_THRESHOLD_LIMIT` lines are folded per call,
    /// bounding the time spent holding the lock.
    ///
    /// With nothing pending there is nothing to fold, so the fast path takes
    /// the read lock directly and several readers can refresh at once. The
    /// emptiness check is a snapshot and a writer may push right after it —
    /// which is no different from pushing a moment after a drain finishes.
    /// The reader just picks those lines up on its next sync.
    fn sync_lock(&self) -> RwLockReadGuard<'_, RingStrStorage> {
        if self.queue.is_empty() {
            return self.buf.read();
        }

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
    /// Never blocks and never reports failure. Under congestion the oldest
    /// pending lines are dropped to make room, and in the extreme — enough
    /// concurrent writers to exhaust the queue between the check and the push
    /// — this line itself is dropped instead. Losing output is preferable to
    /// stalling the process producing it; this is a tail, not a transcript.
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
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    };

    // ---------------------------------------------------------------- helpers

    /// Collect a storage's lines.
    fn lines(storage: &RingStrStorage) -> Vec<String> {
        storage.lines().cloned().collect()
    }

    fn assert_ring(storage: &RingStrStorage, expected: &[&str]) {
        assert_eq!(lines(storage), expected);
    }

    /// Lines are written as `<writer>:<seq>`; this reads the pair back.
    fn parse(line: &str) -> (u32, u64) {
        let (w, s) = line.split_once(':').expect("malformed line");
        (
            w.parse().expect("bad writer id"),
            s.parse().expect("bad seq"),
        )
    }

    /// Invariants that must hold for *any* snapshot, at any time, no matter how
    /// writes and syncs interleave.
    ///
    /// The ring is allowed to lose lines, but never to corrupt them: every line
    /// must be one that was actually written, and each writer's lines must
    /// appear in the order that writer produced them.
    fn assert_snapshot_sane(snapshot: &[String], capacity: usize, writers: u32, max_seq: u64) {
        assert!(
            snapshot.len() <= capacity,
            "ring exceeded its capacity: {} lines retained for a capacity of {capacity}",
            snapshot.len()
        );

        let mut last_seq: Vec<Option<u64>> = vec![None; writers as usize];
        for line in snapshot {
            let (w, seq) = parse(line);
            assert!(w < writers, "line from an unknown writer: {line:?}");
            assert!(seq <= max_seq, "line from the future: {line:?}");
            if let Some(previous) = last_seq[w as usize] {
                assert!(
                    seq > previous,
                    "writer {w} out of order or duplicated: {previous} then {seq}"
                );
            }
            last_seq[w as usize] = Some(seq);
        }
    }

    // ------------------------------------------------------- sequential tests

    /// Writes are staged until somebody reads, then land in order.
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
        // A storage taken before the writes shows nothing until it syncs.
        assert_ring(&storage, &[]);
        ring.update_storage(&mut storage);
        assert_ring(&storage, &["oi", "tudo", "bem"]);
    }

    /// A ring that retains nothing is rejected outright.
    #[test]
    #[should_panic(expected = "non-zero capacity")]
    fn zero_capacity_is_rejected() {
        RingStr::new(0);
    }

    /// Eviction is bound by the *requested* capacity, not by however much the
    /// `VecDeque` happens to have allocated.
    ///
    /// `VecDeque::with_capacity` only promises *at least* what was asked for,
    /// so `buf.capacity()` is not a contract the ring can lean on. Here the
    /// allocation is deliberately grown past the logical bound to make the
    /// difference observable.
    #[test]
    fn eviction_follows_the_logical_capacity_not_the_allocation() {
        let mut storage = RingStrStorage::new(NonZeroUsize::new(3).unwrap());
        storage.buf.reserve(100);
        assert!(
            storage.buf.capacity() > 3,
            "test setup failed to over-allocate the deque"
        );

        for i in 0..10 {
            storage.push(&format!("{i}"));
        }
        assert_ring(&storage, &["7", "8", "9"]);

        // ...and a clone has to inherit the bound, not the allocation.
        let mut cloned = storage.clone();
        assert_eq!(cloned.capacity.get(), 3);
        for i in 10..14 {
            cloned.push(&format!("{i}"));
        }
        assert_ring(&cloned, &["11", "12", "13"]);
    }

    /// Syncing an idle ring takes the read-lock fast path; it must not latch
    /// that emptiness and hide later writes.
    #[test]
    fn the_idle_fast_path_does_not_hide_later_writes() {
        let ring = RingStr::new(4);
        ring.write_line("a");
        let mut storage = ring.create_storage();
        assert_ring(&storage, &["a"]);

        // Queue is empty now: these all go down the fast path.
        for _ in 0..3 {
            ring.update_storage(&mut storage);
            assert_ring(&storage, &["a"]);
        }

        ring.write_line("b");
        ring.update_storage(&mut storage);
        assert_ring(&storage, &["a", "b"]);
    }

    /// Several readers refreshing an idle ring at once.
    ///
    /// On the fast path they hold the read lock concurrently rather than
    /// serialising on the write lock, so this pins that sharing it neither
    /// deadlocks nor lets a snapshot come out wrong.
    #[test]
    fn concurrent_readers_share_the_idle_fast_path() {
        const READERS: u32 = 8;
        let ring = Arc::new(RingStr::new(4));
        for i in 0..4 {
            ring.write_line(format!("0:{i}"));
        }
        // Drain, so every reader below finds an empty queue.
        let expected = lines(&ring.create_storage());

        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let ring = ring.clone();
                let expected = expected.clone();
                std::thread::spawn(move || {
                    let mut storage = ring.create_storage();
                    for _ in 0..2_000 {
                        ring.update_storage(&mut storage);
                        assert_eq!(lines(&storage), expected);
                        assert_eq!(lines(&ring.create_storage()), expected);
                    }
                })
            })
            .collect();

        for r in readers {
            r.join().unwrap();
        }
    }

    /// The retained window is the newest `capacity` lines, never more.
    #[test]
    fn keeps_only_the_newest_lines() {
        let ring = RingStr::new(4);
        for i in 0..100 {
            ring.write_line(format!("{i}"));
        }
        let storage = ring.create_storage();
        assert_ring(&storage, &["96", "97", "98", "99"]);
    }

    /// Syncing again with nothing new must not duplicate or shift anything.
    #[test]
    fn sync_is_idempotent() {
        let ring = RingStr::new(4);
        for i in 0..10 {
            ring.write_line(format!("{i}"));
        }
        let mut storage = ring.create_storage();
        let first = lines(&storage);
        for _ in 0..5 {
            ring.update_storage(&mut storage);
            assert_eq!(lines(&storage), first, "an empty sync changed the buffer");
        }
    }

    /// A buffer synced line by line and one taken fresh must agree.
    ///
    /// This is the load-bearing invariant of the offset arithmetic in
    /// [`RingStrStorage::sync_data`]: incremental catch-up has to land on
    /// exactly the same window as a full snapshot.
    #[test]
    fn incremental_sync_matches_a_fresh_snapshot() {
        // Batch sizes chosen to straddle every branch of `sync_data`:
        // fewer new lines than held, exactly the capacity, and far more.
        for batch in [1usize, 3, 4, 5, 8, 40] {
            let ring = RingStr::new(4);
            let mut incremental = ring.create_storage();
            let mut written = 0;
            for _ in 0..6 {
                for _ in 0..batch {
                    ring.write_line(format!("{written}"));
                    written += 1;
                }
                ring.update_storage(&mut incremental);
                let fresh = ring.create_storage();
                assert_eq!(
                    lines(&incremental),
                    lines(&fresh),
                    "incremental and fresh diverged with batch {batch}"
                );
            }
        }
    }

    /// A reader that fell behind by more than the whole ring still recovers,
    /// landing on the newest lines instead of a stale or mixed window.
    #[test]
    fn a_reader_left_far_behind_recovers() {
        let ring = RingStr::new(4);
        ring.write_line("old");
        let mut storage = ring.create_storage();
        assert_ring(&storage, &["old"]);

        for i in 0..50 {
            ring.write_line(format!("{i}"));
        }
        ring.update_storage(&mut storage);
        assert_ring(&storage, &["46", "47", "48", "49"]);
    }

    /// The boundary where the number of missed lines equals the capacity.
    #[test]
    fn sync_at_the_capacity_boundary() {
        for missed in 3..=5usize {
            let ring = RingStr::new(4);
            for i in 0..4 {
                ring.write_line(format!("a{i}"));
            }
            let mut storage = ring.create_storage();
            for i in 0..missed {
                ring.write_line(format!("b{i}"));
            }
            ring.update_storage(&mut storage);
            assert_eq!(
                lines(&storage),
                lines(&ring.create_storage()),
                "missing {missed} lines produced a wrong window"
            );
            assert_eq!(
                storage.lines().last().map(String::as_str),
                Some(format!("b{}", missed - 1).as_str()),
                "the newest line was lost"
            );
        }
    }

    /// Nothing is dropped while the queue stays under its congestion
    /// threshold, even if nobody reads until the very end.
    #[test]
    fn no_loss_below_the_congestion_threshold() {
        let capacity = 8;
        let ring = RingStr::new(capacity);
        let total = capacity + EXTRA_CAPACITY_THRESHOLD - 1;
        for i in 0..total {
            ring.write_line(format!("{i}"));
        }
        let storage = ring.create_storage();
        let expected: Vec<String> = (total - capacity..total).map(|i| i.to_string()).collect();
        assert_eq!(
            lines(&storage),
            expected,
            "lines were dropped below the threshold"
        );
    }

    /// Far past the congestion threshold the ring drops lines, but it must drop
    /// the *oldest* ones: the newest line always survives.
    #[test]
    fn congestion_drops_the_oldest_not_the_newest() {
        let ring = RingStr::new(4);
        for i in 0..100_000u32 {
            ring.write_line(format!("{i}"));
        }
        let storage = ring.create_storage();
        let got = lines(&storage);

        assert!(got.len() <= 4, "capacity exceeded under congestion");
        assert_eq!(
            got.last().map(String::as_str),
            Some("99999"),
            "the newest line was dropped instead of the oldest"
        );
        let seqs: Vec<u32> = got.iter().map(|s| s.parse().unwrap()).collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "lines came out of order"
        );
    }

    // ------------------------------------------------------ concurrency tests
    //
    // Two shapes of test here, and the distinction matters:
    //
    // - *Overlap* tests pace the writer and gate it behind the reader, then
    //   assert the reader really did observe in-progress states. They verify
    //   ordering guarantees under interleaving, and refuse to pass if the
    //   threads happened to run one after the other.
    // - *Contention* tests let everything run flat out and assert only
    //   invariants that hold at any instant. They are the race hunters, so
    //   they carry no timing-dependent assertion that could make them flaky.

    /// Spawn a writer that waits for the readers to be looping first.
    ///
    /// Without the handshake a release-mode writer can finish before a reader
    /// thread is even scheduled, quietly turning a concurrency test into a
    /// sequential one.
    fn spawn_writer(
        ring: Arc<RingStr>,
        id: u32,
        total: u64,
        ready: Arc<AtomicBool>,
        paced: bool,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while !ready.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            for seq in 0..total {
                ring.write_line(format!("{id}:{seq}"));
                // Give the reader room to land between writes, so the
                // interleaving being tested actually happens.
                if paced && seq % 64 == 0 {
                    std::thread::yield_now();
                }
            }
        })
    }

    /// One writer, one reader, interleaved.
    ///
    /// Every snapshot must stay ordered and within capacity, the window may
    /// only ever move forward, and the last line written must be visible once
    /// the writer is done.
    #[test]
    fn concurrent_reader_sees_a_forward_moving_window() {
        const TOTAL: u64 = 20_000;
        let capacity = 16;
        let ring = Arc::new(RingStr::new(capacity));
        let ready = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));

        let reader = {
            let ring = ring.clone();
            let ready = ready.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let mut storage = ring.create_storage();
                let mut newest: Option<u64> = None;
                let (mut syncs, mut partial) = (0u64, 0u64);
                ready.store(true, Ordering::Release);
                while !done.load(Ordering::Relaxed) {
                    ring.update_storage(&mut storage);
                    let snapshot = lines(&storage);
                    assert_snapshot_sane(&snapshot, capacity, 1, TOTAL);
                    if let Some(last) = snapshot.last() {
                        let (_, seq) = parse(last);
                        if let Some(previous) = newest {
                            assert!(
                                seq >= previous,
                                "the window moved backwards: {previous} then {seq}"
                            );
                        }
                        newest = Some(seq);
                        if seq < TOTAL - 1 {
                            partial += 1;
                        }
                    }
                    syncs += 1;
                }
                (syncs, partial)
            })
        };

        let writer = spawn_writer(ring.clone(), 0, TOTAL, ready, true);
        writer.join().unwrap();
        done.store(true, Ordering::Relaxed);
        let (syncs, partial) = reader.join().unwrap();

        // Guard the test itself: if these ever fail the threads ran one after
        // the other and the assertions above proved nothing.
        assert!(syncs > 0, "the reader never ran");
        assert!(
            partial > 0,
            "the reader only ever saw the finished ring ({syncs} syncs); \
             this test stopped exercising concurrency"
        );

        // Everything the writer produced has been handed over, so a final sync
        // has to surface the last line.
        let storage = ring.create_storage();
        assert_eq!(
            lines(&storage).last().map(String::as_str),
            Some(format!("0:{}", TOTAL - 1).as_str()),
            "the last line never reached the ring"
        );
    }

    /// Many writers and many readers, flat out.
    ///
    /// Interleaving between writers is unconstrained, but each writer's own
    /// lines must stay ordered and unduplicated in every snapshot, and no
    /// snapshot may ever exceed the capacity.
    #[test]
    fn concurrent_writers_and_readers_never_corrupt_a_snapshot() {
        const WRITERS: u32 = 4;
        const READERS: u32 = 3;
        const PER_WRITER: u64 = 5_000;
        let capacity = 32;
        let ring = Arc::new(RingStr::new(capacity));
        let ready = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let live_readers = Arc::new(AtomicU32::new(0));

        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let ring = ring.clone();
                let done = done.clone();
                let ready = ready.clone();
                let live_readers = live_readers.clone();
                std::thread::spawn(move || {
                    let mut storage = ring.create_storage();
                    if live_readers.fetch_add(1, Ordering::AcqRel) + 1 == READERS {
                        ready.store(true, Ordering::Release);
                    }
                    while !done.load(Ordering::Relaxed) {
                        // Alternate the two read paths so both are exercised
                        // against live writers.
                        ring.update_storage(&mut storage);
                        assert_snapshot_sane(&lines(&storage), capacity, WRITERS, PER_WRITER);
                        let fresh = ring.create_storage();
                        assert_snapshot_sane(&lines(&fresh), capacity, WRITERS, PER_WRITER);
                    }
                })
            })
            .collect();

        let writers: Vec<_> = (0..WRITERS)
            .map(|id| spawn_writer(ring.clone(), id, PER_WRITER, ready.clone(), false))
            .collect();

        for w in writers {
            w.join().unwrap();
        }
        done.store(true, Ordering::Relaxed);
        for r in readers {
            r.join().unwrap();
        }

        assert_snapshot_sane(
            &lines(&ring.create_storage()),
            capacity,
            WRITERS,
            PER_WRITER,
        );
    }

    /// A reader syncing *while* the writer is dropping congested lines.
    ///
    /// This is the path where a producer and a consumer pop from the queue at
    /// the same time. Lines may be lost, but the window must never rewind,
    /// duplicate a line or reorder one.
    #[test]
    fn concurrent_congestion_keeps_the_window_ordered() {
        const TOTAL: u64 = 100_000;
        let capacity = 4;
        let ring = Arc::new(RingStr::new(capacity));
        let ready = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));

        let reader = {
            let ring = ring.clone();
            let ready = ready.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let mut storage = ring.create_storage();
                let mut newest: Option<u64> = None;
                ready.store(true, Ordering::Release);
                while !done.load(Ordering::Relaxed) {
                    ring.update_storage(&mut storage);
                    let snapshot = lines(&storage);
                    assert_snapshot_sane(&snapshot, capacity, 1, TOTAL);
                    if let Some(last) = snapshot.last() {
                        let (_, seq) = parse(last);
                        assert!(newest.is_none_or(|p| seq >= p), "the window rewound");
                        newest = Some(seq);
                    }
                }
            })
        };

        let writer = spawn_writer(ring.clone(), 0, TOTAL, ready, false);
        writer.join().unwrap();
        done.store(true, Ordering::Relaxed);
        reader.join().unwrap();
    }

    /// Concurrent readers must not disturb each other's cursors.
    ///
    /// Both read paths take the same internal write lock to drain the queue,
    /// so this pins that draining on one storage never advances another.
    #[test]
    fn concurrent_readers_keep_independent_cursors() {
        const TOTAL: u64 = 20_000;
        const READERS: u32 = 4;
        let capacity = 8;
        let ring = Arc::new(RingStr::new(capacity));
        let ready = Arc::new(AtomicBool::new(false));

        let writer = spawn_writer(ring.clone(), 0, TOTAL, ready.clone(), false);

        // A storage taken before the writes and never synced must stay frozen
        // no matter how much the other readers drain.
        let frozen = ring.create_storage();
        let before = lines(&frozen);
        ready.store(true, Ordering::Release);

        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let ring = ring.clone();
                std::thread::spawn(move || {
                    let mut storage = ring.create_storage();
                    for _ in 0..500 {
                        ring.update_storage(&mut storage);
                        assert_snapshot_sane(&lines(&storage), capacity, 1, TOTAL);
                    }
                })
            })
            .collect();

        for r in readers {
            r.join().unwrap();
        }
        writer.join().unwrap();

        assert_eq!(lines(&frozen), before, "an unsynced storage was mutated");
    }

    // ------------------------------------------------ moving between rings

    /// Forcing a storage onto another ring leaves it exactly as if that ring
    /// had created it.
    #[test]
    fn force_update_moves_a_storage_between_rings() {
        let a = RingStr::new(4);
        for i in 0..10 {
            a.write_line(format!("a{i}"));
        }
        let mut storage = a.create_storage();
        assert_ring(&storage, &["a6", "a7", "a8", "a9"]);

        let b = RingStr::new(4);
        for i in 0..7 {
            b.write_line(format!("b{i}"));
        }
        b.force_update_storage(&mut storage);

        // Both rings keep the same number of lines, so the move lands on
        // exactly what this ring would have handed out fresh.
        let fresh = b.create_storage();
        assert_eq!(
            lines(&storage),
            lines(&fresh),
            "content diverged from a fresh storage"
        );
        assert_eq!(storage.offset, fresh.offset, "cursor was not adopted");
    }

    /// The payoff: after a forced move, ordinary incremental syncing works
    /// against the new ring.
    #[test]
    fn a_forced_storage_syncs_incrementally_afterwards() {
        let a = RingStr::new(4);
        for i in 0..10 {
            a.write_line(format!("a{i}"));
        }
        let mut storage = a.create_storage();

        let b = RingStr::new(4);
        b.write_line("b0");
        b.force_update_storage(&mut storage);
        assert_ring(&storage, &["b0"]);

        for i in 1..6 {
            b.write_line(format!("b{i}"));
            b.update_storage(&mut storage);
        }
        assert_ring(&storage, &["b2", "b3", "b4", "b5"]);
        assert_eq!(lines(&storage), lines(&b.create_storage()));
    }

    /// A ring far *behind* the storage is the case that used to freeze a
    /// buffer silently; forcing has to recover from it.
    #[test]
    fn force_update_recovers_from_a_ring_that_is_behind() {
        let a = RingStr::new(4);
        for i in 0..1_000 {
            a.write_line(format!("a{i}"));
        }
        let mut storage = a.create_storage();
        let far_ahead = storage.offset;

        let b = RingStr::new(4);
        b.write_line("b0");
        assert!(
            b.create_storage().offset < far_ahead,
            "test setup: B should be behind"
        );

        b.force_update_storage(&mut storage);
        assert_ring(&storage, &["b0"]);
        b.write_line("b1");
        b.update_storage(&mut storage);
        assert_ring(&storage, &["b0", "b1"]);
    }

    /// The storage keeps its own eviction bound across a move.
    ///
    /// Its buffer is allocated once and stays that size, so moving onto a
    /// wider ring yields only the newest lines that fit, and moving onto a
    /// narrower one leaves the extra room available for later.
    #[test]
    fn force_update_keeps_the_storage_capacity() {
        // A narrow storage moved onto a ring holding a wider window.
        let narrow = RingStr::new(3);
        for i in 0..3 {
            narrow.write_line(format!("n{i}"));
        }
        let mut storage = narrow.create_storage();
        assert_eq!(storage.capacity.get(), 3);

        let wide = RingStr::new(8);
        for i in 0..8 {
            wide.write_line(format!("w{i}"));
        }
        wide.force_update_storage(&mut storage);
        assert_eq!(
            storage.capacity.get(),
            3,
            "the storage adopted a foreign bound"
        );
        assert_ring(&storage, &["w5", "w6", "w7"]);

        // Later syncs against the wide ring still respect the storage bound.
        for i in 8..12 {
            wide.write_line(format!("w{i}"));
        }
        wide.update_storage(&mut storage);
        assert_ring(&storage, &["w9", "w10", "w11"]);

        // And the other direction: a wide storage onto a narrow ring keeps
        // room to spare rather than shrinking.
        let mut wide_storage = wide.create_storage();
        assert_eq!(wide_storage.capacity.get(), 8);
        narrow.force_update_storage(&mut wide_storage);
        assert_eq!(wide_storage.capacity.get(), 8);
        assert_ring(&wide_storage, &["n0", "n1", "n2"]);

        for i in 3..10 {
            narrow.write_line(format!("n{i}"));
        }
        narrow.update_storage(&mut wide_storage);
        // The narrow ring only ever hands over its own 3-line window, so the
        // wide storage keeps history the ring itself has already dropped —
        // with a gap where n3..n6 were evicted before it caught up.
        assert_ring(&wide_storage, &["n0", "n1", "n2", "n7", "n8", "n9"]);
    }

    /// Line buffers are refilled in place, not reallocated.
    ///
    /// This is the only reason to force a storage rather than overwrite it
    /// with a fresh one, so it is worth pinning: the `String`s a storage
    /// already owns must survive the move.
    #[test]
    fn force_update_reuses_the_line_allocations() {
        let a = RingStr::new(4);
        for i in 0..4 {
            a.write_line(format!("a{i:02}"));
        }
        let mut storage = a.create_storage();
        let before: Vec<*const u8> = storage.buf.iter().map(|s| s.as_ptr()).collect();
        assert_eq!(before.len(), 4);

        let b = RingStr::new(4);
        for i in 0..4 {
            // Same length, so a refill cannot need to grow the allocation.
            b.write_line(format!("b{i:02}"));
        }
        b.force_update_storage(&mut storage);

        let after: Vec<*const u8> = storage.buf.iter().map(|s| s.as_ptr()).collect();
        assert_eq!(
            before, after,
            "line buffers were reallocated instead of refilled"
        );
        assert_ring(&storage, &["b00", "b01", "b02", "b03"]);
    }

    /// The deque behind a storage is allocated once and never grown.
    ///
    /// Keeping the storage's own bound is what makes this hold: adopting a
    /// wider ring's bound would force the buffer to be reallocated.
    #[test]
    fn force_update_never_reallocates_the_deque() {
        let ring = RingStr::new(4);
        ring.write_line("seed");
        let mut storage = ring.create_storage();
        let allocation = storage.buf.capacity();

        // Move it across rings of every shape: wider, narrower, emptier.
        for capacity in [1usize, 4, 64, 2] {
            let other = RingStr::new(capacity);
            for i in 0..capacity * 2 {
                other.write_line(format!("{capacity}-{i}"));
            }
            other.force_update_storage(&mut storage);
            assert!(
                storage.lines().count() <= 4,
                "the storage overflowed its own bound"
            );
            assert_eq!(
                storage.buf.capacity(),
                allocation,
                "the deque was reallocated moving onto a ring of {capacity}"
            );
        }
    }

    /// Forcing onto a ring that is being written to concurrently.
    ///
    /// The move must land on a coherent window: after switching to B, nothing
    /// from A may survive in the storage.
    #[test]
    fn force_update_is_coherent_against_a_live_ring() {
        const TOTAL: u64 = 20_000;
        let capacity = 8;
        let ring_a = Arc::new(RingStr::new(capacity));
        let ring_b = Arc::new(RingStr::new(capacity));
        let ready = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));

        let writers = vec![
            spawn_writer(ring_a.clone(), 0, TOTAL, ready.clone(), false),
            spawn_writer(ring_b.clone(), 1, TOTAL, ready.clone(), false),
        ];

        let switcher = {
            let (ring_a, ring_b) = (ring_a.clone(), ring_b.clone());
            let ready = ready.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let mut storage = ring_a.create_storage();
                let mut on_a = true;
                ready.store(true, Ordering::Release);
                let mut switches = 0u64;
                while !done.load(Ordering::Relaxed) {
                    let (ring, id) = if on_a { (&ring_b, 1) } else { (&ring_a, 0) };
                    ring.force_update_storage(&mut storage);
                    on_a = !on_a;
                    switches += 1;

                    // Only the ring just switched to may be represented.
                    let snapshot = lines(&storage);
                    assert!(snapshot.len() <= capacity, "capacity exceeded after a move");
                    let mut last: Option<u64> = None;
                    for line in &snapshot {
                        let (w, seq) = parse(line);
                        assert_eq!(w, id, "a line from the previous ring survived the move");
                        assert!(
                            last.is_none_or(|p| seq > p),
                            "lines out of order after a move"
                        );
                        last = Some(seq);
                    }

                    // ...and an ordinary sync stays valid on the new ring.
                    ring.update_storage(&mut storage);
                    for line in lines(&storage) {
                        assert_eq!(parse(&line).0, id, "a foreign line appeared after syncing");
                    }
                }
                switches
            })
        };

        for w in writers {
            w.join().unwrap();
        }
        done.store(true, Ordering::Relaxed);
        let switches = switcher.join().unwrap();
        assert!(switches > 0, "the switcher never ran");
    }

    /// Storages handed out to different readers are independent: syncing one
    /// must not disturb another.
    #[test]
    fn readers_do_not_interfere_with_each_other() {
        let ring = RingStr::new(4);
        ring.write_line("a");
        let mut slow = ring.create_storage();
        ring.update_storage(&mut slow);

        let mut fast = ring.create_storage();
        for i in 0..10 {
            ring.write_line(format!("{i}"));
            ring.update_storage(&mut fast);
        }
        assert_ring(&slow, &["a"]);
        assert_ring(&fast, &["6", "7", "8", "9"]);

        ring.update_storage(&mut slow);
        assert_eq!(lines(&slow), lines(&fast));
    }
}
