use crate::{
    log::{line::LogBufferLine, log::LogWriterId},
    util::arena::{ARENA_BLOCK_SIZE, ArenaBlock},
};

/// A chunk's buffer is one arena block, so the two sizes are the same number
/// written in two places. This is where they are made to agree.
const _: () = assert!(
    LOG_CHUNK_SIZE == ARENA_BLOCK_SIZE,
    "LOG_CHUNK_SIZE and ARENA_BLOCK_SIZE must match"
);

/// How many bytes of content one chunk holds.
///
/// This is the granularity of the whole log: the ring remembers a number of
/// *chunks*, not a number of lines, so this sets how much text travels
/// together and how much memory a log of a given capacity costs
/// (`capacity * LOG_CHUNK_SIZE`).
pub const LOG_CHUNK_SIZE: usize = 256;

/// How many lines one chunk may hold.
///
/// The brake for degenerate input: a burst of empty or one word lines would
/// never reach [`LOG_CHUNK_LIMIT`], and with no cap a chunk could hold
/// hundreds of them, each needing an end offset. It bounds the offset table,
/// which is why it has to be fixed.
pub const LOG_CHUNK_MAX_LINES: usize = 2;

/// Content watermark: past this, the chunk starts no further line.
///
/// Not the same as being full. A line already under way runs on to
/// [`LOG_CHUNK_SIZE`], so the gap between the two is the room a last line has
/// to finish in without being split. It also keeps a chunk from taking a line
/// into a handful of leftover bytes, which would split that line for nothing.
pub const LOG_CHUNK_LIMIT: usize = 192;

/// One piece of a chunk's content.
///
/// A chunk packs whole lines, but its last one may have been cut short by the
/// room running out. So a piece either ends a line or it does not, and that
/// is the distinction a reader needs to put the history back together.
///
/// # Joining
///
/// Join pieces until a [`Line`](LogChunkData::Line) closes one — but only
/// pieces of the same [`writer`](LogChunk::writer). A log takes from as many
/// writers as a unit has processes, and a line split across chunks is
/// continued by that writer's *next* chunk, which is not necessarily the next
/// chunk in the ring: another process writing in between puts its own chunk
/// there. Joining on position alone swallows that chunk into the middle of
/// the split line and loses it.
///
/// Note a piece says nothing about how it *starts*. The tail of a split line
/// reads as a `Line`, because it does end one. What marks the seam is the
/// `Partial` before it, in the same writer's previous chunk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LogChunkData<'a> {
    /// Text that ends a line.
    Line(&'a str),
    /// Text that does not: the line goes on in this writer's next chunk,
    /// wherever in the ring that turns out to be.
    Partial(&'a str),
}

impl<'a> LogChunkData<'a> {
    /// The text, whichever kind of piece this is.
    pub fn as_str(self) -> &'a str {
        match self {
            Self::Line(text) | Self::Partial(text) => text,
        }
    }

    /// Whether this piece ends a line.
    pub fn is_line(self) -> bool {
        matches!(self, Self::Line(_))
    }
}

/// A slice of the history: a few lines of text, packed.
///
/// Storage, and only storage. It holds text that is already valid and already
/// split into lines — [`LogBufferLine`] does that work — so nothing here
/// decodes, scans or parses. What it owns is a fixed buffer, allocated once
/// and reused for the life of the log.
///
/// # Packing
///
/// Holding one line per chunk wasted most of the buffer on the short lines
/// that are nearly all of real output. So a chunk takes lines until the
/// quota or the watermark stops it, and remembers where each one ended.
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
/// # Reading it
///
/// [`iter_data`](LogChunk::iter_data) walks the pieces; `get_data` and
/// `get_str` reach one by index.
pub struct LogChunk {
    /// One block of the log's arena. Every chunk in a log draws from the same
    /// pool, which is what lets them be swapped about: a swap exchanges which
    /// block each side points at, and no bytes move.
    buf: ArenaBlock,
    /// Where each piece ends. Pieces are packed back to back, so one starts
    /// where the last ended, and the end of the last piece is also the length
    /// of the whole chunk — which is why there is no separate `len`: it would
    /// be `ends[count - 1]` written down twice.
    ends: [u16; LOG_CHUNK_MAX_LINES],
    /// How many pieces the chunk holds, counting one still being extended.
    count: usize,
    /// Whether the last piece is still open — the line it belongs to has not
    /// been closed by a newline. The next piece pushed extends it rather than
    /// starting a line of its own.
    ///
    /// Together with `finished` this says everything there is to say about
    /// how a chunk stopped: finished with nothing open means the quota or the
    /// watermark stopped it and every line in here is whole; finished with
    /// something open means the room ran out mid line and the next chunk
    /// carries on. There is no third way for a chunk to end, which is why
    /// there is no state enum — it would only be these two bools with two
    /// combinations that cannot happen.
    trailing_open: bool,
    /// Whether the chunk takes no more and is on its way to the log.
    finished: bool,
    /// Who wrote this. Stamped as the chunk is handed to the log, not as it
    /// is filled, so it cannot drift out of step with the content: the writer
    /// doing the handing over is by definition the one that wrote it.
    writer: LogWriterId,
}

