use crate::log::{line::LogBufferLine, log::LogWriterId};

/// How many bytes of content one chunk holds.
///
/// This is the granularity of the whole log: the ring remembers a number of
/// *chunks*, not a number of lines, so the size sets how much of a long line
/// survives in one piece and how much memory a log of a given capacity costs
/// (`capacity * LOG_CHUNK_SIZE`). Small enough that a mostly-short-lines log
/// wastes little, which means lines past this length are split across several
/// chunks and only the last of them reports [`is_line`](LogChunk::is_line).
pub const LOG_CHUNK_SIZE: usize = 256;

/// Why a chunk stopped accepting bytes.
///
/// Private on purpose: the outside asks
/// [`is_finished`](LogChunk::is_finished) or
/// [`is_line`](LogChunk::is_line) and never has to name a state.
#[derive(PartialEq, Eq)]
enum LogChunkState {
    /// Still accepting bytes.
    Open,
    /// A newline closed the line.
    EndLine,
    /// The chunk ran out of room, so the line continues in the next one.
    End,
}

/// A slice of the history: text, and whether it ends a line.
///
/// Storage, and only storage. It holds text that is already valid and already
/// split into lines — [`LogBufferLine`] does that work — so nothing here
/// decodes, scans or parses. What it owns is a fixed buffer that is allocated
/// once and reused for the life of the log.
///
/// # Filling it
///
/// [`push_line`](LogChunk::push_line) takes as much of a line as fits and
/// consumes that much from it. A line that does not fit leaves the chunk
/// finished and itself part drained, for the next chunk to carry on with:
///
/// ```ignore
/// loop {
///     chunk.push_line(&mut line);
///     if chunk.is_finished() {
///         writer.push_chunk(&mut chunk);  // swaps in a recycled chunk
///     }
///     if line.is_drained() {
///         break;
///     }
/// }
/// ```
///
/// A chunk that took a whole line reports [`is_line`](LogChunk::is_line); one
/// that filled up part way through does not, which is how a reader knows the
/// next chunk continues this one.
pub(super) struct LogChunk {
    buf: Box<[u8; LOG_CHUNK_SIZE]>,
    len: usize,
    state: LogChunkState,
    /// Who wrote this. Stamped as the chunk is handed to the log, not as it
    /// is filled, so it cannot drift out of step with the content: the writer
    /// doing the handing over is by definition the one that wrote it.
    writer: LogWriterId,
}

impl LogChunk {
    /// Build an empty chunk, allocating its buffer.
    ///
    /// The only place a chunk's buffer is ever allocated: from here on it is
    /// recycled, never freed and never grown.
    pub(super) fn new() -> Self {
        Self {
            buf: unsafe { Box::<[u8; LOG_CHUNK_SIZE]>::new_zeroed().assume_init() },
            len: 0,
            state: LogChunkState::Open,
            writer: LogWriterId::UNSET,
        }
    }

    /// Exchange this chunk with `other`, buffers and all.
    ///
    /// How a chunk travels: handing one to the log is a swap with a recycled
    /// slot, so both sides keep an allocation and nothing is copied. The
    /// state and length travel with the buffer, which is what lets a stored
    /// chunk still answer [`is_line`](LogChunk::is_line) long after the
    /// writer moved on.
    pub fn swap(&mut self, other: &mut LogChunk) {
        std::mem::swap(self, other);
    }

    /// Empty the chunk and reopen it for writing, keeping its buffer.
    ///
    /// This is what makes a chunk reusable, and it is the whole job of the
    /// queue's recycler, which calls it as each slot is claimed.
    pub fn clear(&mut self) {
        self.len = 0;
        // Without this a recycled chunk comes back already finished, and
        // every later `write` returns 0 for ever.
        self.state = LogChunkState::Open;
        // A recycled chunk carries the last writer's stamp, which would be a
        // lie the moment a different one filled it.
        self.writer = LogWriterId::UNSET;
    }

    /// Who wrote this chunk.
    pub fn writer(&self) -> LogWriterId {
        self.writer
    }

    /// Claim the chunk for `writer`, as it is handed to the log.
    pub(super) fn set_writer(&mut self, writer: LogWriterId) {
        self.writer = writer;
    }

    /// Check if the chunk is finished, that means, or a new line was found
    /// or there is no more room
    pub fn is_finished(&self) -> bool {
        self.state != LogChunkState::Open
    }

    /// Whether this chunk is a whole line rather than the head of a longer
    /// one, so a renderer knows if the next chunk continues it.
    pub fn is_line(&self) -> bool {
        self.state == LogChunkState::EndLine
    }

    /// Place as much of `line` as fits, reporting how many bytes were taken.
    ///
    /// The text arrives already validated and already split — that work
    /// belongs to [`LogBufferLine`], which is why this only copies. What it
    /// takes it also consumes, so a line too long for one chunk is simply
    /// offered to the next until it is drained — no caller keeps a cursor.
    ///
    /// What the chunk decides is where the text lands:
    ///
    /// - it all fits and the line is whole — the chunk is a finished line;
    /// - it all fits and the line is not — the chunk stays open for the rest;
    /// - it does not all fit — the chunk takes what it can and finishes, and
    ///   the caller offers the remainder to the next one.
    ///
    /// Taking a prefix means stopping on a character boundary, never inside
    /// one, so every chunk on its own is still valid text.
    pub fn push_line(&mut self, line: &mut LogBufferLine) -> usize {
        if self.is_finished() {
            return 0;
        }

        let text = line.pending();
        let pending = text.len();
        // The room decides the cut, and it knows nothing about characters, so
        // it lands inside one regularly. Back off to a boundary: a chunk
        // holding half a character would be undecodable on its own, and
        // `as_str` promises it is not.
        let taken = text.floor_char_boundary((LOG_CHUNK_SIZE - self.len).min(pending));

        self.buf[self.len..self.len + taken].copy_from_slice(&text.as_bytes()[..taken]);
        self.len += taken;

        if taken < pending {
            // Room ran out with the line unfinished. Whatever the line says
            // about itself, what is stored here is only a head.
            self.state = LogChunkState::End;
        } else if line.is_complete() {
            self.state = LogChunkState::EndLine;
        }
        line.consume(taken);
        taken
    }

    /// The content, as text.
    ///
    /// Free: the bytes were validated on the way in, so there is nothing left
    /// to decode or check here. That is the reason `write` bothers to
    /// validate at all rather than storing raw bytes and decoding on read —
    /// a log is written once and displayed on every repaint.
    pub fn as_str(&self) -> &str {
        // SAFETY: `len` only ever advances by a run of bytes that was just
        // checked: an ASCII byte, a sequence `std::str::from_utf8` accepted,
        // or `REPLACEMENT`. A concatenation of valid UTF-8 is valid UTF-8,
        // and `clear` resets `len` to 0, so `buf[..len]` is always valid.
        unsafe { std::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }

    /// The content, as bytes — the same thing
    /// [`as_str`](LogChunk::as_str) returns, for a caller writing it straight
    /// out to a terminal without looking at it.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// How many bytes of content the chunk holds.
    ///
    /// Bytes, not characters, and not the number of bytes fed in: a newline
    /// was consumed without being stored, and every undecodable byte became
    /// three.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the chunk holds nothing.
    ///
    /// An empty chunk is never handed to the log — it would show up as a
    /// blank line that the process never printed — so this is what the end of
    /// a stream is tested with before flushing whatever was left.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}
