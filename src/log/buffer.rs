use std::io::{self, ErrorKind};

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
pub struct LogBuffer {
    /// Where finished chunks go. Weak, so this outliving its log is normal.
    writer: LogWriterRef,
    /// The line being assembled. One per buffer, because a partly built line
    /// belongs to the pipe it is being read from and to nothing else.
    line: LogBufferLine,
    /// The chunk being filled. Swapped for a recycled one on every push, so
    /// the same allocation is reused for the life of the task.
    chunk: LogChunk,
    /// Landing area for the raw read, big enough that one syscall is worth
    /// making.
    buf: Box<[u8; LOG_BUFFER_SIZE]>,
    /// How many bytes at the front of `buf` are the head of a character the
    /// last read cut in half. Never more than three; usually zero, since any
    /// input ending on a character boundary — all of ASCII — leaves nothing
    /// behind. The next read is placed after them.
    buf_offset: usize,
}

impl LogBuffer {
    /// Start reading into `writer`'s log.
    ///
    /// Allocates both buffers up front; after this a reader task allocates
    /// nothing, no matter how much output it carries.
    pub fn new(writer: LogWriterRef) -> Self {
        Self {
            writer,
            line: LogBufferLine::new(),
            chunk: LogChunk::new(),
            buf: unsafe { Box::<[u8; LOG_BUFFER_SIZE]>::new_zeroed().assume_init() },
            buf_offset: 0,
        }
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
    /// with no `await` on that path, the spin would wedge the whole runtime
    /// thread rather than just this task.
    ///
    /// Returns early, dropping the rest, once the log is gone.
    pub async fn write(&mut self, mut buf: &[u8]) {
        while !buf.is_empty() {
            buf = &buf[self.line.write_eof(buf)..];
            if self.line.is_ready() && !self.flush_line().await {
                return;
            }
        }
    }

    /// Hand the finished line to the log, then clear it.
    ///
    /// A line can outgrow a chunk, so this loops: each pass places what fits,
    /// and a chunk that filled up goes to the log and comes back empty for
    /// the rest. Only the pass that places the last of the text marks the
    /// chunk a finished line, which is what keeps a split line readable as
    /// one afterwards.
    ///
    /// Returns `false` once the log is gone. There is nowhere left to put
    /// anything then, and a push that fails that way never yields, so a
    /// caller that carried on would spin.
    async fn flush_line(&mut self) -> bool {
        loop {
            self.chunk.push_line(&mut self.line);
            if self.chunk.is_finished() {
                let is_open = self.writer.push_chunk(&mut self.chunk).await;
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
            if !self.flush_line().await {
                self.buf_offset = 0;
                return Ok(false);
            }
        }

        if ended {
            // Nothing more is coming, so a last line with no newline would
            // otherwise sit here for ever.
            self.line.seal();
            if !self.line.is_empty() {
                self.flush_line().await;
            }
            // And a chunk still holding text has no later line to close it.
            if !self.chunk.is_empty() {
                self.writer.push_chunk(&mut self.chunk).await;
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

#[cfg(test)]
mod test {
    use super::*;
    use crate::log::{Log, LogWriterId, chunk::LOG_CHUNK_SIZE};

    /// Let the sync task move what was pushed into the ring.
    async fn settle() {
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
    }

    #[tokio::test]
    async fn write_keeps_every_line_in_the_buffer() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());
        // Regression: the loop used to advance by a count it never read, so
        // everything after the first newline was dropped.
        buffer.write(b"um\ndois\ntres\n").await;
        settle().await;
        assert_eq!(log.collect_lines(), ["um", "dois", "tres"]);
    }

    #[tokio::test]
    async fn write_carries_an_unfinished_line_between_calls() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write(b"sta").await;
        buffer.write(b"rting\nup\n").await;
        settle().await;
        assert_eq!(log.collect_lines(), ["starting", "up"]);
    }

    /// A partial character at the end of a `write` has no next call to
    /// complete it. It has to become a replacement character — holding it
    /// back took no bytes and spun the loop for ever, with no `await` on the
    /// path to even let the runtime interrupt it.
    #[tokio::test]
    async fn write_terminates_on_a_partial_character() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write("olá".as_bytes()).await;
        buffer.write(b"ol\xc3").await;
        buffer.write(b"\n").await;
        settle().await;
        assert_eq!(log.collect_lines(), ["oláol\u{fffd}"]);
    }

    /// The other spin: with the log gone the chunk is never swapped, so it
    /// stays finished, takes nothing, and the failed push never yields.
    #[tokio::test]
    async fn write_terminates_once_the_log_is_dropped() {
        let log = Log::new(16);
        let writer = log.writer();
        drop(log);
        let mut buffer = LogBuffer::new(writer);
        buffer.write(&b"linha\n".repeat(500)).await;
    }

    /// Each writer gets its own id, and it is dense from zero so a renderer
    /// can index by it.
    #[tokio::test]
    async fn writers_are_numbered_from_zero() {
        let log = Log::new(16);
        let ids: Vec<_> = (0..3).map(|_| log.writer().id()).collect();
        assert_eq!(
            ids,
            [
                LogWriterId::new(0),
                LogWriterId::new(1),
                LogWriterId::new(2)
            ]
        );
        assert_eq!(ids[2].index(), 2);
    }

    /// The point of the stamp: two processes writing into one log stay
    /// tellable apart once their lines are interleaved in the ring.
    #[tokio::test]
    async fn interleaved_writers_keep_their_lines_apart() {
        let log = Log::new(16);
        let mut um = LogBuffer::new(log.writer());
        let mut dois = LogBuffer::new(log.writer());

        um.write(b"um-a\n").await;
        dois.write(b"dois-a\n").await;
        um.write(b"um-b\n").await;
        dois.write(b"dois-b\n").await;
        settle().await;

        let a = LogWriterId::new(0);
        let b = LogWriterId::new(1);
        assert_eq!(
            log.collect_attributed(),
            [
                (a, "um-a".to_string()),
                (b, "dois-a".to_string()),
                (a, "um-b".to_string()),
                (b, "dois-b".to_string()),
            ]
        );
    }

    /// A line split across chunks is one writer's, all the way through.
    #[tokio::test]
    async fn a_split_line_carries_one_stamp() {
        let log = Log::new(64);
        let mut buffer = LogBuffer::new(log.writer());
        let longa = "z".repeat(LOG_CHUNK_SIZE * 3);
        buffer.write(format!("{longa}\n").as_bytes()).await;
        settle().await;

        assert_eq!(
            log.collect_attributed(),
            [(LogWriterId::new(0), longa)]
        );
    }

    /// A line too long even for the line buffer leaves it in fragments, and
    /// each fragment is then spread over chunks. Both boundaries are crossed
    /// at once, and the line still has to come back whole.
    #[tokio::test]
    async fn a_line_longer_than_the_line_buffer_survives_both_splits() {
        let log = Log::new(256);
        let mut buffer = LogBuffer::new(log.writer());
        // Not a multiple of either size, so no boundary lines up.
        let longa = "abcdefghij".repeat(937);
        assert!(longa.len() > LOG_CHUNK_SIZE * 4, "should cross many chunks");

        buffer.write(format!("{longa}\ndepois\n").as_bytes()).await;
        settle().await;

        assert_eq!(
            log.collect_attributed(),
            [
                (LogWriterId::new(0), longa),
                (LogWriterId::new(0), "depois".to_string()),
            ]
        );
    }

    /// The same through `read`, where the line is also cut by wherever the
    /// reads happen to land.
    #[tokio::test]
    async fn a_very_long_line_survives_arbitrary_read_boundaries() {
        let log = Log::new(256);
        let mut buffer = LogBuffer::new(log.writer());
        let longa = "0123456789áé".repeat(311);
        let texto = format!("{longa}\ncurta\n");

        let bytes = texto.as_bytes();
        let mut src = bytes;
        while buffer.read(&mut src).await.unwrap() {}
        settle().await;

        assert_eq!(log.collect_lines(), [longa, "curta".to_string()]);
    }

    /// Every chunk must be valid UTF-8 **on its own**, because
    /// [`as_str`](super::chunk::LogChunk::as_str) hands it to
    /// `from_utf8_unchecked`.
    ///
    /// The line guarantees its own content is valid, but it is not the line
    /// that chooses where a chunk cuts — the chunk takes whatever its
    /// remaining room allows, and that offset knows nothing about character
    /// boundaries. Here the first chunk fills to one byte short of a two byte
    /// character, so a cut at the raw capacity would slice it in half.
    ///
    /// Note the joined history stays correct either way: the two halves
    /// concatenate back to the same bytes. That is exactly why this asserts
    /// per chunk — the damage is invisible from the joined output and only
    /// shows in a renderer that reads one chunk, or in the `unsafe`.
    #[tokio::test]
    async fn every_chunk_is_valid_utf8_on_its_own() {
        let log = Log::new(64);
        let mut buffer = LogBuffer::new(log.writer());

        // 255 ASCII bytes leave a single byte of room, and the next
        // character needs two.
        let linha = format!("{}{}", "a".repeat(LOG_CHUNK_SIZE - 1), "á".repeat(64));
        buffer.write(format!("{linha}\n").as_bytes()).await;
        settle().await;

        for (i, bytes) in log.chunk_bytes().iter().enumerate() {
            assert!(
                std::str::from_utf8(bytes).is_ok(),
                "chunk {i} is not valid UTF-8 on its own: {:x?}",
                &bytes[bytes.len().saturating_sub(8)..]
            );
        }
        assert_eq!(log.collect_lines(), [linha]);
    }

    #[tokio::test]
    async fn read_splits_a_line_longer_than_a_chunk() {
        let log = Log::new(64);
        let mut buffer = LogBuffer::new(log.writer());
        let long = "a".repeat(LOG_CHUNK_SIZE * 3);
        let src = format!("{long}\ncurta\n").into_bytes();
        let mut src = &src[..];
        while buffer.read(&mut src).await.unwrap() {}
        settle().await;
        assert_eq!(log.collect_lines(), [long, "curta".to_string()]);
    }

    /// A character straddling the boundary between two reads must survive.
    #[tokio::test]
    async fn read_rejoins_a_character_split_across_reads() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());
        let text = "coração\n".as_bytes();
        // Hand it over one byte at a time: every multi-byte character is
        // guaranteed to be cut.
        for i in 0..text.len() {
            let mut one = &text[i..i + 1];
            buffer.read(&mut one).await.unwrap();
        }
        settle().await;
        assert_eq!(log.collect_lines(), ["coração"]);
    }
}
