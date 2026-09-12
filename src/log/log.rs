use std::ops::Range;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicU32, AtomicU64, Ordering},
};

use crate::{
    log::{
        LogBuffer,
        chunk::{LogChunk, LogChunkData, LogReaderChunk},
    },
    util::{
        arena::{Arena, ArenaBlock},
        localring::LocalRingBuffer,
    },
};
use parking_lot::Mutex;
use tokio::{io::AsyncRead, task::JoinHandle};

/// Size of the queue chunk
const CHUNK_QUEUE_SIZE: usize = 128;

/// Blocks set aside for the chunk each reader task fills before handing it
/// over.
///
/// The ring and the queue are fixed, so their share of the arena is exact.
/// This is the part that is not: one block per [`LogBuffer`] alive, and a
/// unit makes a new one for every run — but because a reader gives its chunk
/// back when it finishes, this bounds how many readers a log can have *at
/// once*, not how many it can have over its life.
const WRITER_CHUNK_BLOCKS: usize = 64;

/// A handle for appending to a [`Log`] from a reader task.
///
/// Weak on purpose: a task still draining its pipe should not keep a log
/// alive after whatever owned it is gone.
///
/// Once it is, there is nowhere left to put a chunk, and the reader stops
/// rather than reading on into a void — see
/// [`read`](super::LogBuffer::read). That closes its end of the pipe, so a
/// process still writing gets a `SIGPIPE`. Deliberate teardown does not rely
/// on that: a [`Unit`](crate::unit::Unit) shuts its run down before letting
/// go of the log.
pub struct LogWriterRef {
    /// The log to append to. Weak on purpose — see the type's own docs; the
    /// blocks a reader is still filling stay alive through the chunk itself,
    /// not through this.
    inner: Weak<LogInner>,
    /// Stamped onto every chunk this writer hands over. Fixed at creation:
    /// one writer is one source of output for its whole life, which is what
    /// makes the stamp worth anything.
    id: LogWriterId,
}

impl LogWriterRef {
    /// Share the writer ref, it is like Clone, but explicit
    pub fn share(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            id: self.id,
        }
    }

    /// Fork the ref
    pub fn fork(&self) -> Self {
        let Some(inner) = self.inner.upgrade() else {
            return Self {
                inner: Weak::new(),
                id: LogWriterId::new(0),
            };
        };
        let id = inner.next_writer_id();
        Self {
            inner: Arc::downgrade(&inner),
            id,
        }
    }

    /// Which writer this is.
    ///
    /// The same id its chunks carry, so a caller holding the writer can find
    /// that writer's output in the history.
    pub fn id(&self) -> LogWriterId {
        self.id
    }

    /// Take a chunk for a reader to fill.
    ///
    /// A reader's chunk is the one part of the arena that churns: the ring
    /// and the queue are built once and live as long as the log, while a new
    /// reader appears for every run of a unit. So one that has been given
    /// back is reused before the arena is asked for another — otherwise a
    /// unit restarted often enough would drain a pool that never refills.
    ///
    /// Past [`WRITER_CHUNK_BLOCKS`] readers at once the pool has nothing
    /// left, and the chunk comes from the heap instead. It behaves the same;
    /// it just sits apart from the others. Better a reader that records
    /// without the locality than one that cannot record at all.
    pub(super) fn chunk(&self) -> LogChunk {
        let Some(inner) = self.inner.upgrade() else {
            // The log is gone, so this reader has nowhere to put anything and
            // will stop at its first push. It still needs somewhere to write
            // until it finds that out.
            return LogChunk::new(ArenaBlock::heap());
        };
        if let Some(chunk) = inner.chunks_free.lock().pop() {
            return chunk;
        }
        LogChunk::new(inner.arena.alloc_or_heap())
    }

    /// Give a reader's chunk back for the next one to use.
    ///
    /// Cleared on the way in, so nothing of the run that finished can show up
    /// in the run that follows. A log already gone takes nothing — the arena
    /// goes with it, so there is nothing to save.
    pub(super) fn recycle(&self, mut chunk: LogChunk) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        chunk.clear();
        inner.chunks_free.lock().push(chunk);
    }

    /// Hand a finished chunk over to the log and take a recycled one back.
    ///
    /// Nothing is copied: the chunk swaps places with whichever slot the ring
    /// was about to overwrite, so it comes back owning the block that slot
    /// used to hold. That is the whole point of the ring holding pre built
    /// chunks — a log at capacity never allocates again.
    ///
    /// The chunk is cleared before it comes back, so the caller can go
    /// straight on writing. Forgetting that step would leave it marked
    /// finished and every later push would place nothing, which is why it
    /// happens here rather than at the call site.
    ///
    /// Returns whether the log was still there to take it. One that has been
    /// dropped takes nothing and the chunk is left as it was — there is
    /// nowhere for its contents to go, and the caller has no reason to carry
    /// on.
    ///
    /// The lock is held for one swap and nothing else. Writers and readers
    /// share it, so what keeps them out of each other's way is not avoiding
    /// the lock but never doing anything slow under it.
    pub(super) fn push_chunk(&mut self, chunk: &mut LogChunk) -> bool {
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        // Stamp before the hand-off: after the swap this chunk is the
        // recycled one, and the stamp belongs to the content going in.
        chunk.set_writer(self.id);
        {
            let mut history = inner.history.lock();
            history.chunks.push().swap(chunk);
            history.pushed += 1;
            // Publish it for readers that have not taken the lock. A plain
            // release store, which on the common architectures is an ordinary
            // move — where bumping a shared atomic would take the cache line
            // exclusively and serialise, for a number the lock already
            // protects.
            inner.pushed_hint.store(history.pushed, Ordering::Release);
        }
        // Outside the lock: the displaced chunk is ours alone now, and
        // clearing it is nobody else's business.
        chunk.clear();
        true
    }

    /// Drain `read` into the log until it ends, on a task of its own.
    ///
    /// Takes the writer by value: a pipe has one reader, and this is it. The
    /// task owns the [`LogBuffer`] and so the partial character carried
    /// between reads, which is why the pump cannot be split across tasks.
    ///
    /// It ends on end of stream — including the disconnect that is how a
    /// child's output normally stops — and returns `Err` only for a read
    /// error that is genuinely unexpected. The handle is usually left
    /// detached, since a process's exit status is the interesting news, not
    /// its reader's.
    pub fn consume_spawn<R>(self, mut read: R) -> JoinHandle<std::io::Result<()>>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let mut log_buffer = LogBuffer::new(self);
            log_buffer.read_all(&mut read).await
        })
    }

    /// Spawn but with stderr
    pub fn consume_spawn_stderr<R1, R2>(
        self,
        mut stdout: R1,
        mut stderr: R2,
    ) -> JoinHandle<std::io::Result<()>>
    where
        R1: AsyncRead + Unpin + Send + 'static,
        R2: AsyncRead + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let mut stderr_buffer = LogBuffer::new(self.fork());
            let mut stdout_buffer = LogBuffer::new(self);
            let (r1, r2) = tokio::join!(
                stderr_buffer.read_all(&mut stderr),
                stdout_buffer.read_all(&mut stdout)
            );
            r1.and(r2)
        })
    }
}

