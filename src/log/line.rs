//! One line under construction, assembled from whatever the pipe hands over.


/// How much of a single line is assembled before it has to be handed over.
///
/// Deliberately not tied to [`LOG_CHUNK_SIZE`](super::chunk::LOG_CHUNK_SIZE).
/// Packing alone would not need it — a line larger than a chunk fits nowhere
/// and has to be split whatever its length is, so
/// [`is_complete`](LogBufferLine::is_complete) being false already says
/// everything the placing decides on. Holding the line whole is worth more
/// than that: while it is here it is one addressable thing, and anything that
/// wants to look at a line as a line has to do it before it is cut up.
///
/// Sized from measured output — a median around 45 bytes and a 99th
/// percentile around 170 — so a kibibyte holds essentially every real line
/// whole and the fragment path stays for genuine outliers: a minified bundle,
/// a `--verbose` compiler invocation.
///
/// One of these exists per reader task, not per log, so the room is cheap.
pub(super) const LOG_LINE_SIZE: usize = 1024;

/// The longest a single UTF-8 character can be.
const UTF8_MAX_WIDTH: usize = 4;

/// What a byte that cannot be decoded is stored as.
///
/// Three bytes wide, so replacing one bad byte *grows* the content — which is
/// why the room left over has to cover the widest thing a step can write, not
/// just one byte.
const REPLACEMENT: &[u8] = "\u{fffd}".as_bytes();

/// Why the line stopped taking bytes.
///
/// Private on purpose: the outside asks [`is_ready`](LogBufferLine::is_ready)
/// or [`is_complete`](LogBufferLine::is_complete) and never has to name a
/// state.
#[derive(PartialEq, Eq)]
enum LogBufferLineState {
    /// Still taking bytes.
    Open,
    /// A newline closed it, so this is a whole line.
    Complete,
    /// It ran out of room first, so the line goes on in the next one.
    Fragment,
}

/// A line being built out of raw bytes.
///
/// Sits between the pipe and storage, and owns the two things raw bytes need
/// before they can be stored: they have to be split into lines, and they have
/// to be turned into text.
///
/// # Why the line is assembled first
///
/// Storage packs lines; it does not parse them. Assembling here means the
/// length of a line is known before anything is placed, so the packer can
/// decide where it goes instead of finding out half way through that it does
/// not fit. The cost is one copy, from here into the chunk.
///
/// # Feeding it
///
/// [`write`](LogBufferLine::write) takes raw bytes and reports how many it
/// consumed, leaving the caller to advance its reader by that much and
/// re-offer the rest. It stops on a newline, when it runs out of room, or
/// when the input ends mid-character.
///
/// That last case is the only one where `write` can return 0 without the line
/// being ready, and it happens only when the whole input is the start of a
/// character that could still turn out valid — at most three bytes. There is
/// no way around it: given `\xc3` alone, there is no telling whether the next
/// byte makes it `é` or leaves it garbage. Guessing would mangle every
/// accented character that lands on a read boundary. Four or more bytes that
/// do not decode are unambiguously broken, and become replacement characters
/// instead.
///
/// So the caller's loop is:
///
/// ```ignore
/// consumed += line.write(&buf[consumed..filled]);
/// if line.is_ready() {
///     // hand it to a chunk, then clear it
/// } else {
///     // read more; whatever is left is a partial character
/// }
/// ```
pub(super) struct LogBufferLine {
    len: usize,
    /// How much of the text a chunk has already taken. Storage drains the
    /// line rather than copying out of it, so what is left to place is the
    /// line's own business and no caller has to keep a cursor.
    taken: usize,
    state: LogBufferLineState,
    /// Held inline rather than behind a [`Box`]. There is exactly one of
    /// these per reader task and it never moves once built, so the
    /// indirection bought nothing — and cost a pointer load on the hottest
    /// loop there is, one byte of output at a time. Inline it also shares a
    /// cache line with the fields above.
    ///
    /// Last by declaration for the reader's sake. Where it actually lands is
    /// the compiler's business — it puts the array right after `len` and
    /// `taken`, so those and the start of the buffer share one cache line.
    buf: [u8; LOG_LINE_SIZE],
}

impl LogBufferLine {
    /// Build an empty line.
    ///
    /// The buffer is part of the line, so there is nothing to allocate and
    /// nothing that can fail. It is reused for every line the task ever
    /// reads.
    pub(super) fn new() -> Self {
        Self {
            len: 0,
            taken: 0,
            state: LogBufferLineState::Open,
            buf: [0u8; LOG_LINE_SIZE],
        }
    }

    /// Empty it and reopen it for writing, keeping the buffer.
    ///
    /// Called once the content has been handed on. Without the state reset a
    /// reused line comes back already ready, and every later write returns 0
    /// for ever.
    pub(super) fn clear(&mut self) {
        self.len = 0;
        self.taken = 0;
        self.state = LogBufferLineState::Open;
    }

    /// The text not yet taken by a chunk.
    pub(super) fn pending(&self) -> &str {
        &self.as_str()[self.taken..]
    }

