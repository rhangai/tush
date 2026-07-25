use std::{cell::UnsafeCell, num::NonZeroUsize, sync::Arc};

use parking_lot::RwLock;

use crate::util::ring::{RingStr, RingStrIter};

#[derive(Clone)]
struct LogData {
    offset: u128,
    buf: RingStr,
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

    pub fn log_unsynced(&self) -> Log {
        let src = self.inner.clone();
        let data = LogData::with_capacity(self.capacity);
        Log {
            src,
            data: UnsafeCell::new(data),
        }
    }
}

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

    fn assert_log(log: &Log, expected: &Vec<&str>) {
        let values: Vec<String> = log.iter().map(|s| s.clone()).collect();
        assert_eq!(&values, expected);
    }
}