/// Which writer produced a chunk.
///
/// A log takes from as many writers as a unit has processes, and their chunks
/// land interleaved in one ring. The stamp is what lets them be told apart
/// again afterwards — to label a line, to colour it, or to filter the history
/// down to one process.
///
/// Ids are handed out by the log, densely from zero, so they double as an
/// index into whatever a renderer keeps per writer.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LogWriterId {
    /// Dense from zero, so it doubles as an index into whatever a renderer
    /// keeps per writer. `u32::MAX` is [`UNSET`](LogWriterId::UNSET) and
    /// belongs to no writer, which is why an arena of that many writers
    /// cannot exist.
    raw: u32,
}

impl LogWriterId {
    /// The stamp a chunk carries before any writer has claimed it.
    ///
    /// A chunk in the ring always carries a real id — it is stamped on the
    /// way in — so this only ever shows on one that has not been handed over
    /// yet, or on one a recycler has just reset.
    pub const UNSET: Self = Self::new(u32::MAX);

    /// Mint the id numbered `raw`. The log is the only thing that should be
    /// choosing these.
    pub(super) const fn new(raw: u32) -> Self {
        Self { raw }
    }

    /// The number behind the id, for indexing.
    pub const fn index(self) -> usize {
        self.raw as usize
    }
}

/// The recent output of a unit, as a fixed ring of chunks.
///
/// Every chunk is built when the log is and recycled from then on, so a log
/// at capacity never allocates again — a finished line changes hands by
/// swapping buffers, never by copying. The price is paid up front and in
/// full: the log holds `capacity * LOG_CHUNK_SIZE` bytes whether the lines
/// turn out long or short.
pub struct Log {
    /// Everything the writers share. Strong here and weak in every writer, so
    /// the log lives exactly as long as whatever owns it — a unit — and not a
    /// moment longer because a reader task was slow to notice.
    inner: Arc<LogInner>,
}

impl Log {
    /// Create a log keeping the last `capacity` chunks.
    ///
    /// A chunk is one line, or one slice of a line too long to fit in
    /// `LOG_CHUNK_SIZE` bytes, so a log of long lines remembers fewer of
    /// them than its capacity suggests. Every chunk is allocated here.
    ///
    /// # Panics
    ///
    /// If `capacity` is zero.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: LogInner::new(capacity),
        }
    }

    /// Get a handle that can append to this log.
    ///
    /// Any number may exist at once: a unit running several processes hands
    /// one to each.
    ///
    /// Their chunks land in the order they are handed over, which is not
    /// quite the order the lines were printed: a chunk carries several lines,
    /// so one writer's batch arrives whole before another's. Within a writer
    /// the order is exact. A line split across chunks is continued by that
    /// writer's next chunk and not by whatever sits next in the ring — see
    /// [`LogChunkData`](super::chunk::LogChunkData).
    pub fn writer(&self) -> LogWriterRef {
        LogWriterRef {
            inner: Arc::downgrade(&self.inner),
            id: self.inner.next_writer_id(),
        }
    }

    /// Open a view onto this log.
    ///
    /// The reader starts empty and behind: its first
    /// [`sync`](LogReader::sync) picks up whatever history is already there,
    /// and every one after that only what has arrived since.
    ///
    /// It takes its own copy of the chunks, so any number may exist and none
    /// of them holds the log open — a view left behind stops changing rather
    /// than keeping a finished unit's memory alive.
    pub fn reader(&self) -> LogReader {
        LogReader::new(
            Arc::downgrade(&self.inner),
            self.inner.history.lock().chunks.capacity(),
        )
    }

    /// Print the history to stdout, oldest first.
    ///
    /// Scaffolding until there is a CLI, and it goes when there is. It holds
    /// the lock for the whole print, which nothing that runs often may do —
    /// stalling every writer of the unit for the length of a terminal write
    /// is only acceptable because this is a debugging aid.
    ///
    /// Prints pieces, not lines: a line split across chunks comes out in
    /// parts, each but the last marked. Joining them needs the writer each
    /// piece belongs to, since another process's chunk can sit between two
    /// pieces of the same line — a reader's job, not this one's.
    pub fn debug(&self) {
        let history = self.inner.history.lock();
        for chunk in history.chunks.iter() {
            for data in chunk.iter_data() {
                match data {
                    // A piece that runs on marks itself, so a line the room
                    // cut in two does not read as two lines that were.
                    LogChunkData::Partial(text) => println!("{text} ⏎"),
                    LogChunkData::Line(text) => println!("{text}"),
                }
            }
        }
    }
}

/// The shared half of a [`Log`], held by an `Arc` the writers only ever see
/// weakly.
///
/// Two stages on purpose. Writers could take the mutex directly, but they are
/// reader tasks on hot pipes and the mutex is also held by whatever is
/// painting the screen; instead they push into a lock free queue and a single
/// background task moves chunks from there into the ring. So a writer's cost
/// is one uncontended swap, contention on the mutex stays between the sync
/// task and the display, and the ring keeps a single writer.
/// What one lock covers: the chunks, and how many there have ever been.
///
/// The two are one thing because they have to agree. A reader takes the count
/// to know what it has missed and the ring to fetch it, and a count read a
/// moment apart from the ring it describes would send it looking for chunks
/// that had already gone by.
///
/// Keeping the count here also makes it free to keep: a writer holding the
/// lock owns this memory outright, so bumping it is an ordinary increment
/// rather than an atomic one — measured at 1.47x the whole push path.
struct LogHistory {
    /// The last `capacity` chunks, oldest first.
    chunks: LocalRingBuffer<LogChunk>,
    /// How many chunks have ever been pushed. Never resets, so it is what a
    /// reader compares its own position against to learn what it has missed.
    ///
    /// The ring's own offsets cannot do this: it normalises them to keep them
    /// small, so they run backwards and mean nothing across time.
    pushed: u64,
}

