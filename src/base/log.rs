use std::{cell::UnsafeCell, sync::Arc};

use parking_lot::RwLock;

use crate::util::ring::{RingStr, RingStrIter};

#[derive(Clone)]
struct LogData {
    offset: u128,
    buf: RingStr,
}

impl LogData {
    fn with_capacity(capacity: usize) -> Self {
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
    inner: Arc<RwLock<LogData>>,
}

impl LogWriter {
    pub fn new(capacity: usize) -> Self {
        let data = LogData::with_capacity(capacity);
        let inner = Arc::new(RwLock::new(data.clone()));
        LogWriter { inner }
    }

    pub fn pair(capacity: usize) -> (Self, Log) {
        let data = LogData::with_capacity(capacity);
        let inner = Arc::new(RwLock::new(data.clone()));
        let log = Log {
            src: inner.clone(),
            data: UnsafeCell::new(data),
        };
        let log_writer = LogWriter { inner };
        (log_writer, log)
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
        let (mut writer, log) = LogWriter::pair(10);
        writer.write_line("oi");
        writer.write_line("tudo");
        writer.write_line("bem");
        writer.write_line("com");

        let log2 = log.clone();
        for line in log.iter() {
            println!("{}", line);
        }

        for line in log2.iter() {
            println!("{}", line);
        }
    }
}