    /// Mark `n` more bytes as taken.
    ///
    /// Called by the chunk that copied them. `n` always lands on a character
    /// boundary, because a chunk never takes half of one.
    pub(super) fn consume(&mut self, n: usize) {
        self.taken += n;
        debug_assert!(self.taken <= self.len, "consumed past the end of the line");
    }

    /// Whether every byte has been placed.
    pub(super) fn is_drained(&self) -> bool {
        self.taken == self.len
    }

    /// Whether there is something to hand over — a whole line, or as much of
    /// one as fits.
    pub(super) fn is_ready(&self) -> bool {
        self.state != LogBufferLineState::Open
    }

    /// Whether a newline closed it, rather than it being the head of a longer
    /// line whose rest is still coming.
    pub(super) fn is_complete(&self) -> bool {
        self.state == LogBufferLineState::Complete
    }

    /// The line so far, as text.
    ///
    /// Free: the bytes were validated on the way in, so there is nothing left
    /// to decode or check here.
    pub(super) fn as_str(&self) -> &str {
        // SAFETY: `len` only ever advances by a run of bytes that was just
        // checked: an ASCII byte, a sequence `std::str::from_utf8` accepted,
        // or `REPLACEMENT`. A concatenation of valid UTF-8 is valid UTF-8,
        // and `clear` resets `len` to 0, so `buf[..len]` is always valid.
        unsafe { std::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }

    /// The line so far, as bytes.
    pub(super) fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// How many bytes of text the line holds.
    ///
    /// Bytes, not characters, and not the number of bytes fed in: the newline
    /// that closed it was consumed without being stored, and every
    /// undecodable byte became three.
    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// Whether the line holds no text.
    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether another character still fits.
    ///
    /// Holding back the width of the widest character is what keeps the write
    /// loop to a single space test: from here, anything one step can store —
    /// an ASCII byte, a four byte character, a three byte replacement — is
    /// guaranteed to fit.
    fn has_room(&self) -> bool {
        self.len <= LOG_LINE_SIZE - UTF8_MAX_WIDTH
    }

    /// Consume bytes, returning how many were taken.
    ///
    /// Stops at the newline that ends the line, when there is no more room,
    /// or when the input ends mid-character.
    ///
    /// The count is bytes taken from `buf`, which is not the same as the
    /// growth in content: a newline is consumed but not stored, and one bad
    /// byte is consumed but stores three.
    pub(super) fn write(&mut self, buf: &[u8]) -> usize {
        self.write_inner(buf, true)
    }

    /// [`write`](LogBufferLine::write) for the last bytes of a stream.
    ///
    /// Nothing more is coming, so a trailing partial character can never be
    /// completed and becomes replacement characters rather than being held
    /// back. Without this the caller would be stuck re-offering those bytes
    /// to a line that keeps returning 0.
    pub(super) fn write_eof(&mut self, buf: &[u8]) -> usize {
        self.write_inner(buf, false)
    }

    /// Close a line the stream ended without terminating.
    ///
    /// A process whose last output has no trailing newline still wrote a
    /// line, and it would otherwise sit here unready for ever.
    pub(super) fn seal(&mut self) {
        if self.state == LogBufferLineState::Open {
            self.state = LogBufferLineState::Complete;
        }
    }

    /// Shared body; `more_coming` says whether a partial tail may be held.
    fn write_inner(&mut self, buf: &[u8], more_coming: bool) -> usize {
        if self.is_ready() {
            return 0;
        }

        let mut taken = 0;
        while taken < buf.len() {
            let byte = buf[taken];

            // A newline stores nothing, so it is taken even with no room
            // left — otherwise a maximally long line would be followed by a
            // spurious empty one.
            if byte == b'\n' {
                self.state = LogBufferLineState::Complete;
                return taken + 1;
            }
            if !self.has_room() {
                self.state = LogBufferLineState::Fragment;
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

        // The input ran out just as the room did. Settling it here spares the
        // caller a further `write` that could only come back as 0.
        if !self.has_room() {
            self.state = LogBufferLineState::Fragment;
        }
        taken
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

#[cfg(test)]
mod test {
    use super::*;

    /// Feed everything in, the way a caller's loop does, and report what came
    /// out: the finished lines, and whether each was closed by a newline.
    fn drain(input: &[u8], eof: bool) -> Vec<(String, bool)> {
        let mut line = LogBufferLine::new();
        let mut out = Vec::new();
        let mut start = 0;
        loop {
            let taken = if eof {
                line.write_eof(&input[start..])
            } else {
                line.write(&input[start..])
            };
            start += taken;
            if line.is_ready() {
                out.push((line.as_str().to_string(), line.is_complete()));
                line.clear();
                continue;
            }
            // Not ready and taking nothing means the input is spent.
            if taken == 0 {
                // What is left over is a line the stream never terminated.
                line.seal();
                if !line.is_empty() {
                    out.push((line.as_str().to_string(), line.is_complete()));
                }
                break;
            }
        }
        out
    }

    fn lines(input: &[u8]) -> Vec<(String, bool)> {
        drain(input, true)
    }

    #[test]
    fn splits_on_newlines_without_storing_them() {
        assert_eq!(
            lines(b"um\ndois\ntres\n"),
            [
                ("um".into(), true),
                ("dois".into(), true),
                ("tres".into(), true)
            ]
        );
    }

    #[test]
    fn an_empty_line_is_still_a_line() {
        assert_eq!(lines(b"\n\n"), [("".into(), true), ("".into(), true)]);
    }

    /// The bytes of a character that straddles two reads are held back, not
    /// guessed at, so the character survives the boundary.
    #[test]
    fn holds_a_partial_character_back_until_it_is_whole() {
        let mut line = LogBufferLine::new();
        assert_eq!(line.write(b"ol\xc3"), 2, "the lead byte should be held");
        assert!(!line.is_ready());
        assert_eq!(line.as_str(), "ol");

        // The next read brings the rest of the character.
        assert_eq!(line.write("\u{e1}\n".as_bytes()), 3);
        assert!(line.is_complete());
        assert_eq!(line.as_str(), "olá");
    }

    /// At end of stream there is no next read, so a partial tail is broken
    /// rather than unfinished.
    #[test]
    fn a_partial_character_at_eof_becomes_a_replacement() {
        assert_eq!(lines(b"ol\xc3"), [("ol\u{fffd}".into(), true)]);
    }

    #[test]
    fn undecodable_bytes_resynchronise_on_the_next_character() {
        assert_eq!(
            lines(b"a\xff\xfeb\n"),
            [("a\u{fffd}\u{fffd}b".into(), true)]
        );
    }

    /// A line too long to hold whole leaves as a fragment, and the rest
    /// follows as its own piece — that is the signal the packer needs.
    #[test]
    fn a_line_past_capacity_leaves_as_a_fragment() {
        let longa = "z".repeat(LOG_LINE_SIZE + 32);
        let out = lines(format!("{longa}\ncurta\n").as_bytes());

        assert!(out.len() >= 3, "expected a fragment, a tail and a line");
        assert!(!out[0].1, "the first piece should not read as complete");
        assert!(out.last().unwrap().1, "the last line is whole");
        assert_eq!(out.last().unwrap().0, "curta");

        // Nothing is lost or invented in the splitting.
        let rebuilt: String = out[..out.len() - 1].iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(rebuilt, longa);
    }

    /// A process whose last output has no trailing newline still wrote a
    /// line; without sealing it would sit here unready for ever.
    #[test]
    fn seal_closes_an_unterminated_line() {
        let mut line = LogBufferLine::new();
        line.write(b"sem newline");
        assert!(!line.is_ready());

        line.seal();
        assert!(line.is_complete());
        assert_eq!(line.as_str(), "sem newline");
    }

    #[test]
    fn seal_leaves_a_fragment_a_fragment() {
        let mut line = LogBufferLine::new();
        line.write("z".repeat(LOG_LINE_SIZE + 8).as_bytes());
        assert!(line.is_ready() && !line.is_complete());

        line.seal();
        assert!(!line.is_complete(), "sealing must not fake a whole line");
    }

    /// Bytes arriving one at a time must produce the same lines as bytes
    /// arriving all at once — every multi-byte character gets cut.
    ///
    /// Note what the caller has to do: `write` reports what it took, and
    /// whatever it did not take is the head of a character it is waiting to
    /// finish. Re-offering those bytes with the next one is the contract —
    /// dropping them is how a caller silently mangles every accented
    /// character that lands on a read boundary.
    #[test]
    fn byte_at_a_time_matches_all_at_once() {
        let input = "coração\nàéîõü\nplain\n".as_bytes();

        let mut line = LogBufferLine::new();
        let mut out = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        for byte in input {
            pending.push(*byte);
            let taken = line.write(&pending);
            pending.drain(..taken);
            if line.is_ready() {
                out.push(line.as_str().to_string());
                line.clear();
            }
        }
        assert!(pending.is_empty(), "bytes left unconsumed: {pending:?}");
        assert_eq!(out, ["coração", "àéîõü", "plain"]);
        assert_eq!(
            out,
            lines(input)
                .into_iter()
                .map(|(t, _)| t)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn clear_makes_it_writable_again() {
        let mut line = LogBufferLine::new();
        line.write(b"primeira\n");
        assert!(line.is_ready());

        line.clear();
        assert!(!line.is_ready() && line.is_empty());
        assert_eq!(line.write(b"segunda\n"), 8);
        assert_eq!(line.as_str(), "segunda");
    }

    /// A ready line takes nothing more: the caller has to hand it on and
    /// clear it, and a loop that forgets must not silently lose bytes.
    #[test]
    fn a_ready_line_refuses_further_bytes() {
        let mut line = LogBufferLine::new();
        line.write(b"cheia\n");
        assert_eq!(line.write(b"mais"), 0);
        assert_eq!(line.as_str(), "cheia");
    }
}

