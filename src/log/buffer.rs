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

/// Where a [`LogBufferAny`] gets its chunks and where it sends them.
///
/// Everything the buffer needs of a log, and nothing else: it never reads the
/// history, never locks anything, never knows how many chunks exist. Naming
/// that narrow surface is what lets the assembling — splitting bytes into
/// lines, packing lines into chunks, carrying a character across a read — be
/// exercised against a stand in, with no arena, no ring and no runtime
/// underneath.
///
/// # The contract
///
/// [`push_chunk`](LogBufferWriter::push_chunk) takes the chunk's contents and
/// leaves an empty one in its place. It does not take the chunk: the buffer
/// keeps writing into whatever comes back, so an implementation that forgets
/// to clear leaves it marked finished and every later push places nothing.
///
/// A block handed out by [`chunk`](LogBufferWriter::chunk) is expected back
/// through [`recycle`](LogBufferWriter::recycle) when the buffer is done, or
/// it is spent for good.
pub trait LogBufferWriter {
    /// A chunk for the buffer to fill. There is always one — a log short of
    /// pooled blocks falls back to the heap rather than refusing.
    fn chunk(&mut self) -> LogChunk;

    /// Take a chunk the buffer has finished with, at the end of its life.
    fn recycle(&mut self, chunk: LogChunk);

    /// Take what `chunk` holds and leave it empty and open.
    ///
    /// `false` once there is nowhere left to put anything, which is the
    /// buffer's cue to stop rather than to retry.
    fn push_chunk(&mut self, chunk: &mut LogChunk) -> bool;

    /// Take `text` as the line being written, which is not finished yet.
    fn set_partial(&mut self, text: &str);

    /// Forget the line being written, because it ended or because the reader
    /// did.
    fn clear_partial(&mut self);
}

/// The real one: chunks come from the log's arena and go into its ring.
///
/// Plain forwarding. The inherent methods carry the reasoning; this only says
/// that a writer into a [`Log`](super::Log) is what the buffer was shaped
/// around.
impl LogBufferWriter for LogWriterRef {
    fn chunk(&mut self) -> LogChunk {
        LogWriterRef::chunk(self)
    }

    fn set_partial(&mut self, text: &str) {
        LogWriterRef::set_partial(self, text);
    }

    fn clear_partial(&mut self) {
        LogWriterRef::clear_partial(self);
    }

    fn recycle(&mut self, chunk: LogChunk) {
        LogWriterRef::recycle(self, chunk)
    }

    fn push_chunk(&mut self, chunk: &mut LogChunk) -> bool {
        LogWriterRef::push_chunk(self, chunk)
    }
}

/// A buffer writing into a real [`Log`](super::Log).
///
/// What every caller outside the tests means by a log buffer; the generic
/// form exists so the assembling can be tested without one.
pub type LogBuffer = LogBufferAny<LogWriterRef>;

