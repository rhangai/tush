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

use anstyle_parse::{Params, Parser, Perform};
use enum_bitset::EnumBitset;
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
    /// Over a socket the asking end does not settle it: a region that arrives
    /// without it defaults to on, and the server overwrites it with what the
    /// proc was declared with before cutting anything. What comes back says
    /// which it was.
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
    /// The clipped text of the line, with no escape sequences left in it when
    /// the region read them — what they said is in
    /// [`styles`](LogLine::styles) instead.
    pub text: String,
    /// Who wrote it — [`LogWriterId::NOTES`] for the supervisor's own lines.
    pub writer: LogWriterId,
    /// Where the style changes along [`text`](LogLine::text), in order, and
    /// empty for a line drawn in one style — which is most of them.
    ///
    /// A run list and not a style per character: a colour holds for a word or
    /// a line, so this is a handful of entries where the other shape would be
    /// one per byte.
    pub styles: Vec<LogLineStyle>,
}

/// Where a run of one style starts: a byte index into
/// [`LogLine::text`], and what to draw from there until the next entry.
///
/// Byte and not column, because what reads it slices the string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogLineStyle {
    /// Where the run starts, as a byte index into the line's text.
    pub index: usize,
    /// What the sequences up to that point added up to.
    pub style: LogStyle,
}

/// What a run of text is drawn in: the SGR parameters, resolved.
///
/// The screen's own style type would do, except that this crosses a socket —
/// so it is spelled out here, in what the sequences actually said, and
/// whatever draws it translates. `anstyle` would have been the obvious type
/// to borrow and cannot be: it has no serde support to enable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogStyle {
    /// The colour the text is drawn in, or `None` for the terminal's own.
    pub foreground: Option<LogColor>,
    /// The colour behind it, on the same terms.
    pub background: Option<LogColor>,
    /// Bold, dim and the rest — whichever of them the sequences asked for.
    pub effects: LogEffectSet,
}

/// One decoration a sequence can ask for, apart from colour.
///
/// A set and not a handful of `bool`s, and generated rather than written: a
/// caller asks `effects.contains(LogEffect::Bold)` instead of remembering
/// which bit bold was, and the set still costs one integer.
#[derive(EnumBitset, Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LogEffect {
    Bold,
    Dim,
    Italic,
    Underline,
    Reverse,
    Strike,
}

impl LogStyle {
    /// Fold one `m` sequence's parameters in.
    ///
    /// Folded rather than replaced, because that is what a terminal does: a
    /// `\x1b[1m` after a `\x1b[31m` is bold *and* red, and only a `0` clears
    /// what came before.
    fn apply(&mut self, params: &Params) {
        let mut params = params.iter();
        while let Some(param) = params.next() {
            let Some(first) = param.first().copied() else {
                continue;
            };
            match first {
                0 => *self = Self::default(),
                1 => self.effects.insert(LogEffect::Bold),
                2 => self.effects.insert(LogEffect::Dim),
                3 => self.effects.insert(LogEffect::Italic),
                4 => self.effects.insert(LogEffect::Underline),
                7 => self.effects.insert(LogEffect::Reverse),
                9 => self.effects.insert(LogEffect::Strike),
                // `22` turns off bold and dim together, which is the one
                // asymmetry in the set: two effects, one code to clear them.
                22 => self.effects -= LogEffect::Bold | LogEffect::Dim,
                23 => self.effects.remove(LogEffect::Italic),
                24 => self.effects.remove(LogEffect::Underline),
                27 => self.effects.remove(LogEffect::Reverse),
                29 => self.effects.remove(LogEffect::Strike),
                30..=37 => self.foreground = Some(LogColor::Indexed(first as u8 - 30)),
                90..=97 => self.foreground = Some(LogColor::Indexed(first as u8 - 90 + 8)),
                39 => self.foreground = None,
                40..=47 => self.background = Some(LogColor::Indexed(first as u8 - 40)),
                100..=107 => self.background = Some(LogColor::Indexed(first as u8 - 100 + 8)),
                49 => self.background = None,
                // A colour the sequence spells out. Left alone when it is
                // malformed rather than cleared, since half a parameter list
                // says nothing about what the colour should become.
                38 => {
                    if let Some(color) = sgr_color(param, &mut params) {
                        self.foreground = Some(color);
                    }
                }
                48 => {
                    if let Some(color) = sgr_color(param, &mut params) {
                        self.background = Some(color);
                    }
                }
                _ => {}
            }
        }
    }
}

/// A colour a sequence named.
///
/// One variant for every palette index rather than one for the sixteen names
/// and another for the 256 palette: `\x1b[31m` and `\x1b[38;5;1m` mean the same
/// cell of the same table, and a terminal is handed an index either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LogColor {
    /// An index into the terminal's palette: 0-7 the plain colours, 8-15 the
    /// bright ones, up to 255 for the rest.
    Indexed(u8),
    /// A colour the sequence gave in full.
    Rgb(u8, u8, u8),
}

/// The colour a `38` or `48` introduces: `5;n` for a palette index, `2;r;g;b`
/// for one spelled out.
///
/// Written out here because the numbers may arrive either as subparameters of
/// the `38` itself (`38:5:1`) or as the parameters after it (`38;5;1`), and
/// both spellings are in the wild.
fn sgr_color<'a>(param: &[u16], rest: &mut impl Iterator<Item = &'a [u16]>) -> Option<LogColor> {
    let mut subs = param[1..].iter().copied();
    let mut next = move || {
        subs.next()
            .or_else(|| rest.next().and_then(|p| p.first().copied()))
    };
    match next()? {
        5 => Some(LogColor::Indexed(next()? as u8)),
        2 => Some(LogColor::Rgb(next()? as u8, next()? as u8, next()? as u8)),
        _ => None,
    }
}

