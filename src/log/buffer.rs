use std::{
    io::{self, ErrorKind},
    mem::ManuallyDrop,
};

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::log::log::LogWriterRef;

use super::{chunk::LogChunk, line::LogBufferLine};

/// How much one [`read`](LogBuffer::read) can take from the pipe at a time.
///
/// Deliberately unrelated to [`LOG_CHUNK_SIZE`](super::chunk::LOG_CHUNK_SIZE):
/// this sizes a syscall, that sizes a unit of storage. Bigger means fewer
/// reads for a chatty process, and one read simply fills as many chunks as it
/// turns out to cover. There is one of these per reader task, not per log.
const LOG_BUFFER_SIZE: usize = 4096;

pub trait LogBufferWriter {
    fn chunk(&mut self) -> LogChunk;
    fn recycle(&mut self, chunk: LogChunk);
    fn push_chunk(&mut self, chunk: &mut LogChunk) -> bool;
}

impl LogBufferWriter for LogWriterRef {
    fn chunk(&mut self) -> LogChunk {
        LogWriterRef::chunk(self)
    }
    fn recycle(&mut self, chunk: LogChunk) {
        LogWriterRef::recycle(self, chunk)
    }
    fn push_chunk(&mut self, chunk: &mut LogChunk) -> bool {
        LogWriterRef::push_chunk(self, chunk)
    }
}

/// Specialization of LogBuffer for LogWriterRef
pub type LogBuffer = LogBufferAny<LogWriterRef>;

/// The reading end of one pipe, owned by the task draining it.
///
/// Sits between a child's stdout and a [`Log`](super::Log), and exists for
/// the two things a raw read cannot do by itself: a read lands wherever the
/// pipe happened to break, so it may stop in the middle of a character, and
/// it may cover many lines at once. This holds the leftover bytes of an
/// unfinished character between calls and feeds whole characters to a chunk,
/// handing each chunk over as it finishes.
///
/// One per reader task. Nothing here is shared, which is why there is no
/// locking on this side at all — the hand-off to the log is the only place
/// two tasks meet.
pub struct LogBufferAny<W: LogBufferWriter> {
    /// Where finished chunks go. Weak, so this outliving its log is normal.
    writer: W,
    /// The line being assembled. One per buffer, because a partly built line
    /// belongs to the pipe it is being read from and to nothing else.
    line: LogBufferLine,
    /// The chunk being filled. Swapped for a recycled one on every push, so
    /// the same block is reused for the life of the task.
    ///
    /// Wrapped so that [`Drop`] can hand it back: a chunk cannot be moved
    /// out of `&mut self` by ordinary means, and letting it fall would spend
    /// an arena block for good on every run of a unit. `ManuallyDrop` keeps
    /// the field reachable as a plain chunk — it derefs — where an `Option`
    /// would put an unwrap at every use for a state that only exists for the
    /// instant between the take and the end of the drop.
    chunk: ManuallyDrop<LogChunk>,
    /// Landing area for the raw read, big enough that one syscall is worth
    /// making.
    buf: Box<[u8; LOG_BUFFER_SIZE]>,
    /// How many bytes at the front of `buf` are the head of a character the
    /// last read cut in half. Never more than three; usually zero, since any
    /// input ending on a character boundary — all of ASCII — leaves nothing
    /// behind. The next read is placed after them.
    buf_offset: usize,
}

impl<W: LogBufferWriter> LogBufferAny<W> {
    /// Start reading into `writer`'s log.
    ///
    /// Takes the chunk this reader will fill from the log's arena, so after
    /// this the task allocates nothing however much output it carries.
    ///
    /// The chunk comes from the log's pool, or from the heap if the pool is
    /// spent — either way there is always one.
    pub fn new(mut writer: W) -> Self {
        Self {
            chunk: ManuallyDrop::new(writer.chunk()),
            writer,
            line: LogBufferLine::new(),
            buf: unsafe { Box::<[u8; LOG_BUFFER_SIZE]>::new_zeroed().assume_init() },
            buf_offset: 0,
        }
    }

    /// Which arena block this reader's chunk sits on, for tests that care
    /// that a block came back rather than a new one being taken.
    #[cfg(test)]
    fn block_index(&self) -> Option<u32> {
        self.chunk.block_index()
    }