struct LogInner {
    /// Every chunk's bytes, in one allocation. The ring, the queue and each
    /// reader's working chunk all draw from here, which is what lets a chunk
    /// move between them by swapping an index.
    arena: Arena,
    /// The history and how much of it has ever gone by.
    ///
    /// Writers and readers share this lock, so the rule that keeps them out
    /// of each other's way is that nothing slow happens under it: a writer
    /// holds it for one swap, and a reader must copy out and let go rather
    /// than work in place.
    history: Mutex<LogHistory>,
    /// Hands out the next [`LogWriterId`]. Only ever incremented, so an id
    /// is never reused even after a writer is gone — a stale stamp in the
    /// ring keeps meaning the writer it always meant.
    next_writer_id: AtomicU32,
    /// A copy of [`LogHistory::pushed`] a reader can see without the lock.
    ///
    /// The count itself lives under the mutex, where a writer already has
    /// exclusive memory and can bump it for nothing. This exists only so a
    /// reader with nothing to do can find that out with one load instead of
    /// taking the lock — the common case for a log nobody is writing to.
    ///
    /// Never ahead of the real count, since it is written while the lock is
    /// held. Seen from outside it can lag behind, which costs a reader one
    /// poll: it looks again shortly and finds the change then.
    pushed_hint: AtomicU64,
    /// Chunks handed back by readers that have finished, waiting for the
    /// next reader. Small — it never holds more than the number of readers
    /// that have ever run at once — and touched only when a reader starts or
    /// stops, so a plain lock costs nothing here.
    chunks_free: Mutex<Vec<LogChunk>>,
}

impl LogInner {
    /// Build the shared state.
    ///
    /// Nothing is spawned and nothing is cyclic: writers put their chunks
    /// into the ring themselves, so there is no background task that has to
    /// reach back at the thing being constructed. That also means a log can
    /// be built anywhere, with or without a runtime under it.
    ///
    /// # Panics
    ///
    /// If `capacity` is zero.
    fn new(capacity: usize) -> Arc<Self> {
        // One allocation for every chunk the log can hold at once: the ring
        // and the working chunk of each reader.
        let arena = Arena::new(capacity + WRITER_CHUNK_BLOCKS);
        Arc::new(LogInner {
            history: Mutex::new(LogHistory {
                chunks: LocalRingBuffer::new_with(capacity, || {
                    let block = arena
                        .alloc()
                        .expect("the arena was sized to hold the whole ring");
                    LogChunk::new(block)
                }),
                pushed: 0,
            }),
            arena,
            chunks_free: Mutex::new(Vec::new()),
            next_writer_id: AtomicU32::new(0),
            pushed_hint: AtomicU64::new(0),
        })
    }

    /// Claim the next writer id.
    fn next_writer_id(&self) -> LogWriterId {
        LogWriterId::new(self.next_writer_id.fetch_add(1, Ordering::Relaxed))
    }
}

/// A private copy of a log's recent output, synced on demand.
///
/// Reading straight from the log means holding its lock, and the lock is also
/// what every writer of the unit needs to hand a chunk over. A view that
/// repainted from the ring would stall them for the length of a repaint,
/// sixty times a second. So a reader keeps its own copy and only goes to the
/// log for what it has not seen.
///
/// # Why the same capacity as the log
///
/// A reader holds exactly as many chunks as the log does, which makes the
/// relationship between them simple enough to state in one line: **caught up,
/// a reader holds exactly what the log holds**.
///
/// That falls out of the arithmetic. Behind by `capacity` or less, the sync
/// copies what is missing and the reader mirrors the log again. Behind by
/// more, the log has already discarded what the reader never saw — and it
/// discarded it because the ring did what a ring does, not because the reader
/// was slow. So the sync simply takes the whole ring and the reader mirrors
/// it once more.
///
/// There is therefore no hole to represent. A reader's contents are always a
/// contiguous run, and it never lacks anything the log still has. That is
/// what having the two the same size buys, and it is why nothing here counts
/// what was lost: losing the oldest chunks is a log's normal operation, and a
/// view showing the newest output is showing what it should.
///
/// # What it does not do yet
///
/// Nothing reads it back. Joining pieces into lines, walking a window,
/// rendering — none of that is here; this is the copy and the bookkeeping to
/// keep it honest.
pub struct LogReader {
    /// The log to sync from. Weak, so a view left open does not keep a unit's
    /// log alive; once it is gone the reader simply stops changing, which is
    /// the right thing for a view of a process that has ended.
    inner: Weak<LogInner>,
    /// The copy, and every byte of it: the chunks hold their bytes inline, so
    /// this one `Box<[LogReaderChunk]>` is the reader's whole storage. Same
    /// capacity as the log's ring — see the type docs.
    chunks: LocalRingBuffer<LogReaderChunk>,
    /// How many chunks the log had pushed when this last synced.
    ///
    /// Compared against the log's own count, which never resets, so the
    /// difference is exactly what has arrived since. Starts at zero rather
    /// than at the log's current count, so a new reader's first sync picks up
    /// the history that is already there.
    seen: u64,
}

impl LogReader {
    /// Build a reader for `inner`, holding `capacity` chunks of its own.
    ///
    /// Takes all of its memory here and never asks for more: the ring builds
    /// every slot at once and the chunks carry their bytes inside them, so
    /// this is one allocation and the only one a reader ever makes.
    fn new(inner: Weak<LogInner>, capacity: usize) -> Self {
        Self {
            inner,
            chunks: LocalRingBuffer::new_with(capacity, LogReaderChunk::new),
            seen: 0,
        }
    }

    /// Copy across whatever the log has that this has not.
    ///
    /// Returns how many chunks were taken, which is zero whenever there was
    /// nothing new — and that case costs a single atomic load, with the lock
    /// never touched. A log nobody is writing to is free to poll.
    ///
    /// When there is something, the lock is held for the copy and nothing
    /// else: no decoding, no allocation, no joining. The longest it is ever
    /// held is a reader catching up from further behind than the ring is
    /// long, which copies the whole ring.
    pub fn sync(&mut self) -> usize {
        let Some(inner) = self.inner.upgrade() else {
            return 0;
        };
        // Acquire pairs with the release on the writer's side: if the count
        // has moved, the chunk behind it is already in the ring.
        if inner.pushed_hint.load(Ordering::Acquire) == self.seen {
            return 0;
        }

        let history = inner.history.lock();
        // The real count, under the lock where it and the ring agree. The
        // hint outside may already be behind it again.
        let pushed = history.pushed;
        let behind = pushed - self.seen;

        // Further behind than the ring is long means the rest is gone from
        // the log too, so the newest `len` chunks are everything there is.
        let take = behind.min(history.chunks.len() as u64) as usize;

        // The newest `take`, oldest first. Taken through the slice pair
        // rather than by skipping the iterator: skipping would walk the whole
        // ring to reach the few chunks that are usually new, which is the
        // common case and the one that must stay cheap.
        let (head, tail) = history.chunks.as_slices();
        let from_tail = take.min(tail.len());
        let from_head = take - from_tail;
        for chunk in head[head.len() - from_head..]
            .iter()
            .chain(&tail[tail.len() - from_tail..])
        {
            self.chunks.push().copy_from(chunk);
        }

        self.seen = pushed;
        take
    }

