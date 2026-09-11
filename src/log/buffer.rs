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

impl LogBuffer {
    /// Start reading into `writer`'s log.
    ///
    /// Takes the chunk this reader will fill from the log's arena, so after
    /// this the task allocates nothing however much output it carries.
    ///
    /// The chunk comes from the log's pool, or from the heap if the pool is
    /// spent — either way there is always one.
    pub fn new(writer: LogWriterRef) -> Self {
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
        self.flush_chunk().await;
    }

    /// Hand over a chunk holding text, whether or not it filled up.
    ///
    /// Packing only pays while output arrives faster than it is read. A chunk
    /// left waiting for company would hold a quiet process's last line out of
    /// the log until it happened to say something else — which for a tailer
    /// is the line that matters most. So a chunk goes at the end of every
    /// read: a burst fills one several times over and packs, a trickle sends
    /// one line at a time and packs nothing, and neither has to be detected.
    async fn flush_chunk(&mut self) -> bool {
        if self.chunk.is_empty() {
            return true;
        }
        self.writer.push_chunk(&mut self.chunk).await
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
            self.flush_chunk().await;
            self.buf_offset = 0;
            return Ok(false);
        }

        self.flush_chunk().await;

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
impl Drop for LogBuffer {
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

#[cfg(test)]
mod test {
    use super::*;
    use crate::log::{
        Log, LogWriterId,
        chunk::{LOG_CHUNK_LIMIT, LOG_CHUNK_SIZE},
        line::LOG_LINE_SIZE,
    };

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

    /// The point of packing: lines that arrive together travel together, so
    /// the buffer a short line used to have to itself now carries several.
    #[tokio::test]
    async fn lines_arriving_together_share_a_chunk() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write(b"um\ndois\ntres\nquatro\n").await;
        settle().await;

        // Two per chunk, and every piece ends a line.
        assert_eq!(
            log.chunk_pieces(),
            [
                vec![("um".to_string(), true), ("dois".to_string(), true)],
                vec![("tres".to_string(), true), ("quatro".to_string(), true)],
            ]
        );
        assert_eq!(log.collect_lines(), ["um", "dois", "tres", "quatro"]);
    }

    /// And the other half of that bargain: a line with nobody to share with
    /// must not wait. A chunk held back for company would keep a quiet
    /// process's last line out of the log until it spoke again.
    #[tokio::test]
    async fn a_line_arriving_alone_does_not_wait_for_company() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());

