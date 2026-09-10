use std::sync::{Arc, Weak};

use crate::util::ring::{RingStr, RingStrLines, RingStrStorage};

/// A snapshot of the most recent lines of a [`Log`].
///
/// A buffer is a private copy of the log contents, so iterating over it never
/// holds a lock on the shared ring and never blocks the writers. It only moves
/// forward when it is explicitly refreshed with [`Log::update_buffer`], which
/// makes it a good fit for a render loop: take one buffer per view, update it
/// once per frame, then iterate freely.
#[derive(Clone)]
pub struct LogBuffer {
    storage: RingStrStorage,
}

impl LogBuffer {
    /// Iterate over the lines captured by the last refresh, oldest first.
    pub fn lines(&self) -> RingStrLines<'_> {
        self.storage.lines()
    }
}

/// Shared state behind a [`Log`], referenced weakly by every writer.
struct LogInner {
    ringstr: RingStr,
}

/// The output history of a single process.
///
/// A log owns a fixed capacity ring of lines: once it is full, writing a new
/// line drops the oldest one. Ownership matters here — writers only hold a
/// [`Weak`] reference through [`LogWriterRef`], so when the log is dropped the
/// still running stdout pump simply stops recording instead of keeping the
/// buffer alive.
pub struct Log {
    inner: Arc<LogInner>,
}

impl Log {
    /// Create a log that retains at most `capacity` lines.
    pub fn new(capacity: usize) -> Self {
        let ringstr = RingStr::new(capacity);
        Self {
            inner: Arc::new(LogInner { ringstr }),
        }
    }

    /// Create a fresh buffer already synced with the current contents.
    pub fn new_buffer(&self) -> LogBuffer {
        let storage = self.inner.ringstr.create_storage();
        LogBuffer { storage }
    }

    /// Advance an existing buffer to the current contents.
    ///
    /// Only the lines written since the last sync are copied; if the buffer
    /// fell behind by more than the capacity, it is refilled from scratch.
    ///
    /// `buffer` must have come from this log — see
    /// [`force_update_buffer`](Log::force_update_buffer) for moving one
    /// between logs.
    pub fn update_buffer(&self, buffer: &mut LogBuffer) {
        self.inner.ringstr.update_storage(&mut buffer.storage);
    }

    /// Re-point an existing buffer at this log, discarding what it held.
    ///
    /// [`update_buffer`](Log::update_buffer) only works on a buffer this log
    /// issued: cursors are meaningless across logs, so refreshing a foreign
    /// one either splices in unrelated lines or silently freezes. This is the
    /// deliberate way across — a view following a unit as the user switches
    /// between them reuses its buffer instead of allocating a new one.
    ///
    /// The buffer keeps the size it was created with, so moving onto a log
    /// with a longer history yields only the newest lines that fit.
    pub fn force_update_buffer(&self, buffer: &mut LogBuffer) {
        self.inner.ringstr.force_update_storage(&mut buffer.storage);
    }

    /// Get a cloneable handle that can append lines to this log.
    pub fn writer(&self) -> LogWriterRef {
        LogWriterRef {
            inner: Arc::downgrade(&self.inner),
        }
    }
}

/// A weak, cloneable handle used to append lines to a [`Log`].
///
/// Writes are silently discarded once the owning [`Log`] has been dropped, so a
/// background task pumping stdout does not need to be cancelled to be made
/// harmless.
#[derive(Clone)]
pub struct LogWriterRef {
    inner: Weak<LogInner>,
}

impl LogWriterRef {
    /// Append one line, or do nothing if the log is gone.
    pub fn write_line(&mut self, line: impl AsRef<str>) {
        if let Some(inner) = self.inner.upgrade() {
            inner.ringstr.write_line(line);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn log() {
        let log = Log::new(3);
        let mut buffer = log.new_buffer();
        let mut writer = log.writer();
        writer.write_line("oi");
        writer.write_line("tudo");
        writer.write_line("bem");

        let expected_a = vec!["oi", "tudo", "bem"];
        assert_log(&log, &mut buffer, &expected_a);
        writer.write_line("com");
        writer.write_line("você");

        let expected_b = vec!["bem", "com", "você"];
        assert_log(&log, &mut buffer, &expected_b);
    }

    fn assert_log(log: &Log, buffer: &mut LogBuffer, expected: &[&str]) {
        log.update_buffer(buffer);
        let values: Vec<String> = buffer.lines().cloned().collect();
        assert_eq!(&values, expected);
    }
}
