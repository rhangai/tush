use tokio::io::{AsyncRead, AsyncReadExt};

use crate::log::{chunk::LOG_CHUNK_SIZE, log::LogWriterRef};

use super::chunk::LogChunk;

const LOG_BUFFER_SIZE: usize = LOG_CHUNK_SIZE * 2;

/// Log buffer, for single task operations
pub struct LogBuffer {
    writer: LogWriterRef,
    chunk: LogChunk,
    buf: Box<[u8; LOG_BUFFER_SIZE]>,
    buf_offset: usize,
}

impl LogBuffer {
    pub fn new(writer: LogWriterRef) -> Self {
        Self {
            writer,
            chunk: LogChunk::new(),
            buf: unsafe { Box::<[u8; LOG_BUFFER_SIZE]>::new_zeroed().assume_init() },
            buf_offset: 0,
        }
    }

    async fn read<R>(&mut self, mut read: R) -> bool
    where
        R: AsyncRead + Unpin,
    {
        // TODO: Must consume buf and pass it to log
        let n = read.read(self.buf.as_mut_slice()).await;
        todo!("Implementar");
        true
    }
}

struct LogBufferBuf {
    buf: Box<[u8]>,
    offset: usize,
}
