use std::sync::{Arc, Weak};

use crate::util::ring::{RingStr, RingStrStorage};

/// A log buffer
#[derive(Clone)]
pub struct LogBuffer {
    storage: RingStrStorage,
}

impl LogBuffer {
    pub fn lines(&self) -> impl Iterator<Item = &String> {
        self.storage.lines()
    }
}

/// Inner log structure
struct LogInner {
    ringstr: RingStr,
}

pub struct Log {
    inner: Arc<LogInner>,
}

impl Log {
    pub fn new(capacity: usize) -> Self {
        let ringstr = RingStr::new(capacity);
        Self {
            inner: Arc::new(LogInner { ringstr }),
        }
    }

    /// Get the iter and syncs it if needed
    pub fn new_buffer(&self) -> LogBuffer {
        let storage = self.inner.ringstr.create_storage();
        LogBuffer { storage }
    }

    /// Get the iter and syncs it if needed
    pub fn update_buffer(&self, buffer: &mut LogBuffer) {
        self.inner.ringstr.update_storage(&mut buffer.storage);
    }

    /// Get the iter and syncs it if needed
    pub fn writer(&self) -> LogWriterRef {
        LogWriterRef {
            inner: Arc::downgrade(&self.inner),
        }
    }
}

#[derive(Clone)]
pub struct LogWriterRef {
    inner: Weak<LogInner>,
}

impl LogWriterRef {
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
        let values: Vec<String> = buffer.lines().map(|s| s.clone()).collect();
        assert_eq!(&values, expected);
    }
}
