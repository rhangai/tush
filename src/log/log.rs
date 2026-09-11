use std::sync::{
    Arc, Weak,
    atomic::{AtomicU32, Ordering},
};

use crate::{
    log::{LogBuffer, chunk::LogChunk},
    util::{
        arena::{Arena, ArenaBlock},
        localring::LocalRingBuffer,
    },
};
use parking_lot::Mutex;
use thingbuf::{Recycle, StaticThingBuf};
use tokio::{io::AsyncRead, sync::Notify, task::JoinHandle};

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
    inner: Weak<LogInner>,
    /// Stamped onto every chunk this writer hands over. Fixed at creation:
    /// one writer is one source of output for its whole life, which is what
    /// makes the stamp worth anything.
    id: LogWriterId,
}

impl LogWriterRef {
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
    /// Nothing is copied: the chunk swaps places with whichever slot the
    /// ring was about to overwrite, so `chunk` comes back owning the buffer
    /// that slot used to hold. That is the whole point of the ring holding
    /// pre-built chunks — a log at capacity never allocates again.
    ///
    /// The chunk is cleared before it comes back, so the caller can go
    /// straight on writing. Forgetting that step would leave it marked
    /// finished, and every later write would return 0 for ever, which is
    /// why it happens here rather than at the call site.
    ///
    /// A log that has already been dropped takes nothing, but the chunk is
    /// still reset: a reader whose log went away should keep draining its
    /// pipe, not seize up.
    pub(super) async fn push_chunk(&mut self, chunk: &mut LogChunk) -> bool {
        loop {
            match self.push_chunk_inner(chunk) {
                Some(true) => {
                    return true;
                }
                Some(false) => {
                    tokio::task::yield_now().await;
                }
                None => {
                    return false;
                }
            };
        }
    }

