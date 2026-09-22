use std::num::NonZeroU32;
use std::ops::Range;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicU32, AtomicU64, Ordering},
};

use crate::log::line::LOG_LINE_SIZE;
use crate::{
    log::{
        LogBuffer,
        chunk::{LOG_CHUNK_SIZE, LogChunk, LogChunkData, LogReaderChunk},
    },
    util::{
        arena::{Arena, ArenaBlock},
        localring::LocalRingBuffer,
    },
};
use anstyle_parse::{Parser, Perform};
use parking_lot::Mutex;
use smallvec::SmallVec;
use tokio::{io::AsyncRead, task::JoinHandle};
use unicode_width::UnicodeWidthChar;

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
        inner.version.fetch_add(1, Ordering::Release);
        // Outside the lock: the displaced chunk is ours alone now, and
        // clearing it is nobody else's business.
        chunk.clear();
        true
    }

    /// Publish `text` as the line this writer is part way through.
    ///
    /// Called at the end of a read that did not finish a line, so that a
    /// process printing a prompt, a progress bar or a `Compiling ...` is
    /// visible while it is doing it rather than only once it says something
    /// else. Without this such a line waits in the reader task for a newline
    /// that may be a minute away, or never come.
    pub(super) fn set_partial(&self, text: &str) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut partials = inner.partials.lock();
        if partials.is(self.id, text) {
            return;
        }
        let version = inner.version.fetch_add(1, Ordering::Release) + 1;
        partials.set(self.id, version, text);
    }

    /// Forget whatever this writer was part way through.
    ///
    /// Two callers, and between them every way a partial line can end: the
    /// buffer when the line is finished and handed over, and the buffer's
    /// `Drop` when the reader task ends for any reason at all — a process
    /// exiting, a unit being stopped, a task aborted mid-line. The last is
    /// the one that matters, because it is the only path that does not run
    /// the code that would otherwise tidy up.
    pub(super) fn clear_partial(&self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if inner.partials.lock().clear(self.id) {
            inner.version.fetch_add(1, Ordering::Release);
        }
    }

    /// A writer for notes about this log, rather than output into it.
    ///
    /// The same log, under [`LogWriterId::NOTES`], and narrowed to the two
    /// ways in that a note has: everything else a writer can do belongs to a
    /// process's output and has no meaning for a line the manager wrote
    /// itself.
    pub fn notes(&self) -> LogWriterNotes {
        LogWriterNotes::new(self.inner.clone())
    }

    /// Write one whole line into the log.
    ///
    /// The other way in — [`consume_spawn`](LogWriterRef::consume_spawn) —
    /// reads bytes off a pipe and assembles them into lines. This is for a
    /// caller that already has the line: it goes into a chunk and the chunk
    /// goes into the ring, with nothing in between.
    pub fn write_line(&mut self, text: &str) {
        self.write_lines([text]);
    }

    /// Write several whole lines into the log.
    ///
    /// One chunk for as many of them as fit, so lines written together are
    /// stored together — and a fresh one whenever that fills, since a chunk
    /// holds a couple of lines and not a list of them.
    ///
    /// The last chunk goes whether or not it filled: a line nobody can see
    /// until the next one is a line that arrived too late to be read.
    pub fn write_lines<I>(&mut self, lines: I)
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut chunk = self.chunk();
        for line in lines {
            let mut rest = line.as_ref();
            loop {
                rest = &rest[chunk.push_line_str(rest)..];
                // Full, so it goes now and the rest of this line — if there
                // is any — carries on in the one that comes back.
                if chunk.is_finished() && !self.push_chunk(&mut chunk) {
                    self.recycle(chunk);
                    return;
                }
                if rest.is_empty() {
                    break;
                }
            }
        }
        if !chunk.is_empty() {
            self.push_chunk(&mut chunk);
        }
        self.recycle(chunk);
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

/// A way into a log for the manager's own notes.
///
/// A [`LogWriterRef`] with everything but the two line writers taken away —
/// there is no chunk to hand over, no pipe to drain, nothing to fork. What is
/// left is what a note is: a line somebody already has, going into a log among
/// output somebody else wrote.
pub struct LogWriterNotes {
    inner: LogWriterRef,
}

impl LogWriterNotes {
    /// Create the LogWriterNotes
    fn new(inner: Weak<LogInner>) -> Self {
        Self {
            inner: LogWriterRef {
                inner,
                id: LogWriterId::NOTES,
            },
        }
    }
    /// Write one whole line into the log.
    pub fn write_line(&mut self, text: &str) {
        self.inner.write_line(text);
    }

    /// Write several whole lines into the log.
    pub fn write_lines<I>(&mut self, lines: I)
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        self.inner.write_lines(lines);
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
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct LogWriterId {
    /// Dense from zero, so it doubles as an index into whatever a renderer
    /// keeps per writer. `u32::MAX` is [`UNSET`](LogWriterId::UNSET) and
    /// belongs to no writer, which is why an arena of that many writers
    /// cannot exist.
    raw: NonZeroU32,
}

impl LogWriterId {
    /// The id notes are written under.
    ///
    /// Reserved rather than minted, and not for tidiness: a line split across
    /// chunks is continued by *that writer's* next chunk, so notes sharing an
    /// id with a process could be spliced into the middle of one of its
    /// lines. An id of their own is what keeps the two apart.
    pub const NOTES: Self = Self::new(1);

    /// The stamp a chunk carries before any writer has claimed it.
    ///
    /// A chunk in the ring always carries a real id — it is stamped on the
    /// way in — so this only ever shows on one that has not been handed over
    /// yet, or on one a recycler has just reset.
    pub const UNSET: Self = Self::new(u32::MAX);

    /// Mint the id numbered `raw`. The log is the only thing that should be
    /// choosing these.
    pub(super) const fn new(raw: u32) -> Self {
        Self {
            raw: NonZeroU32::new(raw).expect("value must be > 0"),
        }
    }
    /// Mint the id numbered `raw`. The log is the only thing that should be
    /// choosing these.
    pub fn new_checked(raw: u32) -> Option<Self> {
        NonZeroU32::new(raw).map(|raw| Self { raw })
    }