    /// Walk the history a piece at a time, oldest first.
    ///
    /// Fetches first, so a render loop is one call and never shows a frame
    /// staler than it had to be. The name carries the `&mut` rather than
    /// leaving it to be discovered — which is also why there is no bare
    /// `iter` here: between two walks that differ in exactly one thing, the
    /// pair is clearer than a default and an exception.
    ///
    /// [`iter_unsync`](LogReader::iter_unsync) is the other half.
    ///
    /// Each step is a [`LogReaderRef`]: the text to put out, whether a line
    /// ends after it, and which chunk it came from. So the whole of a plain
    /// render is
    ///
    /// ```ignore
    /// for piece in reader.iter_sync() {
    ///     piece.print();
    /// }
    /// ```
    ///
    /// and nothing has to be joined, buffered or allocated to get there. A
    /// line the chunk size cut in two arrives as two pieces, the first of
    /// them saying "no newline", and the terminal puts it back together by
    /// simply not breaking.
    ///
    /// # When a piece says no newline
    ///
    /// Only when the line really does carry on *in the very next piece*: the
    /// chunk has to be one the room cut short, and the chunk after it has to
    /// belong to the same writer.
    ///
    /// That second condition is the one that matters. A log takes from every
    /// process of a unit, so another writer's chunk can land between the two
    /// halves of a split line. Printing sequentially there is a choice
    /// between two flawed outputs, and this picks the lesser: the split line
    /// is broken where it should not be, rather than having a stranger's line
    /// run into the middle of it and vanish as a line of its own.
    ///
    /// A caller that wants split lines whole even then has to buffer per
    /// writer, which is more than a `print!` can do and more than most views
    /// need.
    pub fn iter_sync(&mut self) -> LogReaderIter<'_> {
        self.sync();
        self.iter_unsync()
    }

    /// Walk what the reader already holds, without going to the log.
    ///
    /// The same walk over the same pieces as
    /// [`iter_sync`](LogReader::iter_sync); the only difference is that it
    /// fetches nothing first, so it takes `&self` and any number of walks may
    /// be alive at once.
    ///
    /// For a view rendering the copy it already has — repainting after a
    /// resize, drawing the same frame twice — and for anything that must not
    /// touch the log's lock at that moment.
    ///
    /// The `unsync` is about [`sync`](LogReader::sync) and nothing else: it
    /// does not mean what an `unsync` module usually means elsewhere.
    pub fn iter_unsync(&self) -> LogReaderIter<'_> {
        let (head, tail) = self.chunks.as_slices();
        LogReaderIter {
            head,
            tail,
            pos: (0, 0),
            end: (head.len() + tail.len(), 0),
        }
    }

    /// How many chunks the reader holds.
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Whether it holds nothing — a reader that has never synced, or one for
    /// a log that never had anything.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// How many chunks it can hold, which is also how many the log can.
    pub fn capacity(&self) -> usize {
        self.chunks.capacity()
    }

    /// How many chunks the log had pushed as of the last sync.
    ///
    /// Not how many this holds: a reader that has been running longer than
    /// the ring is long has seen far more than it kept.
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// The chunks it holds, oldest first.
    #[cfg(test)]
    fn chunks(&self) -> &LocalRingBuffer<LogReaderChunk> {
        &self.chunks
    }
}

/// Walks a [`LogReader`]'s pieces, oldest first.
///
/// Holds the ring's two slices rather than its iterator because it has to
/// look one chunk ahead to answer whether a line ends — and an iterator that
/// can peek past its own position is more machinery than an index.
pub struct LogReaderIter<'a> {
    /// From the oldest chunk to the end of the ring's array.
    head: &'a [LogReaderChunk],
    /// Whatever wrapped past the end, or nothing if the contents do not.
    tail: &'a [LogReaderChunk],
    /// Where the walk is.
    pos: LogReaderPos,
    /// Where it stops, exclusive.
    ///
    /// A walk needs an end as well as a start, because a window of the
    /// history can stop short of the newest line —
    /// [`tail_range`](LogReaderIter::tail_range) with a range that does not
    /// begin at 0 is exactly that, and a view scrolled up is why it exists.
    /// For a walk of everything it is `(chunks, 0)`: one past the last chunk.
    end: LogReaderPos,
}

/// Somewhere in a walk: which chunk, and which piece of it.
///
/// A pair rather than two fields because the two numbers are never useful
/// apart — every place that reads one reads the other — and because being a
/// pair is what makes `<` mean what the walk needs it to mean. Tuples compare
/// the chunk first and only then the piece, which is exactly how one position
/// comes before another here, so the bound test in
/// [`next`](LogReaderIter::next) is the comparison you would write for a pair
/// of numbers rather than a spelled out pair of cases.
///
/// The chunk counts across `head` then `tail`, the way
/// [`at`](LogReaderIter::at) resolves it.
type LogReaderPos = (usize, usize);