    /// One attempt at the hand-off, reporting which of three things happened.
    ///
    /// - `Some(true)` — the chunk is in the queue and a recycled one has
    ///   taken its place.
    /// - `Some(false)` — the queue is full. Nothing moved and the chunk is
    ///   untouched, so the caller can simply try again once the sync task has
    ///   had a chance to drain.
    /// - `None` — the log is gone, and no amount of retrying will bring it
    ///   back.
    ///
    /// Split out from [`push_chunk`](Self::push_chunk) so the retry loop
    /// holds no `Arc` across its `await`: the upgrade happens and is dropped
    /// inside this call, which keeps a reader task from being the thing that
    /// keeps a dead log alive.
    ///
    /// The swap is where the recycling happens — see [`LogChunkRecycler`]
    /// for why the chunk comes back cleared.
    fn push_chunk_inner(&mut self, chunk: &mut LogChunk) -> Option<bool> {
        let inner = self.inner.upgrade()?;
        let Ok(mut item) = inner.chunks_queue.push_ref() else {
            // Ask for room before the caller goes round again. Without this
            // the spin leans on a notification some earlier push happened to
            // leave behind, which holds today but only by accident.
            inner.notify_writer();
            return Some(false);
        };
        // Stamp before the hand-off: after the swap this chunk is the
        // recycled one, and the stamp belongs to the content going in.
        chunk.set_writer(self.id);
        item.swap(chunk);
        inner.notify_writer();
        Some(true)
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
            loop {
                let result = log_buffer.read(&mut read).await?;
                if !result {
                    break;
                }
            }
            Ok(())
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

    /// Print the whole history to stdout.
    ///
    /// Scaffolding for the CLI that does not exist yet. Syncs first so
    /// anything still in the queue is included, then walks the ring oldest
    /// first.
    ///
    /// Joins the pieces of a line that was split across chunks, which is the
    /// same thing any real renderer has to do: walk the pieces and hold on to
    /// each one until a [`Line`](super::chunk::LogChunkData::Line) closes it.
    pub fn debug(&self) {
        let lines = self.lines();
        for (_, line) in &lines {
            println!("{line}");
        }
        println!("Lines: {}", lines.len());
    }

    /// The history as whole lines, each with the writer that produced it.
    ///
    /// Joins the pieces of a line split across chunks — per writer, since
    /// another process's chunk can sit between two pieces of the same line;
    /// see [`LogChunkData`](super::chunk::LogChunkData).
    ///
    /// Copies out, so the lock is held for the walk and nothing else. A
    /// renderer must not hold it while painting: writers are kept off it by
    /// the queue, but the task draining that queue is not.
    pub(super) fn lines(&self) -> Vec<(LogWriterId, String)> {
        self.inner.sync_queue();

        let mut lines = Vec::new();
        // One entry per writer with a line still open. A handful at most —
        // one per process of the unit — so a scan beats a map.
        let mut pending: Vec<(LogWriterId, String)> = Vec::new();

        for chunk in self.inner.chunks.lock().iter() {
            let writer = chunk.writer();
            let slot = match pending.iter().position(|(id, _)| *id == writer) {
                Some(at) => at,
                None => {
                    pending.push((writer, String::new()));
                    pending.len() - 1
                }
            };
            for data in chunk.iter_data() {
                pending[slot].1.push_str(data.as_str());
                if data.is_line() {
                    lines.push((writer, std::mem::take(&mut pending[slot].1)));
                }
            }
        }

        // Lines the ring cut off in the middle of.
        lines.extend(pending.into_iter().filter(|(_, text)| !text.is_empty()));
        lines
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
struct LogInner {
    /// Every chunk's bytes, in one allocation. The ring, the queue and each
    /// reader's working chunk all draw from here, which is what lets a chunk
    /// move between them by swapping an index.
    arena: Arena,
    /// The history: the last `capacity` chunks, oldest first. The mutex is
    /// the only lock in the module, and the sync task is its only writer.
    chunks: Mutex<LocalRingBuffer<LogChunk>>,
    /// The hand-off. Bounded, so a process shouting faster than the sync task
    /// drains makes its reader wait rather than letting the queue grow
    /// without bound.
    chunks_queue: StaticThingBuf<LogChunk, CHUNK_QUEUE_SIZE, LogChunkRecycler>,
    /// Hands out the next [`LogWriterId`]. Only ever incremented, so an id
    /// is never reused even after a writer is gone — a stale stamp in the
    /// ring keeps meaning the writer it always meant.
    next_writer_id: AtomicU32,
    /// Chunks handed back by readers that have finished, waiting for the
    /// next reader. Small — it never holds more than the number of readers
    /// that have ever run at once — and touched only when a reader starts or
    /// stops, so a plain lock costs nothing here.
    chunks_free: Mutex<Vec<LogChunk>>,
    /// The task draining `chunks_queue` into `chunks`. Aborted on drop.
    sync_handle: JoinHandle<()>,
    /// How a writer tells that task there is something to drain. Held by an
    /// `Arc` rather than reached through the weak reference because the task
    /// has to be able to wait on it without keeping the log alive.
    notify: Arc<Notify>,
}

/// Stop the sync task when the log goes.
///
/// Nothing is flushed here, and nothing can be: this runs once the last
/// strong reference is gone, so the task's own weak upgrade would already
/// fail. Whatever was still in the queue is dropped along with the ring it
/// was headed for.
impl Drop for LogInner {
    fn drop(&mut self) {
        self.sync_handle.abort();
    }
}

impl LogInner {
    /// Build the shared state and start its sync task.
    ///
    /// Cyclic because the task must reach the very thing being constructed,
    /// and only weakly — a task holding an `Arc` would keep the log alive for
    /// ever and nothing would ever be dropped. It is safe to spawn before the
    /// `Arc` is finished because the task waits on the notification first,
    /// and nothing can notify before a writer exists.
    ///
    /// # Panics
    ///
    /// If `capacity` is zero, or if called outside a tokio runtime.
    fn new(capacity: usize) -> Arc<Self> {
        let notify = Arc::new(Notify::new());
        // One allocation for every chunk the log can hold at once: the ring,
        // the queue that feeds it, and the working chunk of each reader.
        let arena = Arena::new(capacity + CHUNK_QUEUE_SIZE + WRITER_CHUNK_BLOCKS);
        Arc::new_cyclic(|weak: &Weak<Self>| {
            let sync_handle = {
                let notify = notify.clone();
                let weak = weak.clone();
                tokio::spawn(async move {
                    loop {
                        notify.notified().await;
                        let Some(inner) = weak.upgrade() else {
                            break;
                        };
                        inner.sync_queue();
                    }
                })
            };
            LogInner {
                chunks: Mutex::new(LocalRingBuffer::new_with(capacity, || {
                    let block = arena
                        .alloc()
                        .expect("the arena was sized to hold the whole ring");
                    LogChunk::new(block)
                })),
                arena: arena.clone(),
                chunks_free: Mutex::new(Vec::new()),
                next_writer_id: AtomicU32::new(0),
                chunks_queue: StaticThingBuf::with_recycle(LogChunkRecycler {
                    arena: arena.clone(),
                }),
                sync_handle,
                notify,
            }
        })
    }

    /// Move everything waiting in the queue into the ring.
    ///
    /// The only place the mutex is taken for writing. Drains in one pass
    /// rather than one wake per chunk, so a burst of output costs a single
    /// acquisition, and checks the queue before reaching for the lock at all
    /// — spurious wakeups are normal, since a writer notifies on every push
    /// whether or not the task was already awake.
    ///
    /// Each move is a swap, so the ring's displaced chunk goes back into the
    /// queue slot and is cleared when that slot is next claimed.
    ///
    /// Callable from anywhere: the display side runs it too, to be sure it is
    /// not showing a stale history.
    fn sync_queue(&self) {
        if self.chunks_queue.is_empty() {
            return;
        }
        let mut chunks = self.chunks.lock();
        while let Some(mut item) = self.chunks_queue.pop_ref() {
            chunks.push().swap(&mut item);
        }
    }

    /// Wake the sync task.
    ///
    /// Named for the caller, not the target — this is what a writer calls
    /// after a successful push. Cheap when the task is already awake, and a
    /// notification raised then is remembered, so a chunk pushed while the
    /// task was mid-drain still gets a pass of its own.
    /// Claim the next writer id.
    fn next_writer_id(&self) -> LogWriterId {
        LogWriterId::new(self.next_writer_id.fetch_add(1, Ordering::Relaxed))
    }

    fn notify_writer(&self) {
        self.notify.notify_one();
    }
}

#[cfg(test)]
impl Log {
    /// The history as lines, for assertions.
    ///
    /// Joins the chunks a long line was split across, which is what a real
    /// renderer has to do and what [`debug`](Log::debug) does not.
    pub(super) fn collect_lines(&self) -> Vec<String> {
        self.lines().into_iter().map(|(_, line)| line).collect()
    }

    /// The pieces of every chunk: `(text, ends a line)`, grouped by chunk.
    ///
    /// Shows the packing itself rather than the history it adds up to, which
    /// is the only way to tell whether lines shared a chunk.
    pub(super) fn chunk_pieces(&self) -> Vec<Vec<(String, bool)>> {
        self.inner.sync_queue();
        self.inner
            .chunks
            .lock()
            .iter()
            .map(|chunk| {
                chunk
                    .iter_data()
                    .map(|data| (data.as_str().to_string(), data.is_line()))
                    .collect()
            })
            .collect()
    }

    /// Every chunk in the ring, as raw bytes.
    ///
    /// For checking the invariant [`as_str`](LogChunk::as_str) relies on:
    /// each chunk has to be valid UTF-8 *on its own*, not merely once the
    /// ring is concatenated.
    pub(super) fn chunk_bytes(&self) -> Vec<Vec<u8>> {
        self.inner.sync_queue();
        let mut pieces = Vec::new();
        for chunk in self.inner.chunks.lock().iter() {
            for n in 0..chunk.count() {
                pieces.push(chunk.piece_bytes(n).unwrap().to_vec());
            }
        }
        pieces
    }

    /// The history as lines, each with the writer that produced it.
    pub(super) fn collect_attributed(&self) -> Vec<(LogWriterId, String)> {
        self.lines()
    }
}

/// Keeps the queue's chunks reusable, and builds them on first use.
///
/// The queue holds chunks, not references to them, and a slot's occupant
/// outlives the value that was popped from it — so a chunk handed back to a
/// writer is whatever the ring displaced, still full of an old line. This is
/// where that is undone: the queue calls it when a slot is claimed for a
/// push, so a writer always receives an empty, open chunk and never has to
/// remember to reset one.
///
/// [`new_element`](Recycle::new_element) is only reached the first time each
/// slot is used; after that every chunk in the system is one that already
/// exists.
struct LogChunkRecycler {
    arena: Arena,
}

impl Recycle<LogChunk> for LogChunkRecycler {
    fn new_element(&self) -> LogChunk {
        let block = self
            .arena
            .alloc()
            .expect("the arena was sized to hold the whole queue");
        LogChunk::new(block)
    }

    fn recycle(&self, element: &mut LogChunk) {
        element.clear();
    }
}
