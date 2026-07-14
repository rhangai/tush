use std::sync::Arc;

use tokio::sync::RwLock;

use crate::util::ring::RingStr;

struct LogData {
    offset: u128,
    buf: RingStr,
}

struct LogInner {
    data: LogData,
}

impl LogInner {
    fn writeln(&mut self, writer: impl FnOnce(&mut String)) {
        self.data.buf.write_line(writer);
        self.data.offset = self.data.offset.wrapping_add(1);
    }

    fn sync_data(&mut self, data: &LogData) {
        if self.data.offset >= data.offset {
            return;
        }

        let n = data.offset - self.data.offset;
        let cap = self.data.buf.capacity() as u128;
        if n > cap {
            self.data.buf.extend_lines(data.buf.iter());
            self.data.offset = data.offset;
            return;
        }

        self.data.buf.extend_lines(data.buf.iter_latest(n as usize));
        self.data.offset = data.offset;
    }
}

#[derive(Clone)]
pub struct LogWriter {
    inner: Arc<RwLock<LogInner>>,
}

pub struct Log {
    inner: Arc<RwLock<LogInner>>,
}