    /// The number behind the id, for indexing.
    pub(crate) fn index(&self) -> usize {
        self.raw.get() as usize
    }

    /// Check if is note
    pub fn is_note(&self) -> bool {
        self.raw == Self::NOTES.raw
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

    /// A log holding `bytes` of output, rounded down to whole chunks.
    ///
    /// Bytes because that is the figure the ring keeps exactly — how many
    /// lines it buys depends on how long they turn out. One chunk is the
    /// floor, since [`new`](Log::new) panics on a log with no room and a
    /// config asking for none meant the smallest one.
    pub fn with_bytes(bytes: usize) -> Self {
        Self::new(Self::capacity_for_bytes(bytes))
    }

    /// How many chunks [`with_bytes`](Log::with_bytes) buys for `bytes`.
    ///
    /// For sizing a [`LogReader`] before there is a log to ask: a reader is
    /// held to the capacity of whatever log it is put on, so one built to
    /// stand in for several has to start at least this long.
    pub fn capacity_for_bytes(bytes: usize) -> usize {
        (bytes / LOG_CHUNK_SIZE).max(1)
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
    /// [`super::chunk::LogChunkData`].
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

    /// Open a view onto this log in a reader that already exists.
    ///
    /// [`reader`](Log::reader) without the allocation, for a screen that
    /// follows one log at a time and moves between them: a reader is a mirror
    /// of a whole log, so building one per switch allocates and zeroes the
    /// log's size again.
    ///
    /// The reader forgets the log it was on entirely — see
    /// [`reset`](LogReader::reset). It keeps its memory, so one whose ring is
    /// shorter than this log's follows a shorter tail rather than growing.
    pub fn reader_into(&self, reader: &mut LogReader) {
        reader.reset(
            Arc::downgrade(&self.inner),
            self.inner.history.lock().chunks.capacity(),
        );
    }

    /// A writer for notes about this log, rather than output into it.
    ///
    /// The same log, under [`LogWriterId::NOTES`], and narrowed to the two
    /// ways in that a note has: everything else a writer can do belongs to a
    /// process's output and has no meaning for a line the manager wrote
    /// itself.
    pub fn notes(&self) -> LogWriterNotes {
        LogWriterNotes::new(Arc::downgrade(&self.inner))
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
    /// The lines currently being written, one per writer.
    partials: Mutex<Partials>,
    /// Everything that has happened to this log, counted.
    ///
    /// Bumped by a chunk arriving and by a partial line changing, so a reader
    /// polling one number learns about both. `pushed` cannot do that job: a
    /// partial does not push a chunk, and a view watching only that would
    /// never notice a line being typed.
    version: AtomicU64,
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
            partials: Mutex::new(Partials::new()),
            version: AtomicU64::new(0),
            // 1 belongs to the notes; processes start after it.
            next_writer_id: AtomicU32::new(2),
            pushed_hint: AtomicU64::new(0),
        })
    }

    /// Claim the next writer id.
    fn next_writer_id(&self) -> LogWriterId {
        let mut n = self.next_writer_id.fetch_add(1, Ordering::Relaxed);
        while n == 0 {
            n = self.next_writer_id.fetch_add(1, Ordering::Relaxed);
        }
        LogWriterId::new(n)
    }
}

/// How many writers may have a line in flight at once.
///
/// One per process reading into the log, and a unit runs its commands one
/// after another, so in practice this is one or two. The cap is not the
/// mechanism — a partial is retired by the writer that owns it, on every path
/// there is — it is what stops a leak from growing without bound if one ever
/// gets past that.
const PARTIALS_MAX: usize = 8;

/// The text of a line still being written.
///
/// Inline and fixed, like every other piece of text this module holds: a line
/// being assembled is a `[u8; LOG_LINE_SIZE]`, a chunk is one arena block, a
/// reader's chunk is its bytes. A `String` here would have been the one thing
/// in the log that allocates per line, and the one whose type does not say
/// how long it can get — and the answer is the same [`LOG_LINE_SIZE`],
/// because past that a line stops being partial and becomes a fragment in the
/// ring.
struct PartialText {
    /// How many bytes of `bytes` are text.
    len: usize,
    /// The text, inline. Only the first `len` mean anything.
    bytes: [u8; LOG_LINE_SIZE],
}

impl PartialText {
    /// An empty one.
    fn new() -> Self {
        Self {
            len: 0,
            bytes: [0; LOG_LINE_SIZE],
        }
    }

    /// Take `text`, replacing whatever was here.
    ///
    /// Truncated to what fits, on a character boundary. The caller cannot
    /// overrun it — a line longer than this has already left the buffer as a
    /// fragment — but a bound that only holds because of something two
    /// modules away is a bound worth keeping anyway.
    fn set(&mut self, text: &str) {
        let mut len = text.len().min(LOG_LINE_SIZE);
        while len > 0 && !text.is_char_boundary(len) {
            len -= 1;
        }
        self.bytes[..len].copy_from_slice(&text.as_bytes()[..len]);
        self.len = len;
    }

    /// The text.
    fn as_str(&self) -> &str {
        // SAFETY: every byte was copied out of a `&str`, and the length was
        // moved back to a character boundary before the copy, so what is here
        // is a prefix of valid UTF-8 cut between characters.
        unsafe { std::str::from_utf8_unchecked(&self.bytes[..self.len]) }
    }
}

/// A line that is still being written.
///
/// Copied out of the reader task rather than moved, because the task is not
/// finished with it: the next read goes on adding to the same line, and this
/// is only what it looked like when the read ran out.
struct Partial {
    /// Who is writing it, or [`UNSET`](LogWriterId::UNSET) for a free slot.
    writer: LogWriterId,
    /// The log's version when it was last written, which is what decides
    /// which one goes if the store ever fills.
    version: u64,
    /// The text so far.
    text: PartialText,
}

impl Partial {
    /// A free slot.
    fn new() -> Self {
        Self {
            writer: LogWriterId::UNSET,
            version: 0,
            text: PartialText::new(),
        }
    }

