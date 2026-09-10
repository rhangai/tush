pub const LOG_CHUNK_SIZE: usize = 4096;

/// Internal log chunk, contains a pointer to the data and performs operations on bytes
pub struct LogChunk {
    buf: Box<[u8; LOG_CHUNK_SIZE]>,
    len: usize,
}

impl LogChunk {
    pub(super) fn new() -> Self {
        Self {
            buf: unsafe { Box::<[u8; LOG_CHUNK_SIZE]>::new_zeroed().assume_init() },
            len: 0,
        }
    }

    pub fn swap(&mut self, other: &mut LogChunk) {
        std::mem::swap(self, other);
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Check if the chunk is finished, that means, or a new line was found
    /// or there is no more room
    pub fn is_finished(&self) -> bool {
        todo!();
    }

    /// Consumes n bytes from the buffer, returns how many bytes was consumed
    /// so the parent can handle
    ///
    /// If it finds a '\n', also mark as completed
    pub fn write(&mut self, buf: &[u8]) -> usize {
        todo!();
    }

    pub fn as_str(&self) -> &str {
        // SAFETY: Since len is only incremented when a valid utf8 is consumed
        // there is no need to worry about this conversion
        unsafe { std::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }
}