impl LogChunk {
    /// Build an empty chunk on `block`.
    ///
    /// Taking the block rather than an arena leaves the caller to say where
    /// it should come from: the ring and the queue are sized up front and ask
    /// for a pooled one, while a reader's chunk is not and can fall back to
    /// the heap. See [`Arena::alloc`](crate::util::arena::Arena::alloc) and
    /// [`alloc_or_heap`](crate::util::arena::Arena::alloc_or_heap).
    pub(super) fn new(block: ArenaBlock) -> Self {
        Self {
            buf: block,
            ends: [0; LOG_CHUNK_MAX_LINES],
            count: 0,
            trailing_open: false,
            finished: false,
            writer: LogWriterId::UNSET,
        }
    }

    /// Exchange this chunk with `other`, buffers and all.
    ///
    /// How a chunk travels: handing one to the log is a swap with a recycled
    /// slot, so both sides keep an allocation and nothing is copied. The
    /// offsets travel with the buffer, which is what lets a stored chunk
    /// still be read apart long after the writer moved on.
    pub fn swap(&mut self, other: &mut LogChunk) {
        std::mem::swap(self, other);
    }

    /// Empty the chunk and reopen it for writing, keeping its buffer.
    ///
    /// This is what makes a chunk reusable, and it is the whole job of the
    /// queue's recycler, which calls it as each slot is claimed.
    ///
    /// The offsets are left as they are: `count` is what says which of them
    /// mean anything, and resetting it is enough to make the rest
    /// unreachable.
    pub fn clear(&mut self) {
        self.count = 0;
        self.trailing_open = false;
        // Without this a recycled chunk comes back already finished, and
        // every later push returns 0 for ever.
        self.finished = false;
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

    /// Whether the chunk takes no more: it is on its way to the log.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// How many pieces the chunk holds.
    pub fn count(&self) -> usize {
        self.count
    }

    /// How many bytes of text the chunk holds, across all its pieces.
    ///
    /// Bytes, not characters, and not the number of bytes fed in: the
    /// newlines that split the lines were never stored, and every undecodable
    /// byte became three.
    pub fn len(&self) -> usize {
        match self.count {
            0 => 0,
            count => self.ends[count - 1] as usize,
        }
    }

    /// Whether the chunk holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Where piece `n` starts and ends, or `None` if there is no such piece.
    ///
    /// Pieces are packed back to back, so one starts where the last ended,
    /// and the final one runs to `len`.
    fn bounds(&self, n: usize) -> Option<(usize, usize)> {
        if n >= self.count {
            return None;
        }
        let start = if n == 0 { 0 } else { self.ends[n - 1] as usize };
        Some((start, self.ends[n] as usize))
    }

    /// Piece `n`, with whether it ends a line.
    pub fn get_data(&self, n: usize) -> Option<LogChunkData<'_>> {
        let (start, end) = self.bounds(n)?;
        // SAFETY: every byte between two offsets was copied out of a
        // `LogBufferLine`, which only ever holds validated UTF-8, and
        // `push_line` cuts on a character boundary — so a piece is valid
        // text on its own and not merely as part of the whole.
        let text = unsafe { std::str::from_utf8_unchecked(&self.buf[start..end]) };

        // Only the last piece can be unterminated; everything before it was
        // closed by the newline that started the next one.
        if n + 1 == self.count && self.trailing_open {
            Some(LogChunkData::Partial(text))
        } else {
            Some(LogChunkData::Line(text))
        }
    }