    /// Whether anybody is writing into it.
    fn is_taken(&self) -> bool {
        self.writer != LogWriterId::UNSET
    }
}

/// The lines currently being written, one per writer.
///
/// # Why they are not in the ring
///
/// A chunk in the ring is permanent and never changes; a partial is neither.
/// It is replaced on every read and then disappears when the finished line
/// lands, so putting it in the ring would write the same text into the
/// history twice and leave every superseded version of it there for good.
/// Readers mirror chunks by offset, too, which a chunk that mutated would
/// break.
///
/// # Why a lock of their own
///
/// Publishing one happens on every read that does not end a line, and the
/// history's lock is the one every writer needs to hand a chunk over. Keeping
/// them apart is what stops a process printing a progress bar from getting in
/// the way of one printing lines.
struct Partials {
    /// [`PARTIALS_MAX`] slots, built once and reused. A free one carries
    /// [`UNSET`](LogWriterId::UNSET), the way a reader's ring carries empty
    /// chunks — there is nothing left to allocate after this.
    lines: Vec<Partial>,
}

impl Partials {
    /// A store with every slot built and none of them taken.
    fn new() -> Self {
        Self {
            lines: (0..PARTIALS_MAX).map(|_| Partial::new()).collect(),
        }
    }

    /// Whether `writer` is already showing exactly `text`.
    ///
    /// Asked before publishing, because a read that brings only the head of a
    /// character adds nothing to the line — and a version that moved for that
    /// would have every view re-copying its window because a pipe twitched.
    fn is(&self, writer: LogWriterId, text: &str) -> bool {
        self.lines
            .iter()
            .any(|line| line.writer == writer && line.text.as_str() == text)
    }

