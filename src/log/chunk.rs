use crate::log::log::LogWriterId;

/// How many bytes of content one chunk holds.
///
/// This is the granularity of the whole log: the ring remembers a number of
/// *chunks*, not a number of lines, so the size sets how much of a long line
/// survives in one piece and how much memory a log of a given capacity costs
/// (`capacity * LOG_CHUNK_SIZE`). Small enough that a mostly-short-lines log
/// wastes little, which means lines past this length are split across several
/// chunks and only the last of them reports [`is_line`](LogChunk::is_line).
pub const LOG_CHUNK_SIZE: usize = 256;

/// The longest a single UTF-8 character can be.
const UTF8_MAX_WIDTH: usize = 4;

/// What a byte that cannot be decoded is stored as.
///
/// Three bytes wide, so replacing one bad byte *grows* the content — which
/// is why the room left over has to cover the widest thing a step can
/// write, not just one byte.
const REPLACEMENT: &[u8] = "\u{fffd}".as_bytes();

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

/// Internal log chunk, contains a pointer to the data and performs operations on bytes
///
/// # Feeding it
///
/// [`write`](LogChunk::write) takes raw bytes and reports how many it
/// consumed, leaving the caller to advance its reader by that much and
/// re-offer the rest. It stops on a newline, when it runs out of room, or
/// when the input ends mid-character.
///
/// That last case is the only one where `write` can return 0 without the
/// chunk being finished, and it happens only when the whole input is the
/// start of a character that could still turn out valid — at most three
/// bytes. There is no way around it: given `\xc3` alone, there is no telling
/// whether the next byte makes it `é` or leaves it garbage. Guessing would
/// mangle every accented character that lands on a read boundary. Four or
/// more bytes that do not decode are unambiguously broken, and become
/// replacement characters instead.
///
/// So the caller's loop is:
///
/// ```ignore
/// consumed += chunk.write(&buf[consumed..filled]);
/// if chunk.is_finished() {
///     writer.push_chunk(&mut chunk);  // swaps in a recycled chunk
/// } else {
///     // read more; whatever is left is a partial character
/// }
/// ```
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

    /// Whether another character still fits.
    ///
    /// Holding back the width of the widest character is what keeps the
    /// write loop to a single space test: from here, anything one step can
    /// store — an ASCII byte, a four byte character, a three byte
    /// replacement — is guaranteed to fit.
    fn has_room(&self) -> bool {
        self.len <= LOG_CHUNK_SIZE - UTF8_MAX_WIDTH
    }

    /// Consumes n bytes from the buffer, returns how many bytes was consumed
    /// so the parent can handle
    ///
    /// If it finds a '\n', also mark as completed
    ///
    /// The count is bytes taken from `buf`, which is not the same as the
    /// growth in content: a newline is consumed but not stored, and one bad
    /// byte is consumed but stores three.
    pub fn write(&mut self, buf: &[u8]) -> usize {
        self.write_inner(buf, true)
    }

    /// [`write`](LogChunk::write) for the last bytes of a stream.
    ///
    /// Nothing more is coming, so a trailing partial character can never be
    /// completed and becomes replacement characters rather than being held
    /// back. Without this the caller would be stuck re-offering those bytes
    /// to a chunk that keeps returning 0.
    pub fn write_eof(&mut self, buf: &[u8]) -> usize {
        self.write_inner(buf, false)
    }

    /// Shared body; `more_coming` says whether a partial tail may be held.
    fn write_inner(&mut self, buf: &[u8], more_coming: bool) -> usize {
        if self.is_finished() {
            return 0;
        }

        let mut taken = 0;
        while taken < buf.len() {
            let byte = buf[taken];

            // A newline stores nothing, so it is taken even with no room
            // left — otherwise a maximally long line would be followed by
            // a spurious empty one.
            if byte == b'\n' {
                self.state = LogChunkState::EndLine;
                return taken + 1;
            }
            if !self.has_room() {
                self.state = LogChunkState::End;
                return taken;
            }

            // ASCII is nearly all of it, and needs no decoding.
            if byte < 0x80 {
                self.buf[self.len] = byte;
                self.len += 1;
                taken += 1;
                continue;
            }

            let rest = &buf[taken..];
            let width = utf8_width(byte);

            // Not all there yet. Hold it back only while it could still
            // become a real character; past four bytes it cannot.
            if more_coming && width > rest.len() && rest.len() < UTF8_MAX_WIDTH && is_partial(rest)
            {
                return taken;
            }

            if width > 0 && width <= rest.len() && std::str::from_utf8(&rest[..width]).is_ok() {
                self.buf[self.len..self.len + width].copy_from_slice(&rest[..width]);
                self.len += width;
                taken += width;
                continue;
            }

            // Undecodable. Store a replacement and step over one byte, so a
            // broken sequence can still resynchronise on the next character.
            self.buf[self.len..self.len + REPLACEMENT.len()].copy_from_slice(REPLACEMENT);
            self.len += REPLACEMENT.len();
            taken += 1;
        }

        // The input ran out just as the room did. Settling it here spares
        // the caller a further `write` that could only come back as 0.
        if !self.has_room() {
            self.state = LogChunkState::End;
        }
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

/// How many bytes the character led by `byte` needs, or 0 if it cannot lead
/// one.
///
/// Only the lead byte is read, so a few illegal encodings slip through —
/// overlong forms, surrogates, anything past U+10FFFF. The full sequence is
/// validated before it is stored.
const fn utf8_width(byte: u8) -> usize {
    match byte {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

/// Whether `bytes` is a valid but unfinished character, so more input could
/// still complete it.
fn is_partial(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        // Valid as far as it goes, and cut short rather than wrong.
        Err(error) => error.valid_up_to() == 0 && error.error_len().is_none(),
        Ok(_) => false,
    }
}
