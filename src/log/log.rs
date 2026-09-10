use std::sync::{Arc, Weak};

use parking_lot::RwLock;

use crate::{log::chunk::LogChunk, util::localring::LocalRingBuffer};

/// Main log structure
pub struct LogWriterRef {
    inner: Weak<LogInner>,
}

impl LogWriterRef {
    pub(crate) fn push_chunk(&mut self, chunk: &mut LogChunk) {
        //
    }
}

/// Main log structure
pub struct Log {
    inner: Arc<LogInner>,
}

struct LogInner {
    chunks: RwLock<LocalRingBuffer<LogChunk>>,
}