    /// Take `text` as the line `writer` is part way through.
    ///
    /// Into the slot it already has, or a free one, or — failing both — the
    /// one written longest ago, which is the one most likely to belong to a
    /// reader that is not coming back. That last case is a backstop and not
    /// the mechanism: a partial is retired by the writer that owns it, on
    /// every path there is.
    fn set(&mut self, writer: LogWriterId, version: u64, text: &str) {
        let slot = self
            .lines
            .iter()
            .position(|line| line.writer == writer)
            .or_else(|| self.lines.iter().position(|line| !line.is_taken()))
            .or_else(|| {
                self.lines
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, line)| line.version)
                    .map(|(index, _)| index)
            });
        let Some(slot) = slot else {
            return;
        };
        let line = &mut self.lines[slot];
        line.writer = writer;
        line.version = version;
        line.text.set(text);
    }

    /// Forget whatever `writer` was part way through.
    ///
    /// Reports whether there was anything, because a version that moved for a
    /// clear that cleared nothing would wake every view for no reason.
    fn clear(&mut self, writer: LogWriterId) -> bool {
        let Some(line) = self.lines.iter_mut().find(|line| line.writer == writer) else {
            return false;
        };
        line.writer = LogWriterId::UNSET;
        line.text.set("");
        true
    }

    /// The lines being written, oldest first.
    ///
    /// By version, because slots are reused in whatever order they came free,
    /// and a list that reorders itself under a reader is a list nobody can
    /// follow.
    ///
    /// Inline to [`PARTIALS_MAX`], the number of slots there are to sort: a
    /// `Vec` here allocated once per sync of a log with a line in flight, under
    /// the lock every writer needs.
    fn taken(&self) -> impl Iterator<Item = &Partial> {
        let mut taken: SmallVec<[&Partial; PARTIALS_MAX]> =
            self.lines.iter().filter(|line| line.is_taken()).collect();
        taken.sort_by_key(|line| line.version);
        taken.into_iter()
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
/// # How much of the log it holds
///
/// As many chunks as the log and no more, whatever its own ring was built
/// with: **caught up, a reader holds exactly what the log holds**.
///
/// That falls out of the arithmetic. Behind by the log's capacity or less,
/// the sync copies what is missing and the reader mirrors the log again.
/// Behind by more, the log has already discarded what the reader never saw —
/// and it discarded it because the ring did what a ring does, not because the
/// reader was slow. So the sync simply takes the whole ring and the reader
/// mirrors it once more.
///
/// A ring longer than the log's is the one way a hole could appear: that
/// second case would leave the chunks it kept from before the gap sitting
/// next to the ones from after it. [`reset`](LogReader::reset) is what stops
/// it, holding the ring to the log's capacity and leaving the slots past that
/// unused — which is what lets one reader stand in for logs of several sizes
/// without being rebuilt for each.
///
/// There is therefore no hole to represent. A reader's contents are always a
/// contiguous run, and it never lacks anything the log still has, which is
/// why nothing here counts what was lost: losing the oldest chunks is a log's
/// normal operation, and a view showing the newest output is showing what it
/// should.
///
/// A ring *shorter* than the log's is sound and simply a shorter tail, since
/// each sync still lands on what came before it — but see what
/// [`sync`](LogReader::sync) then returns.
pub struct LogReader {
    /// The log to sync from, or `None` for a reader that has not been put on
    /// one — [`empty`](LogReader::empty) is where those come from, and
    /// [`reset`](LogReader::reset) is what fills this in.
    ///
    /// Weak, so a view left open does not keep a unit's log alive; once it is
    /// gone the reader simply stops changing, which is the right thing for a
    /// view of a process that has ended. The two cases read the same from
    /// here on: a sync that does nothing over the chunks already copied.
    inner: Option<Weak<LogInner>>,
    /// The copy, and every byte of it: the chunks hold their bytes inline, so
    /// this one `Box<[LogReaderChunk]>` is the reader's whole storage. Held
    /// to the log's capacity and built at whatever size the reader was asked
    /// for — see the type docs.
    chunks: LocalRingBuffer<LogReaderChunk>,
    /// How many chunks the log had pushed when this last synced.
    ///
    /// Compared against the log's own count, which never resets, so the
    /// difference is exactly what has arrived since. Starts at zero rather
    /// than at the log's current count, so a new reader's first sync picks up
    /// the history that is already there.
    seen: u64,
    /// The lines that were still being written at the last sync.
    ///
    /// Copied, like the chunks, so that reading them back does not go near
    /// the log — and held the same way they are held there: [`PARTIALS_MAX`]
    /// slots built once, refilled in place. The whole point of a partial is
    /// that it changes on every read of a busy pipe, so it is the last place
    /// that should be allocating.
    partials: Vec<LogReaderPartial>,
    /// How many of those slots mean anything.
    partials_len: usize,
    /// The log's version at the last sync.
    ///
    /// Everything that has happened to it, chunks and partials together. What
    /// [`seen`](LogReader::seen) counts cannot answer for a line being typed,
    /// and a view watching only that would never redraw for one.
    version: u64,
}

impl LogReader {
    /// Every reader is built here, with or without a log to follow.
    ///
    /// Takes all of its memory now and never asks for more: the ring builds
    /// every slot at once and the chunks carry their bytes inside them, so a
    /// reader is two allocations — the ring and the partial slots — and
    /// nothing it does afterwards adds one.
    fn new_inner(inner: Option<Weak<LogInner>>, capacity: usize) -> Self {
        Self {
            inner,
            chunks: LocalRingBuffer::new_with(capacity, LogReaderChunk::new),
            seen: 0,
            partials: (0..PARTIALS_MAX).map(|_| LogReaderPartial::new()).collect(),
            partials_len: 0,
            version: 0,
        }
    }
    /// Build a reader for `inner`, holding `capacity` chunks of its own.
    fn new(inner: Weak<LogInner>, capacity: usize) -> Self {
        Self::new_inner(Some(inner), capacity)
    }

    /// A reader of `capacity` chunks that is not on a log yet.
    ///
    /// The allocation a caller makes once and then hands to
    /// [`reader_into`](Log::reader_into) for each log in turn. `capacity` is
    /// the ceiling for all of them, since a reader is held to the log it is
    /// on but never grown past its own slots — [`log_capacity`] is what a
    /// caller following a session's units sizes it by.
    ///
    /// Until it is given a log it behaves as one whose log is gone: empty,
    /// and a [`sync`](LogReader::sync) that does nothing.
    ///
    /// # Panics
    ///
    /// If `capacity` is zero, like the ring it is built on.
    ///
    /// [`log_capacity`]: crate::unit::UnitMap::log_capacity
    pub fn empty(capacity: usize) -> Self {
        Self::new_inner(None, capacity)
    }

    /// Whether it is on no log at all — `true` straight out of
    /// [`empty`](LogReader::empty), and what a caller keeping a reader per
    /// unit tests before reading one for the first time.
    pub fn is_detached(&self) -> bool {
        self.inner.is_none()
    }

    /// Point this reader at another log, keeping its memory.
    ///
    /// Everything counted here is counted against one log's chunks and means
    /// nothing against another's, so all of it goes back to where
    /// [`new`](LogReader::new) starts. `version` especially: it is a token
    /// and not a clock, so leaving one log's value against another's is a
    /// first sync that decides nothing has changed and a pane still showing
    /// the unit it was on.
    ///
    /// The ring is held to `capacity` rather than rebuilt at it, so a reader
    /// moving to a smaller log gives no memory back, and one moving to a
    /// larger log than it was built for follows its newest `max_capacity`
    /// chunks.
    ///
    /// Put back on the log it is already on it keeps everything, because
    /// everything it counted was counted against that same log and still
    /// means what it said: a pane returning to a unit shows what it had
    /// instead of copying the history again.
    fn reset(&mut self, inner: Weak<LogInner>, capacity: usize) {
        // Optimization when the inner is already the same
        if self.inner.as_ref().is_some_and(|i| i.ptr_eq(&inner)) {
            self.chunks.set_capacity(capacity);
            return;
        }
        self.inner = Some(inner);
        self.chunks.clear();
        self.chunks.set_capacity(capacity);
        self.seen = 0;
        self.partials_len = 0;
        self.version = 0;
    }

    /// Copy across whatever the log has that this has not.
    ///
    /// Returns how many chunks were taken, which is zero whenever there was
    /// nothing new — and that case costs a single atomic load, with the lock
    /// never touched. A log nobody is writing to is free to poll.
    ///
    /// Taken out of the log, which is not always kept: a reader whose ring is
    /// shorter than the log keeps the newest of them, so this can come back
    /// larger than [`len`](LogReader::len).
    ///
    /// When there is something, the lock is held for the copy and nothing
    /// else: no decoding, no allocation, no joining. The longest it is ever
    /// held is a reader catching up from further behind than the ring is
    /// long, which copies the whole ring.
    pub fn sync(&mut self) -> usize {
        let Some(inner) = self.inner.as_ref().and_then(|w| w.upgrade()) else {
            return 0;
        };
        // Acquire pairs with the release on the writer's side: if the version
        // has moved, whatever moved it is already published.
        let version = inner.version.load(Ordering::Acquire);
        if version == self.version {
            return 0;
        }
        self.version = version;

        // The lines still being written, which change far more often than
        // chunks arrive and are far cheaper to take.
        {
            let partials = inner.partials.lock();
            self.partials_len = 0;
            for partial in partials.taken() {
                let slot = &mut self.partials[self.partials_len];
                slot.writer = partial.writer;
                slot.text.set(partial.text.as_str());
                self.partials_len += 1;
            }
        }

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

    /// Walk the history a piece at a time, oldest first, fetching first.
    ///
    /// The name carries the `&mut` rather than leaving it to be discovered;
    /// [`iter_unsync`](LogReader::iter_unsync) is the other half.
    ///
    /// Each step is a [`LogReaderRef`]: the text, whether a line ends after
    /// it, and which chunk it came from. Nothing has to be joined, buffered
    /// or allocated to render — a line the chunk size cut in two arrives as
    /// two pieces, the first saying "no newline", and the terminal puts it
    /// back together by not breaking.
    ///
    /// # When a piece says no newline
    ///
    /// Only when the line really does carry on *in the very next piece*: the
    /// chunk was cut short for room, and the chunk after it belongs to the
    /// same writer.
    ///
    /// That second condition is the one that matters, because a log takes
    /// from every process of a unit and another writer's chunk can land
    /// between the halves of a split line. Both outputs are then wrong, and
    /// this picks the lesser: the split line breaks where it should not,
    /// rather than a stranger's line running into the middle of it and
    /// vanishing as a line of its own. Wanting better means buffering per
    /// writer, which is more than a `print!` can do.
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

    /// Copy a rectangle of the history into `out`, one line per entry.
    ///
    /// Bounded in both directions — `lines` of them, `columns` wide — so this
    /// costs the size of what will be drawn and not the size of what is held.
    /// That is the difference from [`iter_unsync`](LogReader::iter_unsync),
    /// which walks everything.
    ///
    /// Both ranges are half open and counted back from the newest line,
    /// through [`tail_range`](LogReaderIter::tail_range) so that one place
    /// decides where a line begins. What comes back is oldest first.
    ///
    /// Fewer lines than asked for is not an error: three lines asked for
    /// twenty gives three, and a range beginning past everything held gives
    /// none. `out.len()` is how many there were, which is how a caller
    /// scrolling up finds the top.
    ///
    /// `out` is filled from the start and truncated, so the same `Vec` handed
    /// back each call reuses its strings rather than allocating a pane's
    /// worth every time.
    pub fn copy_region(&self, region: LogRegion, out: &mut Vec<LogLine>) {
        // The lines still being written sit at the end of the log, so a
        // region counted back from the end runs into them first: they take
        // distances `0..held`, and the history starts after them. Which is
        // the whole of what they cost this walk — the history is asked for
        // the same window it always was, shifted past them.
        let held = self.partials_len;
        let mut lines = 0;

        let history = region.line_start.saturating_sub(held)..region.line_end.saturating_sub(held);
        // How far into the line the pieces so far have reached. Kept across
        // pieces because a line the chunk size split arrives as several, and
        // the window is over the line rather than over any one of them — see
        // [`LogClip`].
        let mut clip = LogClip::default();
        let mut open = false;

        // No ceiling on the walk: it stops as soon as it has counted back
        // `history.end` line ends, and in the degenerate case where there are
        // not that many it is bounded by the reader, which is a walk over
        // memory and not a copy of it.
        for piece in self.iter_unsync().tail_range(history, usize::MAX) {
            if !open {
                // The whole line is one writer's: a split one is continued by
                // that writer's next chunk, never by whatever sits next.
                open_line(out, lines, piece.writer());
                open = true;
            }
            clip_into(&mut out[lines].text, piece.as_str(), &mut clip, region);
            if piece.newline() {
                lines += 1;
                clip = LogClip::default();
                open = false;
            }
        }
        // A run the region cut short is still a line, and showing its head
        // beats dropping it.
        if open {
            lines += 1;
        }

        // Then the partials the region reaches. The newest is at distance
        // zero, so distance `d` is the one `d` from the end of the list, and
        // the slice comes out oldest first like everything else here.
        let nearest = region.line_end.min(held);
        if region.line_start < nearest {
            for partial in &self.partials[held - nearest..held - region.line_start] {
                open_line(out, lines, partial.writer);
                let mut clip = LogClip::default();
                clip_into(
                    &mut out[lines].text,
                    partial.text.as_str(),
                    &mut clip,
                    region,
                );
                lines += 1;
            }
        }

        out.truncate(lines);
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

    /// How many chunks it can hold: what the log holds, or the ring it was
    /// built with when that is the smaller of the two.
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

    /// Everything that had happened to the log as of the last sync.
    ///
    /// A change token and nothing more: two of these differing means the log
    /// moved, whether that was a chunk arriving or a line being typed. It is
    /// what a view polls to know whether what it is showing is still what the
    /// log says.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// How many lines are being written right now.
    ///
    /// They sit at the end of the log, so they are the first lines a region
    /// counted back from the end runs into.
    pub fn partials(&self) -> usize {
        self.partials_len
    }

    /// The chunks it holds, oldest first.
    #[cfg(test)]
    fn chunks(&self) -> &LocalRingBuffer<LogReaderChunk> {
        &self.chunks
    }
}

/// A rectangle of a log: which lines, and which columns of them.
///
/// What [`copy_region`](LogReader::copy_region) takes, and shaped like the
/// pane it is for — a window that moves in two directions over text bigger
/// than it in both, and bounded in both so that holding one costs the pane
/// and not the log.
///
/// Both pairs are half open, and both are counted from the edge the log grows
/// from.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct LogRegion {
    /// The first line, counted back from the newest: `0` is the last line,
    /// `1` the one before it.
    ///
    /// Relative to the end because that is the addressing the log answers —
    /// it counts chunks pushed, not lines written, so there is no absolute
    /// line number to name. What it costs is that with output still arriving
    /// the same bounds name different lines each time, so a pane held still
    /// over a running process drifts.
    pub line_start: usize,
    /// One line past the last, so `2..20` is the eighteen above the last two.
    pub line_end: usize,
    /// The first column of each line to take.
    ///
    /// Columns, not bytes: this is a window onto a terminal, and a byte
    /// offset would cut characters in half. A character straddling either
    /// edge is left out rather than halved.
    pub column_start: usize,
    /// One column past the last.
    pub column_end: usize,
}

impl LogRegion {
    /// A region from the two ranges it reads best as:
    /// `LogRegion::new(2..20, 0..512)`.
    ///
    /// Four fields rather than two [`Range`]s, because `Range` is an iterator
    /// and the standard library left it non-[`Copy`] on purpose: a `Copy`
    /// iterator would let `for x in range` consume a duplicate and leave the
    /// original looking untouched. Nothing iterates a region, so holding
    /// `Range`s inside one inherits that restriction for a reason it does not
    /// have, and pays for it in a clone at every call site.
    pub fn new(lines: Range<usize>, columns: Range<usize>) -> Self {
        Self {
            line_start: lines.start,
            line_end: lines.end,
            column_start: columns.start,
            column_end: columns.end,
        }
    }
}

/// One line of a copied region: the text, and who wrote it.
///
/// The id rather than an `is_note` flag, though telling a note from output is
/// what wanted it first: the same field answers which *process* a line came
/// from, and that is the other half of what a pane does with colour.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct LogLine {
    /// The clipped text of the line.
    pub text: String,
    /// Who wrote it — [`LogWriterId::NOTES`] for the supervisor's own lines.
    pub writer: LogWriterId,
}

/// A partial line as a reader keeps it.
///
/// Carries its writer for the same reason a chunk does: a region reports one
/// per line, and a line still being typed is a line like any other to
/// whatever draws it.
struct LogReaderPartial {
    writer: LogWriterId,
    text: PartialText,
}

impl LogReaderPartial {
    /// An empty slot, belonging to nobody until a sync fills it.
    fn new() -> Self {
        Self {
            writer: LogWriterId::UNSET,
            text: PartialText::new(),
        }
    }
}

/// Make sure `out` has an empty line at `line`, reusing the string there.
///
/// Which is what keeps a caller that hands the same `Vec` back on every call
/// from paying for a pane's worth of strings each time.
fn open_line(out: &mut Vec<LogLine>, line: usize, writer: LogWriterId) {
    if line < out.len() {
        out[line].text.clear();
        out[line].writer = writer;
    } else {
        out.push(LogLine {
            text: String::new(),
            writer,
        });
    }
}

/// Append the part of `text` that falls inside the region's columns.
///
/// `column` is how far into the line the pieces before this one already
/// reached, and is advanced past all of `text` whether or not any of it was
/// taken — the window is over the line, and a piece entirely to the left of
/// it still moves the position along.
fn clip_into(out: &mut String, text: &str, clip: &mut LogClip, region: LogRegion) {
    let bytes = text.as_bytes();
    for (index, character) in text.char_indices() {
        let mut shown = LogClipShown::default();
        for byte in &bytes[index..index + character.len_utf8()] {
            clip.parser.advance(&mut shown, *byte);
        }
        if shown.0.is_none() {
            // A sequence's own bytes. They take no columns, and are kept
            // wherever they fall — including left of the window, since what
            // colours the visible run is usually set before it.
            out.push(character);
            continue;
        }
        let start = clip.column;
        clip.column += character.width().unwrap_or(0);
        if start >= region.column_end {
            // Past the right hand edge, and so is everything after it. The
            // position is left where it is because nothing will read it
            // again: no later piece of this line can be inside the window.
            return;
        }
        // A character straddling an edge is dropped: half of one is not
        // something a terminal can draw.
        if start >= region.column_start && clip.column <= region.column_end {
            out.push(character);
        }
    }
}

/// How far into a line [`clip_into`] has got.
///
/// One value and not two `&mut`s because a line arrives in pieces and both
/// halves have to survive the gap between them: at [`LOG_CHUNK_SIZE`] a
/// `\x1b[1;32m` sitting across a chunk boundary is routine rather than a
/// corner, and a walk that forgot it was mid sequence would count the
/// parameters as text and cut the sequence in half.
///
/// Reset per line by the caller, so nothing an unterminated sequence does
/// reaches the line after it.
#[derive(Default)]
struct LogClip {
    /// The column the next character lands in. A sequence's bytes take none.
    column: usize,
    /// Where the walk is in the escape grammar.
    ///
    /// Borrowed rather than written here, and from the crate `clap` already
    /// pulls in: the screen has to walk the same grammar to know which run
    /// each colour applies to, and two copies of where a sequence ends is a
    /// second chance for the window and the render to disagree.
    parser: Parser,
}

/// Whether the bytes just fed made a character the screen would show.
///
/// Every other callback is left at its default, which is the whole of what
/// this has to say: anything that is not text is a sequence, and a sequence
/// takes no columns whatever it turns out to mean.
#[derive(Default)]
struct LogClipShown(Option<char>);

impl Perform for LogClipShown {
    fn print(&mut self, character: char) {
        self.0 = Some(character);
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
    /// lines and `10..20` the ten before those, which is what a view scrolled
    /// ten lines up wants. What comes out is still oldest first.
    ///
    /// It narrows a walk rather than starting one so that it composes with
    /// both [`iter_sync`](LogReader::iter_sync) and
    /// [`iter_unsync`](LogReader::iter_unsync), leaving the question of
    /// whether to fetch where it already had an answer.
    ///
    /// # `max_chunks`
    ///
    /// A ceiling on how far back it will look. Finding where a line begins
    /// means walking backwards counting newlines, and unbounded that walk is
    /// the whole ring — a lot of work to render twenty lines of a log that is
    /// mostly blank ones. A view sets it from what it could possibly draw: a
    /// pane `h` rows tall cannot show more than `h` chunks' worth of pieces.
    ///
    /// Hitting it truncates rather than fails, starting at the oldest chunk
    /// it was allowed to reach — so the first line may come out as its tail,
    /// exactly as if the log had discarded the rest. [`usize::MAX`] for no
    /// ceiling.
    ///
    /// # Fewer lines than asked for
    ///
    /// Not an error and not visible in the result: three lines asked for
    /// twenty gives three. A view that needs to know whether there is more
    /// above it asks for one line more than it draws and sees if it got it.
    ///
    /// A range starting past everything held comes back empty rather than
    /// clamped — scrolled above the top, a view is showing nothing, not
    /// showing the oldest lines twice.
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
    /// Who wrote it, which is how a note is told from output.
    writer: LogWriterId,
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

    /// Who wrote it.
    pub fn writer(&self) -> LogWriterId {
        self.writer
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
                writer: chunk.writer(),
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

    #[test]
    fn an_empty_reader_is_one_whose_log_is_not_there() {
        let mut reader = LogReader::empty(4);
        assert!(reader.is_empty());
        assert_eq!(reader.capacity(), 4);
        assert_eq!(reader.sync(), 0, "there is nothing to sync from");
    }

    #[test]
    fn a_reused_reader_picks_up_the_log_it_is_given() {
        let mut reader = LogReader::empty(16);
        let log = log_with(8, 6);
        log.reader_into(&mut reader);
        reader.sync();

        assert_eq!(
            render(&reader),
            (0..6).map(|n| format!("linha {n}\n")).collect::<String>()
        );
        assert_eq!(reader.seen(), 3);
    }

    /// A version is a token and not a clock: two logs reach the same value by
    /// having had the same amount happen to them. A reader carrying one log's
    /// value onto another would decide its first sync had nothing to do.
    #[test]
    fn a_reused_reader_does_not_trust_the_old_logs_version() {
        let um = Log::new(8);
        let mut escritor = LogBuffer::new(um.writer());
        escritor.write(b"aaa\n");
        let dois = Log::new(8);
        let mut outro = LogBuffer::new(dois.writer());
        outro.write(b"bbb\n");

        let mut reader = um.reader();
        reader.sync();
        let mut probe = dois.reader();
        probe.sync();
        assert_eq!(
            reader.version(),
            probe.version(),
            "the premise: the two logs are at the same version"
        );

        dois.reader_into(&mut reader);
        assert_eq!(reader.sync(), 1, "it took the new log to be unchanged");
        assert_eq!(render(&reader), "bbb\n");
    }

    /// It follows the log it is on and keeps the memory it built: a reader
    /// moved to a smaller log holds less and gives nothing back, which is the
    /// whole point of reusing one.
    #[test]
    fn a_reused_reader_takes_the_logs_size_and_keeps_its_own() {
        let grande = Log::new(16);
        let mut reader = grande.reader();
        assert_eq!(reader.capacity(), 16);

        let pequeno = Log::new(4);
        pequeno.reader_into(&mut reader);
        assert_eq!(reader.capacity(), 4);
        assert_eq!(reader.chunks().max_capacity(), 16, "it gave its slots back");

        let mut buffer = LogBuffer::new(pequeno.writer());
        let text: String = (0..200).map(|n| format!("linha {n}\n")).collect();
        buffer.write(text.as_bytes());
        reader.sync();

        // Against a reader that only ever saw this log: same contents means
        // nothing of the old one survived and there is no gap in the new.
        let mut fresh = pequeno.reader();
        fresh.sync();
        assert_eq!(reader.len(), 4);
        assert_eq!(render(&reader), render(&fresh));
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
        // Numbered from one: zero is [`LogWriterId::NOTES`].
        assert_eq!(chunks[0].writer().index(), 1);
        assert_eq!((chunks[1].count(), chunks[1].len()), (1, 4));
        assert_eq!(chunks[1].writer().index(), 2);
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

    /// The lines of a region, copied into a fresh buffer.
    fn region(reader: &LogReader, lines: Range<usize>) -> Vec<String> {
        columns(reader, lines, 0..usize::MAX)
    }

    /// The lines of a region, windowed to `columns`.
    ///
    /// Text alone: what a line is stamped with has its own tests, and every
    /// assertion here is about where the window landed.
    fn columns(reader: &LogReader, lines: Range<usize>, columns: Range<usize>) -> Vec<String> {
        let mut out = Vec::new();
        reader.copy_region(LogRegion::new(lines, columns), &mut out);
        out.into_iter().map(|line| line.text).collect()
    }

    /// The lines are counted back from the newest, and come out oldest first
    /// — the order a pane draws them in.
    #[test]
    fn copy_region_takes_the_lines_it_is_given() {
        let log = log_with(64, 10);
        let mut reader = log.reader();
        reader.sync();

        assert_eq!(region(&reader, 0..3), ["linha 7", "linha 8", "linha 9"]);
        assert_eq!(region(&reader, 2..4), ["linha 6", "linha 7"]);
        assert_eq!(region(&reader, 2..20).len(), 8, "it stops at what there is");
    }

    /// The same window the walk it is built on gives, so the two cannot
    /// disagree about where a line begins.
    #[test]
    fn copy_region_is_the_tail_walk_with_the_columns_taken_off() {
        let log = log_with(64, 10);
        let mut reader = log.reader();
        reader.sync();

        assert_eq!(region(&reader, 1..5), window(&reader, 1..5, usize::MAX));
    }

    /// The window is over the line, in columns, and reaching past either end
    /// of the text simply gets what is there.
    #[test]
    fn copy_region_windows_the_columns() {
        let log = log_with(64, 2);
        let mut reader = log.reader();
        reader.sync();

        assert_eq!(columns(&reader, 0..2, 0..5), ["linha", "linha"]);
        assert_eq!(columns(&reader, 0..2, 6..11), ["0", "1"]);
        assert_eq!(columns(&reader, 0..2, 99..104), ["", ""]);
    }

    /// Columns, not bytes — a byte offset would cut characters in half, and a
    /// character straddling an edge is left out rather than halved.
    #[test]
    fn the_column_window_counts_columns() {
        let log = Log::new(64);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write("coração\n".as_bytes());
        let mut reader = log.reader();
        reader.sync();

        assert_eq!(columns(&reader, 0..1, 0..4), ["cora"]);
        assert_eq!(columns(&reader, 0..1, 4..7), ["ção"]);
    }

    /// Whatever a terminal reads as a command rather than as text takes no
    /// columns: a window of ten is ten visible characters, however many bytes
    /// of colour are threaded through them.
    #[test]
    fn an_escape_sequence_takes_no_columns() {
        let log = Log::new(64);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write("\u{1b}[31mvermelho\u{1b}[0m normal\n".as_bytes());
        let mut reader = log.reader();
        reader.sync();

        let line = &columns(&reader, 0..1, 0..10)[0];
        assert_eq!(visible(line), "vermelho n");
    }

    /// Half a sequence is worse than none: a terminal handed one would read
    /// the parameters of the next as text, or sit in a colour nothing closes.
    #[test]
    fn a_window_never_cuts_a_sequence_in_half() {
        let log = Log::new(64);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write("ab\u{1b}[1;32mcd\n".as_bytes());
        let mut reader = log.reader();
        reader.sync();

        // The window ends inside the run the sequence sits in.
        let line = &columns(&reader, 0..1, 0..3)[0];
        assert!(line.contains("\u{1b}[1;32m"), "{line:?}");
        assert_eq!(visible(line), "abc");
    }

    /// What colours the window is usually to the left of it, so a sequence
    /// before `column_start` is kept while the text there is dropped —
    /// otherwise scrolling right hands the pane the wrong colour.
    #[test]
    fn a_sequence_left_of_the_window_is_kept() {
        let log = Log::new(64);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write("\u{1b}[31mabcdef\n".as_bytes());
        let mut reader = log.reader();
        reader.sync();

        let line = &columns(&reader, 0..1, 3..6)[0];
        assert_eq!(line, "\u{1b}[31mdef");
    }

    /// The state that matters most, because a chunk is [`LOG_CHUNK_SIZE`] and
    /// a coloured line runs past one constantly: a walk that forgot it was
    /// mid sequence at the boundary would count `31m` as three columns of
    /// text.
    #[test]
    fn a_sequence_split_across_chunks_is_still_one_sequence() {
        let log = Log::new(4096);
        let mut buffer = LogBuffer::new(log.writer());
        let head = "a".repeat(LOG_CHUNK_SIZE - 2);
        let line = format!("{head}\u{1b}[31mbbb\n");
        buffer.write(line.as_bytes());
        let mut reader = log.reader();
        reader.sync();

        let back = &columns(&reader, 0..1, 0..usize::MAX)[0];
        assert!(back.contains("\u{1b}[31m"), "the sequence arrived broken");
        assert_eq!(visible(back).len(), head.len() + 3);
    }

    /// A device control string carries a payload that is not text either —
    /// sixel, a `tmux` passthrough — and the whole of it takes no columns.
    ///
    /// This is what walking the real grammar buys over knowing `CSI` and
    /// `OSC`: `ESC P` opens a string, and a walk that read it as a two
    /// character sequence would count the payload as six columns of text.
    #[test]
    fn a_device_control_string_takes_no_columns() {
        let log = Log::new(64);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write("ab\u{1b}Pq#0;2\u{1b}\\cd\n".as_bytes());
        let mut reader = log.reader();
        reader.sync();

        assert_eq!(visible(&columns(&reader, 0..1, 0..usize::MAX)[0]), "abcd");
    }

    /// A sequence nothing closed is the line's own problem and not the next
    /// line's — the walk starts each one over.
    #[test]
    fn an_unterminated_sequence_does_not_reach_the_next_line() {
        let log = Log::new(64);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write("\u{1b}[31maberto\nseguinte\n".as_bytes());
        let mut reader = log.reader();
        reader.sync();

        assert_eq!(columns(&reader, 0..2, 0..4), ["\u{1b}[31maber", "segu"]);
    }

    /// The text a terminal would actually show, for the assertions above.
    fn visible(line: &str) -> String {
        let mut parser: Parser = Parser::default();
        let mut out = String::new();
        let bytes = line.as_bytes();
        for (index, character) in line.char_indices() {
            let mut shown = LogClipShown::default();
            for byte in &bytes[index..index + character.len_utf8()] {
                parser.advance(&mut shown, *byte);
            }
            if shown.0.is_some() {
                out.push(character);
            }
        }
        out
    }

    /// A line the chunk size split arrives as several pieces, so the column
    /// window has to be over the line rather than over each piece of it.
    #[test]
    fn the_column_window_spans_a_split_line() {
        let log = Log::new(64);
        let mut buffer = LogBuffer::new(log.writer());
        let longa: String = (0..LOG_CHUNK_SIZE * 2)
            .map(|n| char::from(b'a' + (n % 26) as u8))
            .collect();
        buffer.write(format!("{longa}\n").as_bytes());
        let mut reader = log.reader();
        reader.sync();

        // Far past the first chunk, so only a later piece can answer it.
        let from = LOG_CHUNK_SIZE + 10;
        assert_eq!(
            columns(&reader, 0..1, from..from + 4),
            [&longa[from..from + 4]]
        );
    }

    /// A rectangle is bounded in both directions, which is the whole reason
    /// for asking in one: this is what a pane costs, whatever is behind it.
    #[test]
    fn a_region_costs_its_own_size_and_not_the_logs() {
        let log = Log::new(4096);
        let mut buffer = LogBuffer::new(log.writer());
        let wide = "z".repeat(4000);
        let text: String = (0..500).map(|_| format!("{wide}\n")).collect();
        buffer.write(text.as_bytes());
        let mut reader = log.reader();
        reader.sync();

        let mut out = Vec::new();
        reader.copy_region(LogRegion::new(2..20, 0..512), &mut out);
        assert_eq!(out.len(), 18);
        let bytes: usize = out.iter().map(|line| line.text.len()).sum();
        assert!(bytes <= 18 * 512, "a pane's worth, not a log's: {bytes}");
    }

    /// A range beginning past everything held gives nothing rather than
    /// clamping back onto the oldest lines.
    #[test]
    fn a_region_past_the_oldest_line_is_empty() {
        let log = log_with(64, 4);
        let mut reader = log.reader();
        reader.sync();
        assert!(region(&reader, 10..20).is_empty());
    }

    /// The buffer is filled from the start and truncated, so a caller that
    /// keeps it does not pay for a pane's worth of strings on every call.
    #[test]
    fn copy_region_refills_the_buffer_it_is_given() {
        let log = log_with(64, 10);
        let mut reader = log.reader();
        reader.sync();

        let mut out = Vec::new();
        reader.copy_region(LogRegion::new(0..6, 0..usize::MAX), &mut out);
        assert_eq!(out.len(), 6);

        reader.copy_region(LogRegion::new(0..2, 0..usize::MAX), &mut out);
        let texts: Vec<&str> = out.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(
            texts,
            ["linha 8", "linha 9"],
            "the old lines are gone, not appended"
        );
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