impl<'a> LogReaderIter<'a> {
    /// Chunk `n`, counting across both slices, or `None` past the end.
    fn at(&self, n: usize) -> Option<&'a LogReaderChunk> {
        self.head
            .get(n)
            .or_else(|| self.tail.get(n - self.head.len()))
    }

    /// How many chunks there are to walk across, both slices together.
    fn chunks(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    /// Whether a line ends after piece `piece` of chunk `index`.
    ///
    /// The one question both directions have to answer the same way — the
    /// forward walk reports it as [`newline`](LogReaderRef::newline), and the
    /// backward one counts it to find where a line began. Two copies of the
    /// rule would be two chances for the windows to disagree with the render.
    ///
    /// The rule itself is in [`LogReader::iter_sync`]: the line carries on
    /// only if the room cut this chunk short *and* the next chunk is the same
    /// writer's. Which is also why the last piece of the last chunk always
    /// ends a line — there is no next chunk to carry it — and so why counting
    /// backwards from the end never starts inside an unterminated line.
    fn ends_line(&self, index: usize, piece: usize, chunk: &LogReaderChunk) -> bool {
        let last = piece + 1 == chunk.count();
        !(last
            && chunk.continues()
            && self
                .at(index + 1)
                .is_some_and(|next| next.writer() == chunk.writer()))
    }

    /// Narrow the walk to a window of the newest lines, counted back from the
    /// last one.
    ///
    /// `lines` is a range of *distances from the end*, not positions: `0` is
    /// the last line, `1` the one before it. So `0..20` is the last twenty
    /// lines, and `10..20` is the ten before those — which is what a view
    /// scrolled ten lines up wants. What comes out is still oldest first,
    /// like every other walk here; only the window moves.
    ///
    /// # Why it narrows a walk rather than starting one
    ///
    /// Because then it composes with both of them, and the caller keeps
    /// saying which it meant:
    ///
    /// ```ignore
    /// reader.iter_sync().tail(20, 64)              // fetch, then the last 20 lines
    /// reader.iter_unsync().tail_range(10..20, 64)  // another window of the same copy
    /// ```
    ///
    /// The same pair on [`LogReader`] would have had to pick one of the two
    /// and then grow a twin for the other. Building an iterator is two slices
    /// and four numbers, so there is nothing to save by folding the fetch
    /// into it — and the question of whether to sync stays where it already
    /// had an answer.
    ///
    /// # `max_chunks`
    ///
    /// A ceiling on how far back it will look, in chunks. Finding where a
    /// line begins means walking backwards until the newlines have been
    /// counted, and with no bound that walk is the whole ring — which for a
    /// log whose lines are mostly blank is a lot of work to render twenty of
    /// them.
    ///
    /// So it is a budget, and a view sets it from what it could possibly
    /// draw: a pane `h` rows tall cannot show more than `h` chunks' worth of
    /// pieces, whatever the lines in them look like.
    ///
    /// Hitting it truncates rather than fails — the walk simply starts at the
    /// oldest chunk it was allowed to reach, so the first line may come out
    /// as its tail rather than whole, exactly as it would if the log had
    /// discarded the rest. Pass [`usize::MAX`] for no ceiling.
    ///
    /// # Fewer lines than asked for
    ///
    /// Not an error and not distinguishable in the result: a log with three
    /// lines asked for twenty gives three. A view that needs to know whether
    /// there is more above it can ask for one line more than it means to
    /// draw, and see whether it got it.
    ///
    /// A range that starts past everything held is the other half of that,
    /// and it comes back empty rather than clamped — a view scrolled above
    /// the top of the history is showing nothing, not showing the oldest
    /// lines a second time.
    ///
    /// # How the ends are found
    ///
    /// Walking backwards and counting the pieces that end a line, let `n` be
    /// the count so far at some position. Every position from there to the
    /// end spans exactly `n` whole lines — whole, because the very last piece
    /// always ends one, so there is never a dangling remainder at the far
    /// end.
    ///
    /// The line at distance `k` from the end therefore *begins* at the
    /// earliest position whose count is `k + 1`, which is what the two
    /// assignments below capture: keep overwriting while the count sits on
    /// the number wanted, and stop once it goes past. The same expression
    /// gives both ends — the start from `lines.end`, the exclusive end from
    /// `lines.start` — since a line's start is the previous line's end.
    ///
    /// `lines.start` of 0 has no such position, because no position counts
    /// zero: it means the window runs to the newest piece there is, which is
    /// the initial value rather than anything the walk finds.
    pub fn tail_range(mut self, lines: Range<usize>, max_chunks: usize) -> Self {
        let chunks = self.chunks();
        if lines.is_empty() {
            return self.empty();
        }

        let floor = chunks.saturating_sub(max_chunks);
        // Where the window begins. Left at the oldest piece the ceiling
        // allows, which is what a log holding fewer lines than were asked for
        // falls back to: give what there is rather than nothing.
        let mut start = (floor, 0);
        // Where it ends, and unknown until the walk finds it — unless nothing
        // is being skipped, in which case it ends at the newest piece there
        // is, and no position the walk could reach would say so.
        let mut end = (lines.start == 0).then_some((chunks, 0));

        let mut count = 0;
        'walk: for index in (floor..chunks).rev() {
            let Some(chunk) = self.at(index) else { break };
            for piece in (0..chunk.count()).rev() {
                if self.ends_line(index, piece, chunk) {
                    count += 1;
                }
                if count > lines.end {
                    break 'walk;
                }
                if count == lines.end {
                    start = (index, piece);
                }
                if lines.start > 0 && count == lines.start {
                    end = Some((index, piece));
                }
            }
        }

        // Never reached the line the window ends at, so the whole window is
        // older than anything the reader holds. Which is not the same as the
        // *start* going unfound, where part of the window is still here and
        // gets truncated to it — hence one of them clamps and the other does
        // not.
        let Some(end) = end else {
            return self.empty();
        };

        self.pos = start;
        self.end = end;
        self
    }

    /// The last `lines` lines: [`tail_range`](LogReaderIter::tail_range) over
    /// `0..lines`.
    pub fn tail(self, lines: usize, max_chunks: usize) -> Self {
        self.tail_range(0..lines, max_chunks)
    }

    /// Park the walk on its own end, so it yields nothing.
    fn empty(mut self) -> Self {
        self.end = (self.chunks(), 0);
        self.pos = self.end;
        self
    }
}

/// One step of a walk: some text, whether a line ends after it, and which
/// chunk it came from.
///
/// A tuple would carry the first two, but a view wants the third — to group
/// pieces, to anchor a scroll, to know which chunk a click landed on — and a
/// three field tuple at every call site is worse than a name.
pub struct LogReaderRef<'a> {
    /// The text of this piece. Never owns anything: it borrows the reader's
    /// copy, which is why rendering allocates nothing.
    data: &'a str,
    /// Whether a line ends after this piece — see [`LogReader::iter_sync`] for
    /// when it does not.
    newline: bool,
    /// Which chunk it came from, counted from the oldest the reader holds.
    ///
    /// A position in the window, not an identity: the window slides as older
    /// chunks fall out, so the same chunk answers a smaller number after the
    /// next sync. A view that needs a handle that survives that can build one
    /// from [`seen`](LogReader::seen) — `seen - len + index` counts from the
    /// beginning of the log instead, and never repeats.
    index: usize,
}

