use std::{
    cell::UnsafeCell,
    num::NonZeroUsize,
    sync::{Arc, Weak},
};

use parking_lot::RwLock;

use crate::util::ring::{RingStr, RingStrIter};

#[derive(Clone)]
struct LogData {
    buf: RingStr,
    offset: u128,
}

impl LogData {
    fn with_capacity(capacity: NonZeroUsize) -> Self {
        Self {
            offset: 0,
            buf: RingStr::with_capacity(capacity),
        }
    }

    fn write_line(&mut self, line: impl AsRef<str>) {
        self.buf.write_line(line);
        self.offset = self.offset.wrapping_add(1);
    }

    fn sync_data(&mut self, data: &LogData) {
        if self.offset >= data.offset {
            return;
        }

        let n = data.offset - self.offset;
        let cap = self.buf.capacity() as u128;
        if n > cap {
            self.buf.extend_lines(data.buf.iter());
            self.offset = data.offset;
            return;
        }

        self.buf.extend_lines(data.buf.iter_latest(n as usize));
        self.offset = data.offset;
    }
}

/// The log writer
///
/// Source of the data being written
pub struct LogWriter {
    capacity: NonZeroUsize,
    inner: Arc<RwLock<LogData>>,
}

impl LogWriter {
    pub fn new(capacity: NonZeroUsize) -> Self {
        let data = LogData::with_capacity(capacity);
        let inner = Arc::new(RwLock::new(data));
        LogWriter { capacity, inner }
    }

    /// Create a log pair
    ///
    /// A writer and a LogWeak, so it can be created witout keeping a reference to the writer itself
    pub fn pair_weak(capacity: NonZeroUsize) -> (Self, LogWeak) {
        let writer = Self::new(capacity);
        let log = LogWeak {
            src: Arc::downgrade(&writer.inner),
        };
        (writer, log)
    }

    pub fn write_line(&mut self, line: impl AsRef<str>) {
        let mut inner = self.inner.write();
        inner.write_line(line);
    }

    pub fn log(&self) -> Log {
        let src = self.inner.clone();
        let data = {
            let lock = src.read();
            lock.clone()
        };
        Log {
            src,
            data: UnsafeCell::new(data),
        }
    }
}

pub struct LogWeak {
    src: Weak<RwLock<LogData>>,
}

impl LogWeak {
    pub fn upgrade(&self) -> Option<Log> {
        self.src.upgrade().map(|src| {
            let data = {
                let lock = src.read();
                lock.clone()
            };
            Log {
                src,
                data: UnsafeCell::new(data),
            }
        })
    }
}

/// A readonly log
///
/// It keeps itself synced with the writer
pub struct Log {
    src: Arc<RwLock<LogData>>,
    data: UnsafeCell<LogData>,
}

impl Clone for Log {
    fn clone(&self) -> Self {
        let data = unsafe { &mut *self.data.get() }.clone();
        Self {
            src: self.src.clone(),
            data: UnsafeCell::new(data),
        }
    }
}

impl Log {
    pub fn iter<'a>(&'a self) -> RingStrIter<'a> {
        self.sync();
        unsafe { &mut *self.data.get() }.buf.iter()
    }

    fn sync(&self) {
        let inner = self.src.read();
        unsafe { &mut *self.data.get() }.sync_data(&inner);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn log() {
        let mut writer = LogWriter::new(NonZeroUsize::new(3).unwrap());
        let log_a = writer.log();
        writer.write_line("oi");
        writer.write_line("tudo");
        writer.write_line("bem");
        let log_b = writer.log();
        let log_c = log_a.clone();
        let log_d = log_b.clone();

        let expected_a = vec!["oi", "tudo", "bem"];
        assert_log(&log_a, &expected_a);
        assert_log(&log_b, &expected_a);
        assert_log(&log_c, &expected_a);
        assert_log(&log_d, &expected_a);
        writer.write_line("com");
        writer.write_line("você");

        let expected_b = vec!["bem", "com", "você"];
        assert_log(&log_a, &expected_b);
        assert_log(&log_b, &expected_b);
        assert_log(&log_c, &expected_b);
        assert_log(&log_d, &expected_b);
    }

    #[test]
    fn empty_log_yields_no_lines() {
        let writer = LogWriter::new(NonZeroUsize::new(3).unwrap());
        let log = writer.log();
        assert_log(&log, &[]);
    }

    #[test]
    fn log_syncs_lazily_on_each_iter() {
        let mut writer = LogWriter::new(NonZeroUsize::new(4).unwrap());
        let log = writer.log();

        // Nothing written yet.
        assert_log(&log, &[]);

        writer.write_line("starting server");
        assert_log(&log, &["starting server"]);

        writer.write_line("listening on :8080");
        writer.write_line("ready to accept connections");
        assert_log(
            &log,
            &[
                "starting server",
                "listening on :8080",
                "ready to accept connections",
            ],
        );
    }

    #[test]
    fn reader_taken_after_writes_sees_earlier_lines() {
        let mut writer = LogWriter::new(NonZeroUsize::new(3).unwrap());
        writer.write_line("connecting to database");
        writer.write_line("running migrations");

        // A log obtained from the writer starts from the current buffer...
        let log = writer.log();
        assert_log(&log, &["connecting to database", "running migrations"]);

        // ...and keeps tracking further writes through the shared source.
        writer.write_line("migrations applied");
        assert_log(
            &log,
            &[
                "connecting to database",
                "running migrations",
                "migrations applied",
            ],
        );
    }

    #[test]
    fn sync_gap_larger_than_capacity_keeps_latest() {
        let mut writer = LogWriter::new(NonZeroUsize::new(2).unwrap());
        let log = writer.log();
        // Prime the reader so its offset trails the writer.
        assert_log(&log, &[]);

        // Write more lines than the capacity before the reader syncs again,
        // so the number of missed lines exceeds the buffer capacity.
        writer.write_line("job 1 started");
        writer.write_line("job 2 started");
        writer.write_line("job 3 started");
        writer.write_line("job 4 started");
        writer.write_line("job 5 started");

        // Only the two most recent lines survive in the ring buffer.
        assert_log(&log, &["job 4 started", "job 5 started"]);
    }

    #[test]
    fn capacity_of_one_keeps_only_last_line() {
        let mut writer = LogWriter::new(NonZeroUsize::new(1).unwrap());
        let log = writer.log();
        writer.write_line("loading config");
        writer.write_line("config loaded");
        writer.write_line("shutting down");

        assert_log(&log, &["shutting down"]);
    }

    fn assert_log(log: &Log, expected: &[&str]) {
        let values: Vec<String> = log.iter().map(|s| s.clone()).collect();
        assert_eq!(&values, expected);
    }
}