    /// The text of piece `n`, without saying whether it ends a line.
    pub fn get_str(&self, n: usize) -> Option<&str> {
        Some(self.get_data(n)?.as_str())
    }

    /// Walk the pieces in order.
    pub fn iter_data(&self) -> LogChunkDataIter<'_> {
        LogChunkDataIter { chunk: self, n: 0 }
    }

    /// Place as much of `line` as fits, reporting how many bytes were taken.
    ///
    /// The text arrives already validated and already split — that work
    /// belongs to [`LogBufferLine`], which is why this only copies. What it
    /// takes it also consumes, so a line too long for one chunk is simply
    /// offered to the next until it is drained; no caller keeps a cursor.
    ///
    /// What the chunk decides is where the text lands:
    ///
    /// - it all fits and the line is whole — a piece is closed, and the chunk
    ///   stays open for the next line unless the quota or the watermark stops
    ///   it;
    /// - it all fits and the line is not whole — the piece stays open, and
    ///   the rest of the same line extends it;
    /// - it does not all fit — the chunk takes what it can and finishes, and
    ///   the caller offers the remainder to the next one.
    ///
    /// Taking a prefix means stopping on a character boundary, never inside
    /// one, so every piece on its own is still valid text.
    pub fn push_line(&mut self, line: &mut LogBufferLine) -> usize {
        if self.is_finished() {
            return 0;
        }

        let len = self.len();
        let text = line.pending();
        let pending = text.len();
        // The room decides the cut, and it knows nothing about characters, so
        // it lands inside one regularly. Back off to a boundary: a piece
        // holding half a character would be undecodable on its own, and
        // `get_data` promises it is not.
        let taken = text.floor_char_boundary((LOG_CHUNK_SIZE - len).min(pending));

        // There is text to place and no room for any of it. Finish here so
        // the caller hands this chunk on and offers the rest to the next.
        // An empty line places nothing either, but it is a line, so it has to
        // go through the rest of this.
        if taken == 0 && pending > 0 {
            self.finished = true;
            return 0;
        }

        // Text that does not continue an open piece opens one of its own.
        if !self.trailing_open {
            self.count += 1;
        }

        self.buf[len..len + taken].copy_from_slice(&text.as_bytes()[..taken]);
        // Extending the open piece moves its end; a new one gets its first.
        self.ends[self.count - 1] = (len + taken) as u16;
        line.consume(taken);

        if taken < pending {
            // Room ran out with the line unfinished. Whatever the line says
            // about itself, what is stored here is only a head.
            self.trailing_open = true;
            self.finished = true;
        } else if line.is_complete() {
            self.trailing_open = false;
            if self.count == LOG_CHUNK_MAX_LINES || self.len() >= LOG_CHUNK_LIMIT {
                self.finished = true;
            }
        } else {
            // The whole fragment fit, but its line goes on.
            self.trailing_open = true;
        }
        taken
    }

    /// Which arena block this chunk sits on, or `None` if it came from the
    /// heap because the pool was spent.
    #[cfg(test)]
    pub(super) fn block_index(&self) -> Option<u32> {
        self.buf.index()
    }

    /// The raw bytes of piece `n`, for tests that have to check the very
    /// thing [`get_data`](LogChunk::get_data) assumes.
    #[cfg(test)]
    pub(super) fn piece_bytes(&self, n: usize) -> Option<&[u8]> {
        let (start, end) = self.bounds(n)?;
        Some(&self.buf[start..end])
    }
}

/// Walks a chunk's pieces, oldest first.
pub struct LogChunkDataIter<'a> {
    /// Borrowed, not copied: a piece hands out a slice of the chunk's buffer,
    /// so the chunk has to outlive the walk.
    chunk: &'a LogChunk,
    /// The next piece to yield. Ends when it reaches the chunk's count, so a
    /// chunk filled further during the walk would be impossible — the borrow
    /// rules that out.
    n: usize,
}

/// Exact, not merely bounded: a chunk's piece count is fixed once it has been
/// handed over, and the borrow keeps it that way for the walk. So a caller can
/// size a buffer from [`len`](ExactSizeIterator::len) and trust it.
impl<'a> Iterator for LogChunkDataIter<'a> {
    type Item = LogChunkData<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let data = self.chunk.get_data(self.n)?;
        self.n += 1;
        Some(data)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.chunk.count - self.n;
        (left, Some(left))
    }
}

impl ExactSizeIterator for LogChunkDataIter<'_> {}