impl<'a> LogReaderRef<'a> {
    /// The text of this piece.
    pub fn as_str(&self) -> &'a str {
        self.data
    }

    /// Whether a line ends after it.
    pub fn newline(&self) -> bool {
        self.newline
    }

    /// Which chunk it came from, counted from the oldest held.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Put it out, breaking the line or not as it says.
    ///
    /// The whole of a plain renderer: `for piece in reader.iter_sync() {
    /// piece.print() }` reproduces the output as the process wrote it, with
    /// nothing joined or allocated on the way.
    pub fn print(&self) {
        if self.newline {
            println!("{}", self.data);
        } else {
            print!("{}", self.data);
        }
    }
}

impl<'a> Iterator for LogReaderIter<'a> {
    type Item = LogReaderRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Both are positions, so this is the comparison a pair of
            // numbers gets. It comes first because a window may stop part way
            // through a chunk that has more pieces in it.
            if self.pos >= self.end {
                return None;
            }

            let (index, piece) = self.pos;
            let chunk = self.at(index)?;
            // A chunk with nothing left to give; move on. Pushed chunks are
            // never empty, so this is a step rather than a loop in practice.
            if piece >= chunk.count() {
                self.pos = (index + 1, 0);
                continue;
            }

            let text = chunk.get_str(piece)?;
            let newline = self.ends_line(index, piece, chunk);

            self.pos = (index, piece + 1);
            return Some(LogReaderRef {
                data: text,
                newline,
                index,
            });
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::log::{LogBuffer, chunk::LOG_CHUNK_SIZE, line::LOG_LINE_SIZE};

    /// A log with `lines` lines already in it.
    ///
    /// Written in one go on purpose: a buffer hands its chunk over at the end
    /// of every call, so writing line by line would put each on a chunk of
    /// its own and there would be no packing to see. One call is the burst
    /// case, and two lines to a chunk makes the counts below predictable.
    fn log_with(capacity: usize, lines: usize) -> Log {
        let log = Log::new(capacity);
        let mut buffer = LogBuffer::new(log.writer());
        let text: String = (0..lines).map(|n| format!("linha {n}\n")).collect();
        buffer.write(text.as_bytes());
        log
    }

    /// Every line the reader holds, joined and in order.
    ///
    /// The reader exposes no reading yet, so this reaches past it — which is
    /// exactly what a test of the copying should do: it checks that the bytes
    /// and the shape arrived, not that something can render them.
    fn texts(reader: &LogReader) -> Vec<String> {
        reader
            .chunks()
            .iter()
            .filter(|chunk| !chunk.is_empty())
            .map(|chunk| format!("{}:{}", chunk.writer().index(), chunk.len()))
            .collect()
    }

    /// What a renderer does, collected instead of printed.
    fn render(reader: &LogReader) -> String {
        let mut out = String::new();
        for piece in reader.iter_unsync() {
            out.push_str(piece.as_str());
            if piece.newline() {
                out.push('\n');
            }
        }
        out
    }

    #[test]
    fn iter_gives_back_the_lines_that_went_in() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write(b"um\ndois\ntres\n");

