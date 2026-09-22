//! Cutting a window out of what a reader already holds.
//!
//! The reader answers which lines; this answers which columns of them. It is
//! the one part of the module that touches nothing behind the log's lock —
//! no ring, no version, no arena — so it is a walk over memory the caller
//! already has, and it is kept apart for that reason.
//!
//! Where a line *ends* is not decided here. That is
//! [`LogReaderIter::ends_line`](crate::log::LogReaderIter), and a second
//! opinion about it would be a second chance for the window and the render to
//! disagree — the pieces arrive already cut, each saying whether it closed a
//! line.

use std::ops::Range;

use anstyle_parse::{Parser, Perform};
use unicode_width::UnicodeWidthChar;

use crate::log::log::LogWriterId;

/// A rectangle of a log: which lines, and which columns of them.
///
/// What [`copy_region`](crate::log::LogReader::copy_region) takes, and shaped like the
/// pane it is for — a window that moves in two directions over text bigger
/// than it in both, and bounded in both so that holding one costs the pane
/// and not the log.
///
/// Both pairs are half open, and both are counted from the edge the log grows
/// from.
///
/// It also carries [`parse_ansi`](LogRegion::parse_ansi), because what counts
/// as a column depends on it and nothing else here could answer that.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct LogRegion {
    /// The first line, counted back from the newest: `0` is the last line,
    /// `1` the one before it.
    ///
    /// Relative to the end because that is the addressing the log answers —
    /// it counts chunks pushed, not lines written, so there is no absolute
    /// line number to name. What it costs is that with output still arriving
    /// the same bounds name different lines each time, so a pane held still
    /// over a running process drifts.
    pub line_start: usize,
    /// One line past the last, so `2..20` is the eighteen above the last two.
    pub line_end: usize,
    /// The first column of each line to take.
    ///
    /// Columns, not bytes: this is a window onto a terminal, and a byte
    /// offset would cut characters in half. A character straddling either
    /// edge is left out rather than halved.
    pub column_start: usize,
    /// One column past the last.
    pub column_end: usize,
    /// Whether an escape sequence is read as one, and so takes no columns.
    ///
    /// Off, a `\x1b[31m` is four columns of text like any other and comes back
    /// in the line for whatever draws it to show — which is what somebody
    /// looking at what a proc actually writes asked for. It belongs to the
    /// unit the region is cut from, not to the screen: turning the colour off
    /// is the screen's business and does not change where a line ends.
    ///
    /// Defaults to on when a region arrives without it, so the hand written
    /// query that [`log`](crate::server) answers still means what it used to.
    #[serde(default = "parse_ansi_default")]
    pub parse_ansi: bool,
}

/// Escape sequences are read unless a region says otherwise.
fn parse_ansi_default() -> bool {
    true
}

impl LogRegion {
    /// A region from the two ranges it reads best as:
    /// `LogRegion::new(2..20, 0..512)`.
    ///
    /// Four fields rather than two [`Range`]s, because `Range` is an iterator
    /// and the standard library left it non-[`Copy`] on purpose: a `Copy`
    /// iterator would let `for x in range` consume a duplicate and leave the
    /// original looking untouched. Nothing iterates a region, so holding
    /// `Range`s inside one inherits that restriction for a reason it does not
    /// have, and pays for it in a clone at every call site.
    pub fn new(lines: Range<usize>, columns: Range<usize>) -> Self {
        Self {
            line_start: lines.start,
            line_end: lines.end,
            column_start: columns.start,
            column_end: columns.end,
            parse_ansi: parse_ansi_default(),
        }
    }

    /// The same rectangle, reading escape sequences or not — see
    /// [`parse_ansi`](LogRegion::parse_ansi).
    pub fn with_parse_ansi(mut self, parse_ansi: bool) -> Self {
        self.parse_ansi = parse_ansi;
        self
    }
}

/// One line of a copied region: the text, and who wrote it.
///
/// The id rather than an `is_note` flag, though telling a note from output is
/// what wanted it first: the same field answers which *process* a line came
/// from, and that is the other half of what a pane does with colour.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct LogLine {
    /// The clipped text of the line.
    pub text: String,
    /// Who wrote it — [`LogWriterId::NOTES`] for the supervisor's own lines.
    pub writer: LogWriterId,
}

/// Make sure `out` has an empty line at `line`, reusing the string there.
///
/// Which is what keeps a caller that hands the same `Vec` back on every call
/// from paying for a pane's worth of strings each time.
pub(super) fn open_line(out: &mut Vec<LogLine>, line: usize, writer: LogWriterId) {
    if line < out.len() {
        out[line].text.clear();
        out[line].writer = writer;
    } else {
        out.push(LogLine {
            text: String::new(),
            writer,
        });
    }
}

/// Append the part of `text` that falls inside the region's columns.
///
/// `column` is how far into the line the pieces before this one already
/// reached, and is advanced past all of `text` whether or not any of it was
/// taken — the window is over the line, and a piece entirely to the left of
/// it still moves the position along.
pub(super) fn clip_into(out: &mut String, text: &str, clip: &mut LogClip, region: LogRegion) {
    let bytes = text.as_bytes();
    for (index, character) in text.char_indices() {
        // Unparsed, a sequence is text: every character counts a column and
        // the window falls where the bytes fall.
        if region.parse_ansi {
            let mut shown = LogClipShown::default();
            for byte in &bytes[index..index + character.len_utf8()] {
                clip.parser.advance(&mut shown, *byte);
            }
            if shown.0.is_none() {
                // A sequence's own bytes. They take no columns, and are kept
                // wherever they fall — including left of the window, since
                // what colours the visible run is usually set before it.
                out.push(character);
                continue;
            }
        }
        let start = clip.column;
        clip.column += character.width().unwrap_or(0);
        if start >= region.column_end {
            // Past the right hand edge, and so is everything after it. The
            // position is left where it is because nothing will read it
            // again: no later piece of this line can be inside the window.
            return;
        }
        // A character straddling an edge is dropped: half of one is not
        // something a terminal can draw.
        if start >= region.column_start && clip.column <= region.column_end {
            out.push(character);
        }
    }
}

/// How far into a line [`clip_into`] has got.
///
/// One value and not two `&mut`s because a line arrives in pieces and both
/// halves have to survive the gap between them: at a chunk boundary a
/// `\x1b[1;32m` sitting across a chunk boundary is routine rather than a
/// corner, and a walk that forgot it was mid sequence would count the
/// parameters as text and cut the sequence in half.
///
/// Reset per line by the caller, so nothing an unterminated sequence does
/// reaches the line after it.
#[derive(Default)]
pub(super) struct LogClip {
    /// The column the next character lands in. A sequence's bytes take none.
    column: usize,
    /// Where the walk is in the escape grammar. Left untouched for a region
    /// that does not read sequences, which has no grammar to be in.
    ///
    /// Borrowed rather than written here, and from the crate `clap` already
    /// pulls in: the screen has to walk the same grammar to know which run
    /// each colour applies to, and two copies of where a sequence ends is a
    /// second chance for the window and the render to disagree.
    parser: Parser,
}

/// Whether the bytes just fed made a character the screen would show.
///
/// Every other callback is left at its default, which is the whole of what
/// this has to say: anything that is not text is a sequence, and a sequence
/// takes no columns whatever it turns out to mean.
#[derive(Default)]
pub(super) struct LogClipShown(pub(super) Option<char>);

impl Perform for LogClipShown {
    fn print(&mut self, character: char) {
        self.0 = Some(character);
    }
}