/// The reading end of one pipe, owned by the task draining it.
///
/// Sits between a child's stdout and whatever stores its output, and exists
/// for the two things a raw read cannot do by itself: a read lands wherever
/// the pipe happened to break, so it may stop in the middle of a character,
/// and it may cover many lines at once. This holds the leftover bytes of an
/// unfinished character between calls and feeds whole characters to a chunk,
/// handing each chunk over as it finishes.
///
/// One per reader task. Nothing here is shared, which is why there is no
/// locking on this side at all — the hand-off through [`LogBufferWriter`] is
/// the only place two tasks meet.
///
/// Generic over that hand-off rather than tied to a [`Log`](super::Log): the
/// work here is all assembling, and assembling is worth testing on its own.
/// [`LogBuffer`] is the form the rest of the crate uses.
pub struct LogBufferAny<W: LogBufferWriter> {
    /// Where chunks come from and where they go. Owned, so a buffer outliving
    /// its log is the writer's problem to answer for and not this one's.
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
        self.publish_partial();
    }

    /// Show the line so far, if there is one and it is not finished.
    ///
    /// At the end of a read rather than as it goes, because what a reader
    /// wants to see is the line as it stands now — and a read that arrives in
    /// three pieces is still one moment as far as anybody watching is
    /// concerned.
    fn publish_partial(&mut self) {
        if self.line.is_ready() || self.line.is_empty() {
            return;
        }
        // Disjoint fields: the line is read while the writer is written to.
        let (line, writer) = (&self.line, &mut self.writer);
        writer.set_partial(line.as_str());
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
        // The line is on its way into the history, so there is no longer
        // anything part way through — whether a newline closed it, the room
        // ran out, or the stream ended and sealed it.
        self.writer.clear_partial();
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
    ///   [`super::LogWriterRef`].
    /// - `Err` — something went wrong that reading cannot make sense of,
    ///   for the caller to decide about. The error comes back untouched, so
    ///   its [`kind`](io::Error::kind) is still there to match on.
    ///
    /// # Which errors are which
    ///
    /// A child's output normally ends by the other end disappearing, so a
    /// broken pipe or a reset connection is the end of the stream and not a
    /// failure — the exit status is what carries that news. `Interrupted`
    /// asks for another call. Anything else comes back as `Err` with the
    /// buffer untouched, so a caller that wants to retry can.
    ///
    /// `WouldBlock` is deliberately not in that list: an `AsyncRead` answers
    /// "not yet" with `Poll::Pending`, so a reader that returns it here is
    /// broken, and "call again" would spin the caller at full tilt.
    ///
    /// # What carries over
    ///
    /// Reading lands wherever the pipe breaks, regularly mid character. Those
    /// bytes stay in `buf` and the next read appends after them, so the chunk
    /// only ever sees whole characters. At most three are held: past four, an
    /// undecodable sequence is broken rather than unfinished, and becomes
    /// replacement characters.
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
        self.publish_partial();

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

    /// Read until done
    pub async fn read_all<R>(&mut self, read: &mut R) -> io::Result<()>
    where
        R: AsyncRead + Unpin,
    {
        loop {
            let result = self.read(read).await?;
            if !result {
                break;
            }
        }
        Ok(())
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
        // Whatever was part way through goes with the task that was writing
        // it. This is the path an aborted reader takes — a unit stopped or
        // restarted mid-line — and the only one that does not run the code
        // that would otherwise retire the partial.
        self.writer.clear_partial();

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
    use crate::{
        log::{
            LogWriterId,
            chunk::{LOG_CHUNK_LIMIT, LOG_CHUNK_SIZE, LogChunkData},
            line::LOG_LINE_SIZE,
        },
        util::arena::ArenaBlock,
    };
    use std::{cell::RefCell, rc::Rc};

    /// What a stand in writer saw, shared with the test that inspects it.
    #[derive(Default)]
    struct Handed {
        /// Chunks pushed, in the order they were handed over.
        chunks: Vec<LogChunk>,
        /// Chunks given back, waiting to be lent again — the real writer's
        /// free pool, in miniature.
        spare: Vec<LogChunk>,
        /// How many blocks were made rather than reused.
        minted: usize,
        /// Whether pushes still land. Set to false to play the log going
        /// away under a reader still holding one.
        open: bool,
        /// The line the buffer says is part way through, if any.
        partial: Option<String>,
    }

    impl Handed {
        /// Shared from the start, since both the writer and the test that
        /// inspects it need a hold.
        fn new() -> Rc<RefCell<Self>> {
            Rc::new(RefCell::new(Self {
                open: true,
                ..Default::default()
            }))
        }
    }

    /// A [`LogBufferWriter`] with nothing behind it.
    ///
    /// Blocks come from the heap, so there is no arena to size and no ring to
    /// drain; pushed chunks are simply kept, which is all a test needs to see.
    struct Fake {
        /// Stamped onto every chunk, so a test with two of these can tell
        /// their output apart the way a real log does.
        id: LogWriterId,
        /// Shared with the test, which is the only way to see what a buffer
        /// handed over: the buffer owns its writer and never gives it back.
        handed: Rc<RefCell<Handed>>,
    }

    impl Fake {
        /// The first process id, for the tests that only have one writer.
        fn new(handed: &Rc<RefCell<Handed>>) -> Self {
            Self::with_id(handed, 2)
        }

        /// A writer with a stamp of its own, sharing one record with the
        /// others — which is how a log with several processes behaves.
        fn with_id(handed: &Rc<RefCell<Handed>>, id: u32) -> Self {
            Self {
                id: LogWriterId::new(id),
                handed: handed.clone(),
            }
        }
    }

    impl LogBufferWriter for Fake {
        /// The partial line, as the writer last saw it. Kept rather than
        /// forwarded anywhere, since a test's whole interest in it is what
        /// the buffer decided to publish and when.
        fn set_partial(&mut self, text: &str) {
            self.handed.borrow_mut().partial = Some(text.to_owned());
        }

        fn clear_partial(&mut self) {
            self.handed.borrow_mut().partial = None;
        }

        fn chunk(&mut self) -> LogChunk {
            let mut handed = self.handed.borrow_mut();
            handed.spare.pop().unwrap_or_else(|| {
                handed.minted += 1;
                LogChunk::new(ArenaBlock::heap())
            })
        }

        fn recycle(&mut self, mut chunk: LogChunk) {
            chunk.clear();
            self.handed.borrow_mut().spare.push(chunk);
        }

        fn push_chunk(&mut self, chunk: &mut LogChunk) -> bool {
            let mut handed = self.handed.borrow_mut();
            if !handed.open {
                return false;
            }
            // The same order the real writer uses: stamp, swap the contents
            // out, hand back something empty and open.
            chunk.set_writer(self.id);
            let mut taken = LogChunk::new(ArenaBlock::heap());
            handed.minted += 1;
            taken.swap(chunk);
            chunk.clear();
            handed.chunks.push(taken);
            true
        }
    }

    /// A buffer on writer zero, for the tests that do not care which.
    fn buffer(handed: &Rc<RefCell<Handed>>) -> LogBufferAny<Fake> {
        LogBufferAny::new(Fake::new(handed))
    }

    /// The pieces of every chunk pushed: `(text, ends a line)`, by chunk.
    ///
    /// Shows the packing itself rather than the history it adds up to, which
    /// is the only way to tell whether lines shared a chunk.
    fn pieces(handed: &Rc<RefCell<Handed>>) -> Vec<Vec<(String, bool)>> {
        handed
            .borrow()
            .chunks
            .iter()
            .map(|chunk| {
                chunk
                    .iter_data()
                    .map(|data| (data.as_str().to_string(), data.is_line()))
                    .collect()
            })
            .collect()
    }

    /// The pushed chunks read back as whole lines, each with its writer.
    ///
    /// Joins per writer, not by position: another process's chunk can sit
    /// between two pieces of the same split line, and joining on position
    /// swallows it. This is the only joiner in the crate, so the rule lives
    /// here and the test below is what holds it.
    fn lines(handed: &Rc<RefCell<Handed>>) -> Vec<(LogWriterId, String)> {
        let handed = handed.borrow();
        let mut lines = Vec::new();
        let mut pending: Vec<(LogWriterId, String)> = Vec::new();

        for chunk in &handed.chunks {
            let writer = chunk.writer();
            let slot = match pending.iter().position(|(id, _)| *id == writer) {
                Some(at) => at,
                None => {
                    pending.push((writer, String::new()));
                    pending.len() - 1
                }
            };
            for data in chunk.iter_data() {
                pending[slot].1.push_str(data.as_str());
                if data.is_line() {
                    lines.push((writer, std::mem::take(&mut pending[slot].1)));
                }
            }
        }
        lines.extend(pending.into_iter().filter(|(_, text)| !text.is_empty()));
        lines
    }

    /// [`lines`] without the stamps, for the tests that only assert content.
    fn texts(handed: &Rc<RefCell<Handed>>) -> Vec<String> {
        lines(handed).into_iter().map(|(_, text)| text).collect()
    }

    // ---- write ----------------------------------------------------------

    #[test]
    fn write_keeps_every_line_in_the_buffer() {
        let handed = Handed::new();
        // Regression: the loop used to advance by a count it never read, so
        // everything after the first newline was dropped.
        buffer(&handed).write(b"um\ndois\ntres\n");
        assert_eq!(texts(&handed), ["um", "dois", "tres"]);
    }

    #[test]
    fn write_carries_an_unfinished_line_between_calls() {
        let handed = Handed::new();
        let mut buffer = buffer(&handed);
        buffer.write(b"sta");
        buffer.write(b"rting\nup\n");
        assert_eq!(texts(&handed), ["starting", "up"]);
    }

    /// A partial character at the end of a `write` has no next call to
    /// complete it. It has to become a replacement character — holding it
    /// back took no bytes and spun the loop for ever, on a path with nothing
    /// to yield to.
    #[test]
    fn write_terminates_on_a_partial_character() {
        let handed = Handed::new();
        let mut buffer = buffer(&handed);
        buffer.write("olá".as_bytes());
        buffer.write(b"ol\xc3");
        buffer.write(b"\n");
        assert_eq!(texts(&handed), ["oláol\u{fffd}"]);
    }

    /// The other spin: with nowhere to push, the chunk is never swapped, so
    /// it stays finished and takes nothing while the failed push never
    /// yields.
    #[test]
    fn write_terminates_once_the_writer_refuses() {
        let handed = Handed::new();
        handed.borrow_mut().open = false;
        buffer(&handed).write(&b"linha\n".repeat(500));
        assert!(handed.borrow().chunks.is_empty());
    }

    #[test]
    fn empty_lines_are_kept() {
        let handed = Handed::new();
        buffer(&handed).write(b"\n\na\n");
        assert_eq!(texts(&handed), ["", "", "a"]);
    }

    // ---- packing --------------------------------------------------------

    /// The point of packing: lines that arrive together travel together, so
    /// the buffer a short line used to have to itself now carries several.
    #[test]
    fn lines_arriving_together_share_a_chunk() {
        let handed = Handed::new();
        buffer(&handed).write(b"um\ndois\ntres\nquatro\n");

        assert_eq!(
            pieces(&handed),
            [
                vec![("um".to_string(), true), ("dois".to_string(), true)],
                vec![("tres".to_string(), true), ("quatro".to_string(), true)],
            ]
        );
        assert_eq!(texts(&handed), ["um", "dois", "tres", "quatro"]);
    }

    /// And the other half of that bargain: a line with nobody to share with
    /// must not wait. A chunk held back for company would keep a quiet
    /// process's last line out of the log until it spoke again.
    #[test]
    fn a_line_arriving_alone_does_not_wait_for_company() {
        let handed = Handed::new();
        buffer(&handed).write(b"sozinha\n");
        assert_eq!(pieces(&handed), [vec![("sozinha".to_string(), true)]]);
    }

    /// Past the watermark the chunk starts no further line, even with a slot
    /// still free — otherwise the next line would be split across two chunks
    /// to use up a handful of leftover bytes.
    #[test]
    fn a_chunk_past_the_watermark_takes_no_new_line() {
        let handed = Handed::new();
        let longa = "x".repeat(LOG_CHUNK_LIMIT + 8);
        buffer(&handed).write(format!("{longa}\ncurta\n").as_bytes());

        assert_eq!(
            pieces(&handed),
            [vec![(longa, true)], vec![("curta".to_string(), true)]]
        );
    }

    /// A line cut by the room running out reads as `Partial`, and the piece
    /// that finishes it reads as a `Line` — that pair is what a reader joins
    /// on.
    #[test]
    fn a_split_line_is_a_partial_then_a_line() {
        let handed = Handed::new();
        let longa = "y".repeat(LOG_CHUNK_SIZE + 40);
        buffer(&handed).write(format!("{longa}\n").as_bytes());

        let pieces = pieces(&handed);
        assert_eq!(pieces.len(), 2, "expected the line to span two chunks");
        assert_eq!(pieces[0], [("y".repeat(LOG_CHUNK_SIZE), false)]);
        assert_eq!(pieces[1], [("y".repeat(40), true)]);
        assert_eq!(texts(&handed), [longa]);
    }

    // ---- attribution ----------------------------------------------------

    #[test]
    fn a_split_line_carries_one_stamp() {
        let handed = Handed::new();
        let longa = "z".repeat(LOG_CHUNK_SIZE * 3);
        buffer(&handed).write(format!("{longa}\n").as_bytes());

        assert_eq!(lines(&handed), [(LogWriterId::new(2), longa)]);
    }

    /// The point of the stamp: two processes writing into one log stay
    /// tellable apart once their chunks are interleaved.
    #[test]
    fn interleaved_writers_keep_their_lines_apart() {
        let handed = Handed::new();
        let mut um = LogBufferAny::new(Fake::with_id(&handed, 2));
        let mut dois = LogBufferAny::new(Fake::with_id(&handed, 3));

        um.write(b"um-a\n");
        dois.write(b"dois-a\n");
        um.write(b"um-b\n");
        dois.write(b"dois-b\n");

        let (a, b) = (LogWriterId::new(2), LogWriterId::new(3));
        assert_eq!(
            lines(&handed),
            [
                (a, "um-a".to_string()),
                (b, "dois-a".to_string()),
                (a, "um-b".to_string()),
                (b, "dois-b".to_string()),
            ]
        );
    }

    // ---- recycling ------------------------------------------------------

    /// A unit makes a new reader for every run, and the arena hands blocks
    /// out without taking them back — so a reader that kept its block would
    /// cost one per restart, and a log restarted often enough would quietly
    /// stop recording. Handing it back on drop is what keeps that cost at
    /// zero.
    #[test]
    fn a_reader_gives_its_chunk_back_when_it_ends() {
        let handed = Handed::new();
        {
            let buffer = buffer(&handed);
            assert_eq!(handed.borrow().minted, 1, "one chunk to fill");
            assert!(handed.borrow().spare.is_empty());
            drop(buffer);
        }
        assert_eq!(
            handed.borrow().spare.len(),
            1,
            "the chunk was not given back"
        );

        // And the next reader takes that one rather than making another.
        let before = handed.borrow().minted;
        let _next = buffer(&handed);
        assert_eq!(handed.borrow().minted, before, "a spare block was ignored");
    }

    /// A line the writer never finished dies with its reader, and the reader
    /// that follows starts clean.
    ///
    /// The unterminated text is held by the line, not the chunk — a line with
    /// no newline is never ready, so it is never placed — which is why it
    /// goes rather than turning up in the middle of the next run's output.
    #[test]
    fn an_unfinished_line_does_not_survive_its_reader() {
        let handed = Handed::new();
        buffer(&handed).write(b"inacabada");
        buffer(&handed).write(b"nova\n");

        assert_eq!(texts(&handed), ["nova"]);
    }

    // ---- read -----------------------------------------------------------

    #[tokio::test]
    async fn read_splits_a_line_longer_than_a_chunk() {
        let handed = Handed::new();
        let long = "a".repeat(LOG_CHUNK_SIZE * 3);
        let src = format!("{long}\ncurta\n").into_bytes();
        let mut src = &src[..];

        let mut buffer = buffer(&handed);
        while buffer.read(&mut src).await.unwrap() {}
        assert_eq!(texts(&handed), [long, "curta".to_string()]);
    }

    /// A character straddling the boundary between two reads must survive.
    #[tokio::test]
    async fn read_rejoins_a_character_split_across_reads() {
        let handed = Handed::new();
        let mut buffer = buffer(&handed);
        let text = "coração\n".as_bytes();
        // Hand it over one byte at a time: every multi-byte character is
        // guaranteed to be cut.
        for i in 0..text.len() {
            let mut one = &text[i..i + 1];
            buffer.read(&mut one).await.unwrap();
        }
        assert_eq!(texts(&handed), ["coração"]);
    }

    /// A line too long even for the line buffer leaves it in fragments, and
    /// each fragment is then spread over chunks. Both boundaries are crossed
    /// at once, and the line still has to come back whole.
    #[tokio::test]
    async fn a_line_longer_than_the_line_buffer_survives_both_splits() {
        let handed = Handed::new();
        // Not a multiple of either size, so no boundary lines up.
        let longa = "abcdefghij".repeat(937);
        assert!(longa.len() > LOG_CHUNK_SIZE * 4);

        let src = format!("{longa}\ndepois\n").into_bytes();
        let mut src = &src[..];
        let mut buffer = buffer(&handed);
        while buffer.read(&mut src).await.unwrap() {}

        let a = LogWriterId::new(2);
        assert_eq!(lines(&handed), [(a, longa), (a, "depois".to_string())]);
    }

    /// The same with the cuts falling wherever the reads land, on text where
    /// that regularly means inside a character.
    #[tokio::test]
    async fn a_very_long_line_survives_arbitrary_read_boundaries() {
        let handed = Handed::new();
        let longa = "0123456789áé".repeat(311);
        let src = format!("{longa}\ncurta\n").into_bytes();
        let mut src = &src[..];

        let mut buffer = buffer(&handed);
        while buffer.read(&mut src).await.unwrap() {}
        assert_eq!(texts(&handed), [longa, "curta".to_string()]);
    }

    /// Two processes, one printing a line too long for a chunk, the other
    /// writing between its pieces.
    ///
    /// A split line is continued by *that writer's* next chunk, which is not
    /// the next chunk pushed when something else got there first. A reader
    /// joining on position alone swallowed the other process's line into the
    /// middle of the long one and lost it — and blamed the wrong writer.
    #[tokio::test]
    async fn a_split_line_is_not_spliced_with_another_writers() {
        let handed = Handed::new();
        let mut longo = LogBufferAny::new(Fake::with_id(&handed, 2));
        let mut curto = LogBufferAny::new(Fake::with_id(&handed, 3));

        // One read brings more than the line buffer holds without closing the
        // line, so it leaves as a fragment and the last chunk stays Partial.
        let parte1 = "L".repeat(LOG_LINE_SIZE + 64);
        let mut src = parte1.as_bytes();
        longo.read(&mut src).await.unwrap();

        // The other process writes in between.
        curto.write(b"do outro\n");

        // And the long one finishes its line.
        let mut src = &b"FIM\n"[..];
        longo.read(&mut src).await.unwrap();

        let (a, b) = (LogWriterId::new(2), LogWriterId::new(3));
        let lines = lines(&handed);
        assert!(
            lines.contains(&(b, "do outro".to_string())),
            "the second writer's line was swallowed: {:?}",
            lines.iter().map(|(w, t)| (w, t.len())).collect::<Vec<_>>()
        );
        let (writer, longa) = lines.iter().find(|(_, t)| t.len() > 200).unwrap();
        assert_eq!(*writer, a);
        assert_eq!(*longa, format!("{parte1}FIM"));
    }

    // ---- the unsafe the rest rests on -----------------------------------

    /// Every piece must be valid UTF-8 **on its own**, because `get_data`
    /// hands it to `from_utf8_unchecked`.
    ///
    /// The line guarantees its own content is valid, but it is not the line
    /// that chooses where a chunk cuts — the chunk takes whatever its
    /// remaining room allows, and that offset knows nothing about character
    /// boundaries. Here the first chunk fills to one byte short of a two byte
    /// character, so a cut at the raw capacity would slice it in half.
    ///
    /// Note the joined history stays correct either way: the two halves
    /// concatenate back to the same bytes. That is exactly why this asserts
    /// per piece — the damage is invisible from the joined output and only
    /// shows in a renderer that reads one chunk, or in the `unsafe`.
    #[test]
    fn every_piece_is_valid_utf8_on_its_own() {
        let handed = Handed::new();
        // 255 ASCII bytes leave a single byte of room, and the next character
        // needs two.
        let linha = format!("{}{}", "a".repeat(LOG_CHUNK_SIZE - 1), "á".repeat(64));
        buffer(&handed).write(format!("{linha}\n").as_bytes());

        for (c, chunk) in handed.borrow().chunks.iter().enumerate() {
            for n in 0..chunk.count() {
                let bytes = chunk.piece_bytes(n).unwrap();
                assert!(
                    std::str::from_utf8(bytes).is_ok(),
                    "chunk {c} piece {n} is not valid UTF-8 on its own"
                );
            }
        }
        assert_eq!(texts(&handed), [linha]);
    }

    /// `LogChunkData` says whether a piece ends a line and nothing about how
    /// it starts — the tail of a split line reads as a `Line`, because it
    /// does end one.
    #[test]
    fn a_tail_piece_reads_as_a_line() {
        let handed = Handed::new();
        buffer(&handed).write(format!("{}\n", "w".repeat(LOG_CHUNK_SIZE + 4)).as_bytes());

        let handed = handed.borrow();
        assert!(matches!(
            handed.chunks[0].get_data(0),
            Some(LogChunkData::Partial(_))
        ));
        assert!(matches!(
            handed.chunks[1].get_data(0),
            Some(LogChunkData::Line(_))
        ));
    }
}