    /// Push bytes in directly, without a reader.
    ///
    /// For output that is already in memory and already whole — a
    /// supervisor's own notices ("starting", "exited with 1"), not pipe
    /// traffic. Splits on newlines and hands over every chunk it fills, the
    /// same as [`read`](LogBuffer::read) does.
    ///
    /// An unfinished line stays in the current chunk for the next call to
    /// continue, so writing `"sta"` then `"rting\n"` logs one line. What does
    /// *not* carry over is a partial character: one call is one whole
    /// message, so the line is fed as end of input and a trailing `\xc3`
    /// becomes a replacement character rather than being held back for a call
    /// that may never come. Split text on a character boundary, or use
    /// [`read`](LogBuffer::read), which does carry a partial character across
    /// reads.
    ///
    /// Feeding it that way is also what keeps the loop moving: plain
    /// [`write`](LogBufferLine::write) would hold a partial tail back and
    /// take none of it, leaving `buf` the same length on every pass — and
    /// nothing on this path yields, so the spin would wedge the thread
    /// rather than just this task.
    ///
    /// Returns early, dropping the rest, once the log is gone.
    pub fn write(&mut self, mut buf: &[u8]) {
        while !buf.is_empty() {
            buf = &buf[self.line.write_eof(buf)..];
            if self.line.is_ready() && !self.flush_line() {
                return;
            }
        }
        self.flush_chunk();
    }

    /// Hand over a chunk holding text, whether or not it filled up.
    ///
    /// Packing only pays while output arrives faster than it is read. A chunk
    /// left waiting for company would hold a quiet process's last line out of
    /// the log until it happened to say something else — which for a tailer
    /// is the line that matters most. So a chunk goes at the end of every
    /// read: a burst fills one several times over and packs, a trickle sends
    /// one line at a time and packs nothing, and neither has to be detected.
    ///
    /// Once it goes it is in the ring, visible to any reader that looks
    /// next — there is no stage between here and the history.
    fn flush_chunk(&mut self) -> bool {
        if self.chunk.is_empty() {
            return true;
        }
        self.writer.push_chunk(&mut self.chunk)
    }

    /// Hand the finished line to the log, then clear it.
    ///
    /// A line can outgrow a chunk, so this loops: each pass places what fits,
    /// and a chunk that filled up goes to the log and comes back empty for
    /// the rest. Only the pass that places the last of the text marks the
    /// chunk a finished line, which is what keeps a split line readable as
    /// one afterwards.
    ///
    /// Returns `false` once the log is gone: there is nowhere left to put
    /// anything, and no reason for the caller to carry on.
    fn flush_line(&mut self) -> bool {
        loop {
            self.chunk.push_line(&mut self.line);
            if self.chunk.is_finished() {
                let is_open = self.writer.push_chunk(&mut self.chunk);
                if !is_open {
                    return false;
                }
            }
            if self.line.is_drained() {
                break;
            }
        }
        self.line.clear();
        true
    }

    /// Read once and hand every line it completes to the log.
    ///
    /// - `Ok(true)` — call again.
    /// - `Ok(false)` — stop calling. Either the stream ended, in which case
    ///   whatever was still held has been flushed, or the log was dropped and
    ///   there is nothing left to read into; the two are not distinguished,
    ///   because the caller's move is the same for both.
    ///
    ///   In the second case the partly filled chunk is discarded rather than
    ///   flushed, and the pipe is left undrained — dropping the reader closes
    ///   it, so a process still writing gets a `SIGPIPE`. That is a backstop,
    ///   not the way a run is meant to end; see
    ///   [`LogWriterRef`](super::LogWriterRef).
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
    pub async fn read<R>(&mut self, read: &mut R) -> io::Result<bool>
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
        while start < filled {
            start += if ended {
                self.line.write_eof(&self.buf[start..filled])
            } else {
                self.line.write(&self.buf[start..filled])
            };
            if !self.line.is_ready() {
                // Either the read is spent, or what is left is the head of a
                // character the next one finishes. Both mean stop here.
                break;
            }
            if !self.flush_line() {
                self.buf_offset = 0;
                return Ok(false);
            }
        }

        if ended {
            // Nothing more is coming, so a last line with no newline would
            // otherwise sit here for ever.
            self.line.seal();
            if !self.line.is_empty() {
                self.flush_line();
            }
            // And a chunk still holding text has no later line to close it.
            self.flush_chunk();
            self.buf_offset = 0;
            return Ok(false);
        }

        self.flush_chunk();

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

/// Hand the reader's chunk back when the task that owned it is done.
///
/// A block spent here is spent for good — the arena hands out and does not
/// take back — and a unit makes a new reader for every run, so without this a
/// log restarted often enough would run out of blocks and stop recording.
///
/// Drop rather than an explicit call on purpose: it has to happen whether the
/// reader ended because the pipe closed or because the task was cancelled out
/// from under it, and only `Drop` covers both.
impl<W: LogBufferWriter> Drop for LogBufferAny<W> {
    fn drop(&mut self) {
        // SAFETY: the one and only take. `drop` runs once, nothing reads
        // `chunk` after this, and `ManuallyDrop` means the field is not
        // dropped again on the way out — so the chunk is moved exactly once
        // and destroyed exactly once, by `recycle`.
        let chunk = unsafe { ManuallyDrop::take(&mut self.chunk) };
        self.writer.recycle(chunk);
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
