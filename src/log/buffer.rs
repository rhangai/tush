use std::io::{self, ErrorKind};

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

    /// Read once and hand every line it completes to the log.
    ///
    /// - `Ok(true)` — call again.
    /// - `Ok(false)` — the stream is over; whatever was still held has been
    ///   flushed.
    /// - `Err` — something went wrong that reading cannot make sense of,
    ///   for the caller to decide about. The error comes back untouched, so
    ///   its [`kind`](io::Error::kind) is still there to match on.
    ///
    /// # Which errors are which
    ///
    /// A child's output normally ends by the other end disappearing, so a
    /// broken pipe or a reset connection is reported as the end of the
    /// stream, not as a failure — the process's exit status is what carries
    /// that news. `Interrupted` means a signal cut the syscall short before
    /// anything was read, so it asks for another call. Anything else is
    /// genuinely unexpected and comes back as `Err`, leaving the buffer
    /// untouched so a caller that wants to retry can.
    ///
    /// `WouldBlock` is deliberately *not* in that list. An `AsyncRead` is
    /// supposed to answer "not yet" with `Poll::Pending`, and tokio's own
    /// readers turn the syscall's `WouldBlock` into exactly that before it
    /// could reach here. A reader that returns it as an error is broken,
    /// and saying "call again" to that would spin the caller at full tilt;
    /// surfacing it is the honest answer.
    ///
    /// # What carries over
    ///
    /// Reading lands wherever the pipe happens to break, which is regularly
    /// in the middle of a character. Those bytes stay in `buf` and the next
    /// read appends after them, so the chunk only ever sees whole
    /// characters. At most three bytes are ever held: past four, an
    /// undecodable sequence is broken rather than unfinished, and the chunk
    /// takes it as replacement characters.
    pub async fn read<R>(&mut self, mut read: R) -> io::Result<bool>
    where
        R: AsyncRead + Unpin,
    {
        // Read in after the partial character the last call held back.
        let (filled, ended) = match read.read(&mut self.buf[self.buf_offset..]).await {
            Ok(0) => (self.buf_offset, true),
            Ok(n) => (self.buf_offset + n, false),
            // ErrorKind::Interrupted means you can try again immediatly
            Err(error) if error.kind() == ErrorKind::Interrupted => return Ok(true),
            // The far end went away, which is how a child's output ends.
            Err(error) if is_disconnected(&error) => (self.buf_offset, true),
            Err(error) => return Err(error),
        };

        // Feed the chunk until it stops taking bytes. Each finished chunk
        // goes to the log and comes back cleared, so the loop can carry on
        // with the same one.
        let mut start = 0;
        loop {
            start += if ended {
                self.chunk.write_eof(&self.buf[start..filled])
            } else {
                self.chunk.write(&self.buf[start..filled])
            };
            if !self.chunk.is_finished() {
                break;
            }
            self.writer.push_chunk(&mut self.chunk);
        }

        if ended {
            // Nothing more is coming, so a last line with no newline would
            // otherwise sit here for ever.
            if !self.chunk.is_empty() {
                self.writer.push_chunk(&mut self.chunk);
            }
            self.buf_offset = 0;
            return Ok(false);
        }

        // Whatever the chunk would not take is the head of a character the
        // next read will finish — at most three bytes, and usually none at
        // all, since input ending on a character boundary (all of ASCII)
        // leaves nothing behind. Worth the test to skip the move entirely
        // in that case.
        self.buf_offset = filled - start;
        if self.buf_offset > 0 {
            self.buf.copy_within(start..filled, 0);
        }
        Ok(true)
    }
}

/// Errors that mean the other end is gone.
///
/// For a child process this is the ordinary way output stops — the pipe
/// closes when it exits, and a pty reports the same thing as a reset. None
/// of it is a failure of the reading itself.
fn is_disconnected(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
    )
}

struct LogBufferBuf {
    buf: Box<[u8]>,
    offset: usize,
}