/// Make sure `out` has an empty line at `line`, reusing the string there.
///
/// Which is what keeps a caller that hands the same `Vec` back on every call
/// from paying for a pane's worth of strings each time.
pub(super) fn open_line(out: &mut Vec<LogLine>, line: usize, writer: LogWriterId) {
    if line < out.len() {
        out[line].text.clear();
        out[line].styles.clear();
        out[line].writer = writer;
    } else {
        out.push(LogLine {
            text: String::new(),
            writer,
            styles: Vec::new(),
        });
    }
}

/// Append the part of `text` that falls inside the region's columns, and the
/// style each run of it is drawn in.
///
/// `column` is how far into the line the pieces before this one already
/// reached, and is advanced past all of `text` whether or not any of it was
/// taken — the window is over the line, and a piece entirely to the left of
/// it still moves the position along. A sequence left of the window still
/// counts for the same reason: it is what colours the run that follows.
pub(super) fn clip_into(line: &mut LogLine, text: &str, clip: &mut LogClip, region: LogRegion) {
    let bytes = text.as_bytes();
    for (index, character) in text.char_indices() {
        // Unparsed, a sequence is text: every character counts a column, the
        // window falls where the bytes fall, and nothing is styled.
        if region.parse_ansi {
            let mut perform = LogClipPerform {
                shown: None,
                style: &mut clip.style,
            };
            for byte in &bytes[index..index + character.len_utf8()] {
                clip.parser.advance(&mut perform, *byte);
            }
            if perform.shown.is_none() {
                // A sequence's own bytes. They are consumed rather than kept:
                // what they said is in `clip.style` now, and the text is left
                // as the text.
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
            // The run is opened by the first character drawn in it, so a
            // sequence nothing visible follows costs no entry.
            if clip.style != clip.written {
                line.styles.push(LogLineStyle {
                    index: line.text.len(),
                    style: clip.style,
                });
                clip.written = clip.style;
            }
            line.text.push(character);
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
    /// pulls in: writing a second one is writing a second opinion about where
    /// a sequence ends.
    parser: Parser,
    /// What the sequences so far add up to, which is what the next visible
    /// character is drawn in.
    style: LogStyle,
    /// The style the last run opened with, so a sequence that changes nothing
    /// — a colour set twice, a reset of what was already default — does not
    /// open a run saying the same thing.
    written: LogStyle,
}

/// What the bytes just fed turned out to be: a character the screen shows, or
/// a sequence — and if the sequence said something about colour, it is folded
/// into `style` on the way past.
///
/// Every other callback is left at its default, which is the whole of what
/// this has to say: anything else is a sequence that takes no columns and
/// changes nothing about how the text is drawn.
pub(super) struct LogClipPerform<'a> {
    shown: Option<char>,
    style: &'a mut LogStyle,
}

impl Perform for LogClipPerform<'_> {
    fn print(&mut self, character: char) {
        self.shown = Some(character);
    }

    /// `m` is the only one that matters here: everything else a CSI can be —
    /// moving the cursor, clearing the screen — is a terminal's business and
    /// not a copied region's.
    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], _ignore: bool, action: u8) {
        if action == b'm' {
            self.style.apply(params);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Walk `text` the way [`clip_into`] does — one perform per character,
    /// one style across all of them — and report what it ends in.
    fn style_of(text: &str) -> LogStyle {
        let mut parser: Parser = Parser::default();
        let mut style = LogStyle::default();
        for byte in text.as_bytes() {
            let mut perform = LogClipPerform {
                shown: None,
                style: &mut style,
            };
            parser.advance(&mut perform, *byte);
        }
        style
    }

    /// A terminal accumulates: the second sequence says nothing about the
    /// colour the first set, so the colour stays.
    #[test]
    fn a_sequence_folds_into_what_came_before() {
        let style = style_of("\u{1b}[31m\u{1b}[1m");
        assert_eq!(style.foreground, Some(LogColor::Indexed(1)));
        assert_eq!(style.effects, LogEffect::Bold.into());
    }

    #[test]
    fn a_reset_clears_everything_at_once() {
        assert_eq!(style_of("\u{1b}[1;31;45m\u{1b}[0m"), LogStyle::default());
    }

    /// `22` is the asymmetry: two flags, one code that clears both.
    #[test]
    fn bold_and_dim_go_out_together() {
        let style = style_of("\u{1b}[1;2;3m\u{1b}[22m");
        assert_eq!(style.effects, LogEffect::Italic.into());
    }

    /// The bright codes are the top half of the same palette, not a set of
    /// their own.
    #[test]
    fn a_bright_colour_is_a_palette_index() {
        assert_eq!(
            style_of("\u{1b}[91m").foreground,
            Some(LogColor::Indexed(9))
        );
    }

    /// Both spellings are in the wild, and they mean the same colour.
    #[test]
    fn a_palette_colour_reads_the_same_either_way() {
        let semicolons = style_of("\u{1b}[38;5;208m").foreground;
        let colons = style_of("\u{1b}[38:5:208m").foreground;
        assert_eq!(semicolons, Some(LogColor::Indexed(208)));
        assert_eq!(colons, semicolons);
    }

    #[test]
    fn a_colour_spelled_out_is_read_in_full() {
        assert_eq!(
            style_of("\u{1b}[48;2;10;20;30m").background,
            Some(LogColor::Rgb(10, 20, 30))
        );
    }

    /// Half a parameter list says nothing about what the colour should
    /// become, so what is already set stays.
    #[test]
    fn a_malformed_colour_leaves_the_last_one_alone() {
        let style = style_of("\u{1b}[31m\u{1b}[38;5m");
        assert_eq!(style.foreground, Some(LogColor::Indexed(1)));
    }
}