        buffer.write(b"sozinha\n").await;
        settle().await;
        assert_eq!(log.collect_lines(), ["sozinha"]);
        assert_eq!(log.chunk_pieces(), [vec![("sozinha".to_string(), true)]]);
    }

    /// Past the watermark the chunk starts no further line, even with a slot
    /// still free — otherwise the next line would be split across two chunks
    /// to use up a handful of leftover bytes.
    #[tokio::test]
    async fn a_chunk_past_the_watermark_takes_no_new_line() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());
        let longa = "x".repeat(LOG_CHUNK_LIMIT + 8);

        buffer.write(format!("{longa}\ncurta\n").as_bytes()).await;
        settle().await;

        assert_eq!(
            log.chunk_pieces(),
            [
                vec![(longa, true)],
                vec![("curta".to_string(), true)],
            ]
        );
    }

    /// A line cut by the room running out reads as `Partial`, and the piece
    /// that finishes it reads as a `Line` — that pair is what a reader joins
    /// on.
    #[tokio::test]
    async fn a_split_line_is_a_partial_then_a_line() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());
        let longa = "y".repeat(LOG_CHUNK_SIZE + 40);

        buffer.write(format!("{longa}\n").as_bytes()).await;
        settle().await;

        let pieces = log.chunk_pieces();
        assert_eq!(pieces.len(), 2, "expected the line to span two chunks");
        assert_eq!(pieces[0], [("y".repeat(LOG_CHUNK_SIZE), false)]);
        assert_eq!(pieces[1], [("y".repeat(40), true)]);
        assert_eq!(log.collect_lines(), [longa]);
    }

    /// An empty line stores nothing but still takes a slot, and still has to
    /// come back.
    #[tokio::test]
    async fn empty_lines_are_kept() {
        let log = Log::new(16);
        let mut buffer = LogBuffer::new(log.writer());
        buffer.write(b"\n\na\n").await;
        settle().await;

        assert_eq!(log.collect_lines(), ["", "", "a"]);
    }

    /// A unit makes a new reader for every run, and the arena hands blocks
    /// out without taking them back — so a reader that kept its block would
    /// cost one per restart. The heap fallback means it would still work, but
    /// silently: every run after the first few dozen would sit off on its
    /// own, and the whole point of the arena would quietly drain away.
    ///
    /// Giving the chunk back on drop is what keeps the cost per restart at
    /// zero, so this asserts the blocks stay pooled rather than merely that
    /// the log kept working.
    #[tokio::test]
    async fn restarts_do_not_drain_the_arena() {
        let log = Log::new(16);
        for run in 1..=1_000 {
            let mut buffer = LogBuffer::new(log.writer());
            assert!(
                buffer.block_index().is_some(),
                "run {run} fell back to the heap: the pool drained"
            );
            buffer.write(b"linha\n").await;
            // The reader finishes and its buffer falls, as on a restart.
        }
        settle().await;
        assert_eq!(log.collect_lines().len(), 16, "the ring should be full");
    }

    /// A line the writer never finished dies with its reader, and the reader
    /// that follows starts on a clean chunk.
    ///
    /// The unterminated text is held by the line, not the chunk — a line with
    /// no newline is never ready, so it is never placed — which is why it
    /// goes rather than turning up in the middle of the next run's output.
    #[tokio::test]
    async fn an_unfinished_line_does_not_survive_its_reader() {
        let log = Log::new(16);

        let mut primeiro = LogBuffer::new(log.writer());
        primeiro.write(b"inacabada").await;
        drop(primeiro);

        let mut segundo = LogBuffer::new(log.writer());
        segundo.write(b"nova\n").await;
        settle().await;

        assert_eq!(
            log.collect_lines(),
            ["nova"],
            "one run's leftovers turned up in the next"
        );
    }

    /// The chunk really is reused rather than a fresh block being taken —
    /// which is the whole reason the pool exists.
    #[tokio::test]
    async fn a_reader_reuses_the_block_the_last_one_gave_back() {
        let log = Log::new(4);

        let primeiro = LogBuffer::new(log.writer());
        let bloco = primeiro.block_index();
        drop(primeiro);

        let segundo = LogBuffer::new(log.writer());
        assert_eq!(
            segundo.block_index(),
            bloco,
            "a returned block was not picked up again"
        );
    }

    /// Two processes, one printing a line too long for a chunk, the other
    /// writing between its pieces.
    ///
    /// A split line is continued by *that writer's* next chunk, which is not
    /// the next chunk in the ring when something else got there first. A
    /// reader joining on position alone swallowed the other process's line
    /// into the middle of the long one and lost it — and blamed the wrong
    /// writer for the result.
    #[tokio::test]
    async fn a_split_line_is_not_spliced_with_another_writers() {
        let log = Log::new(16);
        let mut longo = LogBuffer::new(log.writer());
        let mut curto = LogBuffer::new(log.writer());

        // Uma leitura entrega mais que o buffer de linha sem fechar a linha,
        // entao ela sai como fragmento e o ultimo chunk fica Partial.
        let parte1 = "L".repeat(LOG_LINE_SIZE + 64);
        let mut src = parte1.as_bytes();
        longo.read(&mut src).await.unwrap();

        // O outro processo escreve no meio.
        curto.write(b"do outro\n").await;

        // E o longo termina a linha dele.
        let mut src = &b"FIM\n"[..];
        longo.read(&mut src).await.unwrap();
        settle().await;

        let linhas = log.collect_attributed();
        let a = LogWriterId::new(0);
        let b = LogWriterId::new(1);

        // A linha do outro processo tem que sair inteira e dele.
        assert!(
            linhas.contains(&(b, "do outro".to_string())),
            "a linha do segundo writer foi engolida: {:?}",
            linhas.iter().map(|(w, t)| (w, t.len())).collect::<Vec<_>>()
        );
        // E a linha longa tem que ser so dela mesma.
        let (writer, longa) = linhas.iter().find(|(_, t)| t.len() > 200).unwrap();
        assert_eq!(*writer, a);
        assert!(
            longa.chars().all(|c| c == 'L') || longa.ends_with("FIM"),
            "a linha longa levou texto de outro writer junto"
        );
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