        let mut reader = log.reader();
        reader.sync();
        assert_eq!(render(&reader), "um\ndois\ntres\n");
    }

    /// A line the chunk size cut in two arrives as two pieces, and the first
    /// says not to break — so a `print!` puts it back together with nothing
    /// buffered.
    #[test]
    fn iter_does_not_break_a_line_the_room_cut() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());
        let longa = "L".repeat(LOG_CHUNK_SIZE + 40);
        buffer.write(format!("{longa}\ndepois\n").as_bytes());

        let mut reader = log.reader();
        reader.sync();

        // More pieces than lines, and still the same text.
        assert!(reader.iter_unsync().count() > 2);
        assert_eq!(render(&reader), format!("{longa}\ndepois\n"));
        // Exactly one piece says "carry on".
        assert_eq!(reader.iter_unsync().filter(|p| !p.newline()).count(), 1);
    }

    /// With another process's chunk between the halves of a split line, a
    /// sequential render cannot have both. It breaks the split line rather
    /// than letting the stranger's line run into the middle of it — the
    /// second would lose a line, the first only wraps one.
    #[test]
    fn iter_breaks_a_split_line_rather_than_splicing_another_writers_into_it() {
        let log = Log::new(16);
        let mut longo = LogBuffer::new(log.writer());
        let mut curto = LogBuffer::new(log.writer());

        let grande = "L".repeat(LOG_CHUNK_SIZE + 40);
        // Written in pieces so the other writer can land between them.
        longo.write(grande.as_bytes());
        curto.write(b"do outro\n");
        longo.write(b"FIM\n");

        let mut reader = log.reader();
        reader.sync();

        let saida = render(&reader);
        assert!(
            saida.contains("do outro\n"),
            "the other writer's line was swallowed: {saida:?}"
        );
        assert!(
            !saida.contains("Ldo outro"),
            "another writer's line ran into the middle of a split one"
        );
    }

    /// The last piece of the newest chunk has nothing after it to carry the
    /// line on, so it ends one — a line still being written shows rather than
    /// waiting for its newline.
    ///
    /// It takes more than the line buffer to get here: a shorter unterminated
    /// line never leaves the buffer at all, so the log would have nothing.
    #[test]
    fn iter_closes_the_line_at_the_end_of_what_it_has() {
        let log = Log::new(64);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write(&"m".repeat(LOG_LINE_SIZE + 8).into_bytes());

        let mut reader = log.reader();
        reader.sync();
        assert!(!reader.is_empty(), "nothing reached the log");
        assert!(
            reader
                .iter_unsync()
                .last()
                .is_some_and(|piece| piece.newline()),
            "the last piece left the line hanging"
        );
    }

    /// The index says which chunk a piece came from, so a view can group them
    /// — pieces of one chunk share it, and it only ever goes up.
    #[test]
    fn iter_reports_which_chunk_each_piece_came_from() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());
        // Four lines pack two to a chunk.
        buffer.write(b"um\ndois\ntres\nquatro\n");

        let mut reader = log.reader();
        reader.sync();

        let seen: Vec<_> = reader
            .iter_unsync()
            .map(|piece| (piece.index(), piece.as_str().to_string()))
            .collect();
        assert_eq!(
            seen,
            [
                (0, "um".to_string()),
                (0, "dois".to_string()),
                (1, "tres".to_string()),
                (1, "quatro".to_string()),
            ]
        );
    }

    /// `iter_unsync` shows only what has been fetched, so a reader that never
    /// synced has nothing — which is the whole difference between the two.
    #[test]
    fn iter_unsync_shows_only_what_was_already_fetched() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write(b"algo\n");
        assert_eq!(log.reader().iter_unsync().count(), 0);
    }

    /// And `iter_sync` fetches first, so it never needs syncing by hand.
    #[test]
    fn iter_sync_fetches_before_it_walks() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write(b"um\ndois\n");

        let mut reader = log.reader();
        assert_eq!(reader.iter_sync().count(), 2, "it walked without fetching");

        // And it keeps up without being asked again.
        buffer.write(b"tres\n");
        assert_eq!(reader.iter_sync().count(), 3);
        assert_eq!(render(&reader), "um\ndois\ntres\n");
    }

    #[test]
    fn a_new_reader_is_empty_and_behind() {
        let log = log_with(16, 4);
        let reader = log.reader();
        assert_eq!(reader.seen(), 0);
        assert!(texts(&reader).is_empty(), "it looked before being asked");
    }

    /// The first sync picks up the history that was already there — a view
    /// opened on a process that has been running shows what it has said.
    #[test]
    fn the_first_sync_takes_what_was_already_there() {
        let log = log_with(16, 4);
        let mut reader = log.reader();

        // Two lines per chunk, so four lines is two chunks.
        assert_eq!(reader.sync(), 2);
        assert_eq!(reader.len(), 2);
        assert_eq!(reader.seen(), 2);
    }

    /// And nothing costs nothing: with no change there is no lock and no
    /// copy, which is what makes polling a quiet log cheap.
    #[test]
    fn syncing_twice_over_takes_nothing_the_second_time() {
        let log = log_with(16, 4);
        let mut reader = log.reader();
        reader.sync();

        assert_eq!(reader.sync(), 0);
        assert_eq!(reader.sync(), 0);
        assert_eq!(reader.len(), 2, "it copied something anyway");
    }

    /// Only the delta crosses, not the whole ring.
    #[test]
    fn a_later_sync_takes_only_what_arrived_since() {
        let log = log_with(16, 8);
        let mut reader = log.reader();
        assert_eq!(reader.sync(), 4);

        let mut buffer = LogBuffer::new(log.writer());
        buffer.write(b"nova\noutra\n");

        assert_eq!(reader.sync(), 1, "it re-copied chunks it already had");
        assert_eq!(reader.len(), 5);
    }

    /// Caught up, a reader holds exactly what the log holds. That is the
    /// whole point of the two rings being the same size.
    #[test]
    fn caught_up_it_mirrors_the_log() {
        let log = log_with(8, 6);
        let mut reader = log.reader();
        reader.sync();

        // Six lines is three chunks, all of which fit — so what it holds is
        // everything the log ever pushed, which is what mirroring means while
        // nothing has been evicted yet.
        assert_eq!(reader.seen(), 3);
        assert_eq!(reader.len(), reader.seen() as usize);
        assert_eq!(reader.capacity(), 8);
    }

    /// Behind by more than the ring is long, the rest is gone from the log
    /// too — so the sync takes the whole ring and the reader mirrors it
    /// again. There is no hole, because there is nothing the log still has
    /// that the reader lacks.
    #[test]
    fn falling_far_behind_leaves_it_mirroring_the_newest() {
        let log = Log::new(4);
        let mut reader = log.reader();

        let mut buffer = LogBuffer::new(log.writer());
        // Far more lines than the ring can hold, in one burst.
        let text: String = (0..200).map(|n| format!("linha {n}\n")).collect();
        buffer.write(text.as_bytes());

        let taken = reader.sync();
        assert_eq!(taken, 4, "it should take the ring, not the backlog");
        assert_eq!(reader.len(), 4);
        assert_eq!(reader.seen(), 100, "the count is of what the log pushed");
    }

    /// The copy carries the shape, not just the bytes: how many pieces, how
    /// long, and who wrote them.
    #[test]
    fn the_copy_carries_the_shape() {
        let log = Log::new(16);
        let mut um = LogBuffer::new(log.writer());
        let mut dois = LogBuffer::new(log.writer());
        um.write(b"aa\nbb\n");
        dois.write(b"cccc\n");

        let mut reader = log.reader();
        reader.sync();

        let chunks: Vec<_> = reader.chunks().iter().filter(|c| !c.is_empty()).collect();
        assert_eq!(chunks.len(), 2);
        assert_eq!((chunks[0].count(), chunks[0].len()), (2, 4));
        assert_eq!(chunks[0].writer().index(), 0);
        assert_eq!((chunks[1].count(), chunks[1].len()), (1, 4));
        assert_eq!(chunks[1].writer().index(), 1);
    }

    /// A reader does not hold its log open: a view left behind on a unit that
    /// is gone stops changing instead of keeping its memory alive.
    #[test]
    fn a_reader_does_not_keep_the_log_alive() {
        let log = log_with(16, 4);
        let mut reader = log.reader();
        reader.sync();
        let before = reader.len();

        drop(log);
        assert_eq!(reader.sync(), 0);
        assert_eq!(reader.len(), before, "it lost what it had already copied");
    }

    /// All of a reader's bytes are in the ring's one allocation, laid out in
    /// order — which is the reason the chunks hold them inline.
    #[test]
    fn the_whole_reader_is_one_run_of_memory() {
        let log = log_with(8, 32);
        let mut reader = log.reader();
        reader.sync();
        assert_eq!(reader.len(), 8, "the ring should be full for this");

        let stride = std::mem::size_of::<crate::log::chunk::LogReaderChunk>();
        assert!(stride >= LOG_CHUNK_SIZE, "the bytes are not inline");

        let mut addresses: Vec<usize> = reader
            .chunks()
            .iter()
            .map(|chunk| chunk as *const _ as usize)
            .collect();
        addresses.sort_unstable();
        for pair in addresses.windows(2) {
            assert_eq!(pair[1] - pair[0], stride, "the chunks are not one run");
        }
    }

    /// The lines a window gives back, joined out of its pieces.
    ///
    /// A split line arrives as several pieces and is one line here, which is
    /// the whole point of counting by `newline` rather than by piece. A
    /// trailing run with no newline is kept too — a window the ceiling cut
    /// part way through a line is supposed to show the tail of it.
    fn window(reader: &LogReader, lines: Range<usize>, max_chunks: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut line = String::new();
        for piece in reader.iter_unsync().tail_range(lines, max_chunks) {
            line.push_str(piece.as_str());
            if piece.newline() {
                out.push(std::mem::take(&mut line));
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
        out
    }

    /// A reader holding `lines` numbered lines, all of them still in the ring.
    fn tail_reader(lines: usize) -> LogReader {
        let log = log_with(64, lines);
        let mut reader = log.reader();
        reader.sync();
        reader
    }

    #[test]
    fn tail_gives_back_the_newest_lines_oldest_first() {
        let reader = tail_reader(10);
        assert_eq!(
            window(&reader, 0..3, usize::MAX),
            ["linha 7", "linha 8", "linha 9"]
        );
    }

    /// `tail` is the range starting at zero, and has to stay that way rather
    /// than becoming a second implementation of the same walk.
    #[test]
    fn tail_is_tail_range_from_zero() {
        let reader = tail_reader(10);
        let by_range: Vec<_> = reader
            .iter_unsync()
            .tail_range(0..4, 8)
            .map(|p| p.as_str().to_owned())
            .collect();
        let by_tail: Vec<_> = reader
            .iter_unsync()
            .tail(4, 8)
            .map(|p| p.as_str().to_owned())
            .collect();
        assert_eq!(by_range, by_tail);
    }

    /// Narrowing composes with the fetching walk too — which is the reason
    /// these live on the iterator rather than on the reader.
    #[test]
    fn a_window_composes_with_the_syncing_walk() {
        let log = log_with(64, 6);
        let mut reader = log.reader();

        // Never synced, so the unsynced walk has nothing while the syncing
        // one fetches first and then narrows.
        assert_eq!(reader.iter_unsync().tail(2, usize::MAX).count(), 0);
        let lines: Vec<String> = reader
            .iter_sync()
            .tail(2, usize::MAX)
            .map(|p| p.as_str().to_owned())
            .collect();
        assert_eq!(lines, ["linha 4", "linha 5"]);
    }

    /// The range counts distance from the end, so a start past zero is a view
    /// scrolled up: the lines before the ones `0..n` would have given.
    #[test]
    fn a_range_that_starts_past_zero_skips_the_newest_lines() {
        let reader = tail_reader(10);
        assert_eq!(window(&reader, 0..2, usize::MAX), ["linha 8", "linha 9"]);
        assert_eq!(window(&reader, 2..4, usize::MAX), ["linha 6", "linha 7"]);
        assert_eq!(window(&reader, 1..3, usize::MAX), ["linha 7", "linha 8"]);
    }

    /// Asking for more than there is gives what there is. A view cannot know
    /// how much a log holds before it asks, so this is the ordinary case for
    /// a process that has just started, not an edge one.
    #[test]
    fn asking_for_more_lines_than_exist_gives_all_of_them() {
        let reader = tail_reader(3);
        assert_eq!(
            window(&reader, 0..50, usize::MAX),
            ["linha 0", "linha 1", "linha 2"]
        );
    }

    /// And a range that begins past everything held gives nothing, rather
    /// than clamping back onto the oldest lines — a view scrolled above the
    /// top of the history is showing nothing.
    #[test]
    fn a_range_that_starts_past_the_oldest_line_is_empty() {
        let reader = tail_reader(3);
        assert!(window(&reader, 10..20, usize::MAX).is_empty());
    }

    #[test]
    fn an_empty_range_walks_nothing() {
        let reader = tail_reader(10);
        assert_eq!(reader.iter_unsync().tail_range(0..0, usize::MAX).count(), 0);
        assert_eq!(reader.iter_unsync().tail_range(5..5, usize::MAX).count(), 0);
        assert_eq!(reader.iter_unsync().tail(0, usize::MAX).count(), 0);
    }

    /// An unbounded window is the plain walk — the narrowing must not be a
    /// second way of reading the same chunks.
    #[test]
    fn an_unbounded_window_is_the_plain_walk() {
        let reader = tail_reader(10);
        let whole: Vec<_> = reader
            .iter_unsync()
            .map(|p| (p.as_str().to_owned(), p.newline(), p.index()))
            .collect();
        let tail: Vec<_> = reader
            .iter_unsync()
            .tail(usize::MAX, usize::MAX)
            .map(|p| (p.as_str().to_owned(), p.newline(), p.index()))
            .collect();
        assert_eq!(whole, tail);
    }

    /// The ceiling bounds the backwards walk, so a window can come back short
    /// — which is the point: it is a render budget, not a request that can
    /// fail. Two lines to a chunk here, so two chunks is four lines.
    #[test]
    fn the_chunk_ceiling_truncates_the_window() {
        let reader = tail_reader(20);
        assert_eq!(
            window(&reader, 0..10, 2),
            ["linha 16", "linha 17", "linha 18", "linha 19"]
        );
        // Without the ceiling the same ask reaches all ten.
        assert_eq!(window(&reader, 0..10, usize::MAX).len(), 10);
    }

    /// A line the chunk size cut in two is still one line to count back over,
    /// and the window hands back every piece of it.
    #[test]
    fn a_split_line_counts_once_and_comes_back_whole() {
        let log = Log::new(64);
        let mut buffer = LogBuffer::new(log.writer());
        let longa = "L".repeat(LOG_CHUNK_SIZE + 40);
        buffer.write(format!("antes\n{longa}\ndepois\n").as_bytes());

        let mut reader = log.reader();
        reader.sync();

        assert_eq!(
            window(&reader, 0..2, usize::MAX),
            [longa.clone(), "depois".to_owned()]
        );
        assert_eq!(window(&reader, 1..2, usize::MAX), [longa]);
        assert_eq!(window(&reader, 2..3, usize::MAX), ["antes"]);
    }

    /// Hitting the ceiling part way through a line gives the tail of it, the
    /// same shape a log that had discarded the rest would give.
    #[test]
    fn the_ceiling_can_land_inside_a_line() {
        let log = Log::new(64);
        let mut buffer = LogBuffer::new(log.writer());
        let longa = "L".repeat(LOG_CHUNK_SIZE * 3);
        buffer.write(format!("{longa}\n").as_bytes());

        let mut reader = log.reader();
        reader.sync();

        let cortada = window(&reader, 0..1, 2);
        assert_eq!(cortada.len(), 1);
        let cortada = &cortada[0];
        assert!(
            cortada.len() < longa.len() && longa.ends_with(cortada.as_str()),
            "expected the tail of the line, got {} of {}",
            cortada.len(),
            longa.len()
        );
    }

    /// The index a piece reports is its place in the reader's window, so a
    /// narrowed walk has to report the same numbers the plain one does rather
    /// than counting from wherever it happened to start.
    #[test]
    fn a_window_reports_the_indexes_of_the_whole_reader() {
        let reader = tail_reader(10);
        let last = reader
            .iter_unsync()
            .last()
            .expect("there is output")
            .index();
        assert_eq!(
            reader
                .iter_unsync()
                .tail(1, usize::MAX)
                .map(|p| p.index())
                .last(),
            Some(last)
        );
    }
}
