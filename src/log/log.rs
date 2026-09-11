use std::sync::{Arc, Weak};

use crate::{
    log::{LogBuffer, chunk::LogChunk},
    util::localring::LocalRingBuffer,
};
use parking_lot::RwLock;
use thingbuf::{Recycle, ThingBuf};
use tokio::{io::AsyncRead, sync::Notify, task::JoinHandle};

/// A handle for appending to a [`Log`] from a reader task.
///
/// Weak on purpose: tasks still draining their pipes should not keep a log
/// alive after whatever owned it is gone. Once it is, pushes are dropped
/// and the reader carries on emptying its pipe.
pub struct LogWriterRef {
    inner: Weak<LogInner>,
}

impl LogWriterRef {
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
    pub(super) fn push_chunk(&mut self, chunk: &mut LogChunk) {
        if let Some(inner) = self.inner.upgrade() {
            inner.chunks.write().push().swap(chunk);
        }
        chunk.clear();
    }

    /// Consume toda a
    pub fn consume_spawn<R>(self, mut read: R) -> JoinHandle<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let mut log_buffer = LogBuffer::new(self);
            while log_buffer.read(&mut read).await.is_ok() {}
        })
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
    /// one to each, and their lines land in the order they arrive.
    pub fn writer(&self) -> LogWriterRef {
        LogWriterRef {
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub fn debug(&self) {
        let mut lines = 0;
        for line in self.inner.chunks.read().iter() {
            println!("{}", line.as_str());
            lines += 1;
        }
        println!("Lines: {}", lines);
    }
}

struct LogInner {
    chunks: RwLock<LocalRingBuffer<LogChunk>>,
    notify: Arc<Notify>,
}

impl LogInner {
    fn new(capacity: usize) -> Arc<Self> {
        let notify = Arc::new(Notify::new());
        Arc::new(LogInner {
            chunks: RwLock::new(LocalRingBuffer::new_with(capacity, LogChunk::new)),
            notify,
        })
    }
}

/// Recycler
struct LogChunkRecycler {}
impl Recycle<LogChunk> for LogChunkRecycler {
    fn new_element(&self) -> LogChunk {
        LogChunk::new()
    }

    fn recycle(&self, element: &mut LogChunk) {
        element.clear();
    }
}
