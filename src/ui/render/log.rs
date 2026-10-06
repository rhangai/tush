use std::{
    ops::Range,
    time::{Duration, Instant},
};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, StatefulWidget, Widget},
};

use crate::{
    log::{LogColor, LogEffect, LogLine, LogRegion, LogStyle},
    runner::RunnerState,
    ui::{
        render::{DIGITS_MAX, decimal, room, set_clipped},
        theme::UiTheme,
    },
    view::{ViewLog, ViewUnit},
};

/// How many lines beyond the pane are asked for, on each side.
///
/// The slack that makes a page of scrolling cost no round trip. On both sides
/// because scrolling goes both ways — stretched backwards only, it would make
/// scrolling down cost exactly what it was meant to save.
const LOG_MARGIN: usize = 100;

/// How many columns of each line are asked for, whatever the pane's width.
///
/// A copy takes whole lines, and the client holds only what was asked for —
/// fetching the selected lines again when the button comes up would fetch
/// other lines, the log being addressed from an end that keeps moving. Capped
/// so a window still costs the pane and not the log: past this a line, a
/// minified bundle say, is copied cut.
const LOG_COLUMNS_MAX: usize = 4096;

/// How long the bottom border says a copy went out.
const COPIED_FOR: Duration = Duration::from_secs(2);

/// How soon a press on the same cell counts as the next click of a double
/// or triple one. crossterm reports presses only, so the pane counts them.
const MULTI_CLICK_WITHIN: Duration = Duration::from_millis(400);

/// Blank columns at each edge, so the output is not written onto the border.
const PAD_X: u16 = 1;

/// Where the log pane is looking from, and how big it turned out.
///
/// The size is written by the drawing and read by the *next* frame's request,
/// a pane's size not being known until the layout that makes it. The frame of
/// lag is invisible: [`LOG_MARGIN`] covers a pane that just grew.
#[derive(Default)]
pub struct UiRenderLogState {
    /// How many lines back from the newest the pane is showing. Zero follows
    /// the end of the log.
    scroll: usize,
    /// Rows and columns of text, as of the last frame drawn.
    size: (usize, usize),
    /// Where the text starts on screen, as of the last frame drawn — what a
    /// mouse position is measured from.
    origin: (u16, u16),
    /// The text being selected, while a selection is up.
    selection: Option<UiRenderLogSelection>,
    /// The rows on screen when the button went down, which the pane draws
    /// instead of the client's while [`selection`](Self::selection) is up:
    /// the log is addressed from its end, so with output arriving the
    /// client's lines slide out from under the selection. Kept between
    /// selections for its capacity; what it holds means nothing without one.
    frozen: Vec<LogLine>,
    /// When the last copy went out and how many lines it held, for the
    /// border to say so for [`COPIED_FOR`].
    copied: Option<(Instant, usize)>,
    /// When the last press was, on which cell, and which click it counted
    /// as. Outlives the selection, which every press clears.
    last_press: Option<(Instant, (usize, usize), u8)>,
}

/// Two cells of the text, as (row, column) from its top left: where the
/// button went down, and where the pointer is now. Either can be the earlier.
///
/// On a double or triple click (`clicks` above one) they are instead the
/// first and last cell of the word or line, and the pointer moves neither.
#[derive(Clone, Copy, PartialEq, Eq)]
struct UiRenderLogSelection {
    anchor: (usize, usize),
    head: (usize, usize),
    clicks: u8,
}

impl UiRenderLogSelection {
    /// Whether there is anything to show yet. A press that has not moved is
    /// still possibly a click, and highlighting its cell would flash it.
    fn is_shown(&self) -> bool {
        self.clicks > 1 || self.anchor != self.head
    }

    /// The two cells in reading order.
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.head < self.anchor {
            (self.head, self.anchor)
        } else {
            (self.anchor, self.head)
        }
    }
}

impl UiRenderLogState {
    /// Put the pane back at the end of the log, and follow it again.
    ///
    /// Called when the selection moves — a scroll is a position in one unit's
    /// output and means nothing in another's — and when the user asks,
    /// because getting back to the end should not mean holding a key down.
    /// Either way the frozen rows are no longer what the pane is about.
    pub fn follow(&mut self) {
        self.scroll = 0;
        self.last_press = None;
        self.clear_selection();
    }

    /// Whether the pane is showing the end of the log as it arrives.
    pub fn is_following(&self) -> bool {
        self.scroll == 0
    }

    /// Scroll back by `lines`, or forward by a negative number of them.
    ///
    /// Drops the selection: it is a stretch of the rows on screen, and those
    /// are about to be other rows.
    pub fn scroll_lines(&mut self, lines: isize) {
        self.clear_selection();
        let by = lines.unsigned_abs();
        self.scroll = if lines < 0 {
            self.scroll.saturating_sub(by)
        } else {
            self.scroll.saturating_add(by)
        };
    }

    /// Scroll back by `pages`, or forward by a negative number of them.
    ///
    /// A page is the pane less one line, so a line you just read stays on
    /// screen to land on.
    pub fn scroll_pages(&mut self, pages: isize) {
        let page = self.size.0.saturating_sub(1).max(1) as isize;
        self.scroll_lines(pages.saturating_mul(page));
    }

    /// The rectangle the pane wants: what it draws, plus [`LOG_MARGIN`] lines
    /// either side, [`LOG_COLUMNS_MAX`] wide.
    pub fn region(&self) -> LogRegion {
        let rows = self.size.0;
        let start = self.scroll.saturating_sub(LOG_MARGIN);
        LogRegion::new(start..self.scroll + rows + LOG_MARGIN, 0..LOG_COLUMNS_MAX)
    }

    /// Pull the scroll back to what there is to show.
    ///
    /// `held` is how far back the lines that came in reach. The client never
    /// says how much history it has — coming back with fewer lines than were
    /// asked for is how a view learns it hit the top.
    ///
    /// The limit puts the oldest line at the top of the pane, not the bottom:
    /// past that every further line of scroll buys a blank row, so a log
    /// shorter than the pane does not scroll at all.
    ///
    /// Not while a selection is up: `held` is the client's count, which keeps
    /// moving under rows that are frozen.
    pub fn clamp(&mut self, held: usize) {
        if self.selection.is_some() {
            return;
        }
        self.scroll = self.scroll.min(held.saturating_sub(self.size.0));
    }

    /// Start a selection at the cell under (`x`, `y`), freezing the rows of
    /// `log` the pane is showing.
    ///
    /// Drops any selection already up first, so a click anywhere clears one.
    /// A press outside the text, or over a pane with nothing in it, starts
    /// nothing. The second press on a cell selects the word under it and the
    /// third its whole line; a blank cell has no word, so selects nothing.
    pub fn select_from(&mut self, x: u16, y: u16, log: Option<ViewLog>) {
        self.clear_selection();
        let (rows, columns) = self.size;
        let (left, top) = self.origin;
        let (Some(column), Some(row)) = (x.checked_sub(left), y.checked_sub(top)) else {
            return;
        };
        let (row, column) = (row as usize, column as usize);
        if row >= rows || column >= columns {
            return;
        }
        let Some(log) = log else {
            return;
        };
        let lines = visible(&log, self.scroll, rows);
        if lines.is_empty() {
            return;
        }
        let clicks = self.count_click((row, column));
        self.frozen.clear();
        self.frozen.extend_from_slice(lines);
        let cell = (row.min(self.frozen.len() - 1), column);
        let (anchor, head) = match clicks {
            1 => (cell, cell),
            _ => {
                let Some(line) = self.frozen.get(row) else {
                    return;
                };
                let bounds = match clicks {
                    2 => word(&line.text, column),
                    _ => (!line.text.is_empty()).then_some((0, usize::MAX)),
                };
                let Some((first, last)) = bounds else {
                    return;
                };
                ((row, first), (row, last))
            }
        };
        self.selection = Some(UiRenderLogSelection {
            anchor,
            head,
            clicks,
        });
    }

    /// Record a press on `cell` and say which click it is: the next one of a
    /// run when on the same cell within [`MULTI_CLICK_WITHIN`], up to three,
    /// and the first of a new run otherwise.
    fn count_click(&mut self, cell: (usize, usize)) -> u8 {
        let now = Instant::now();
        let clicks = match self.last_press {
            Some((at, last, clicks))
                if last == cell && clicks < 3 && now - at < MULTI_CLICK_WITHIN =>
            {
                clicks + 1
            }
            _ => 1,
        };
        self.last_press = Some((now, cell, clicks));
        clicks
    }

    /// Move the selection's free end to the cell under (`x`, `y`), clamped to
    /// the text: a drag that leaves the pane holds at its edge.
    pub fn select_to(&mut self, x: u16, y: u16) {
        let Some(selection) = &mut self.selection else {
            return;
        };
        if selection.clicks > 1 {
            return;
        }
        let (left, top) = self.origin;
        let last_row = self.frozen.len().saturating_sub(1);
        let last_column = self.size.1.saturating_sub(1);
        let row = (y.saturating_sub(top) as usize).min(last_row);
        let column = (x.saturating_sub(left) as usize).min(last_column);
        selection.head = (row, column);
    }

    /// The button came up: a selection that never left the cell it started
    /// on was a click, and a click clears; anything else is copied into
    /// `out`, rows joined by newlines, and the return says there is a copy.
    ///
    /// A release on the pane's right edge takes the rest of its line, which
    /// is what dragging off the side of a line that does not fit means. A
    /// word or a line already ends where it ends, wherever that falls.
    pub fn select_end(&mut self, out: &mut String) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        let dragged = selection.clicks == 1;
        if dragged && selection.anchor == selection.head {
            self.clear_selection();
            return false;
        }
        out.clear();
        let edge = self.size.1.saturating_sub(1);
        let ((first_row, first_column), (last_row, last_column)) = selection.ordered();
        for row in first_row..=last_row {
            let Some(line) = self.frozen.get(row) else {
                break;
            };
            let from = if row == first_row { first_column } else { 0 };
            let to = if row == last_row && (last_column < edge || !dragged) {
                last_column
            } else {
                usize::MAX
            };
            if row > first_row {
                out.push('\n');
            }
            if let Some((_, bytes)) = overlap(&line.text, from, to) {
                out.push_str(&line.text[bytes]);
            }
        }
        self.copied = Some((Instant::now(), last_row - first_row + 1));
        true
    }

    /// Drop the selection, and with it the freeze and the word that a copy
    /// went out — left up, it would say so over a pane that moved on.
    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.copied = None;
    }
}

/// The log pane: a unit's output, titled with the unit rather than with the
/// word "log", so it says which output it is about before it has any.
pub struct UiRenderLog<'a> {
    unit: Option<&'a ViewUnit>,
    log: Option<ViewLog<'a>>,
    border: &'a Block<'a>,
    theme: &'a UiTheme,
}

impl<'a> UiRenderLog<'a> {
    /// The pane for `unit`'s `log`, drawn inside `border`.
    pub fn new(
        unit: Option<&'a ViewUnit>,
        log: Option<ViewLog<'a>>,
        border: &'a Block<'a>,
        theme: &'a UiTheme,
    ) -> Self {
        Self {
            unit,
            log,
            border,
            theme,
        }
    }

    /// Write the title over the top border: the unit, then its state spelled
    /// out, then the mode.
    ///
    /// The words the list gave up to fit on one line live here, where there
    /// is room for them and where they are about the unit being looked at.
    fn draw_title(&self, buffer: &mut Buffer, area: Rect, inner: Rect) {
        let theme = self.theme;
        let separator = &theme.symbols.separator;
        let plain = Style::new();
        let dim = plain.add_modifier(Modifier::DIM);
        let right = inner.right();

        let mut x = buffer.set_stringn(inner.x, area.y, " ", 1, plain).0;
        let Some(unit) = self.unit else {
            let x = set_clipped(
                buffer,
                theme,
                x,
                area.y,
                &theme.texts.log,
                room(x, right),
                dim,
            );
            buffer.set_stringn(x, area.y, " ", 1, plain);
            return;
        };

        x = set_clipped(
            buffer,
            theme,
            x,
            area.y,
            &unit.name,
            room(x, right),
            plain.add_modifier(Modifier::BOLD),
        );
        x = buffer
            .set_stringn(x, area.y, separator, room(x, right), dim)
            .0;

        let style = plain.fg(theme.colors.status(unit.state));
        let label = theme.texts.status(unit.state);
        x = buffer
            .set_stringn(x, area.y, label, room(x, right), style)
            .0;
        if let RunnerState::ExitError(Some(code)) = unit.state {
            let mut digits = [0u8; DIGITS_MAX];
            let number = decimal(code.get() as usize, &mut digits);
            x = buffer
                .set_stringn(x, area.y, number, room(x, right), style)
                .0;
        }

        if let Some(mode) = unit.mode.as_deref() {
            x = buffer
                .set_stringn(x, area.y, separator, room(x, right), dim)
                .0;
            x = set_clipped(buffer, theme, x, area.y, mode, room(x, right), dim);
        }
        buffer.set_stringn(x, area.y, " ", 1, plain);
    }
}

impl StatefulWidget for UiRenderLog<'_> {
    type State = UiRenderLogState;

    fn render(self, area: Rect, buffer: &mut Buffer, state: &mut Self::State) {
        let inner = self.border.inner(area);
        self.border.render(area, buffer);

        // Indented on both sides, and the region asked for is the narrower
        // rectangle that leaves — a line clipped to the pane's full width
        // would have its last columns written over the border.
        let text = Rect {
            x: inner.x + PAD_X,
            width: inner.width.saturating_sub(PAD_X * 2),
            ..inner
        };
        state.size = (text.height as usize, text.width as usize);
        state.origin = (text.x, text.y);

        // The title goes over the top border rather than through the block:
        // a block's title is a `Line`, and both building one and rendering
        // one allocate.
        self.draw_title(buffer, area, inner);
        // A frozen pane looks exactly like a process that went quiet, so it
        // says so — and while it is frozen, the count of lines below is of
        // lines it is not showing anyway.
        let texts = &self.theme.texts;
        let copied = state.copied.filter(|(at, _)| at.elapsed() < COPIED_FOR);
        if let Some((_, count)) = copied {
            let mut digits = [0u8; DIGITS_MAX];
            let number = decimal(count, &mut digits);
            let unit = if count == 1 {
                &texts.copied_line
            } else {
                &texts.copied_lines
            };
            draw_border_note(buffer, area, &[&texts.copied, number, unit]);
        } else if state
            .selection
            .is_some_and(|selection| selection.is_shown())
        {
            draw_border_note(buffer, area, &[&texts.selecting]);
        } else {
            draw_behind(buffer, self.theme, area, state.scroll);
        }

        let lines = match state.selection {
            Some(_) => &state.frozen[..],
            None => self.log.as_ref().map_or(&[][..], |log| {
                visible(log, state.scroll, text.height as usize)
            }),
        };
        if lines.is_empty() {
            let style = Style::new().add_modifier(Modifier::DIM);
            let empty = &self.theme.texts.empty;
            buffer.set_stringn(text.x, text.y, empty, room(text.x, text.right()), style);
            return;
        }
        // A note is the supervisor talking, not the process; dimmed for the
        // same reason the empty pane above is, so the output reads first.
        let plain = Style::new();
        let dim = plain.add_modifier(Modifier::DIM);
        for (row, line) in lines.iter().enumerate() {
            let base = if line.writer.is_note() { dim } else { plain };
            let y = text.y + row as u16;
            if self.theme.log_colors {
                draw_runs(buffer, line, text, y, base);
            } else {
                buffer.set_stringn(text.x, y, &line.text, text.width as usize, base);
            }
        }
        if let Some(selection) = state.selection.filter(UiRenderLogSelection::is_shown) {
            draw_selection(buffer, lines, text, selection);
        }
    }
}

/// Reverse the selected cells, over text already drawn.
///
/// The rows between the ends are selected to their end; past the pane there
/// is nothing drawn to reverse.
fn draw_selection(
    buffer: &mut Buffer,
    lines: &[LogLine],
    text: Rect,
    selection: UiRenderLogSelection,
) {
    let style = Style::new().add_modifier(Modifier::REVERSED);
    let width = text.width as usize;
    let ((first_row, first_column), (last_row, last_column)) = selection.ordered();
    for row in first_row..=last_row {
        let Some(line) = lines.get(row) else {
            break;
        };
        let from = if row == first_row { first_column } else { 0 };
        let to = if row == last_row {
            last_column
        } else {
            usize::MAX
        };
        let Some((columns, _)) = overlap(&line.text, from, to.min(width.saturating_sub(1))) else {
            continue;
        };
        let end = columns.end.min(width);
        let cells = Rect::new(
            text.x + columns.start as u16,
            text.y + row as u16,
            (end - columns.start) as u16,
            1,
        );
        buffer.set_style(cells, style);
    }
}

/// The characters of `text` with a cell in `from..=to`: the columns they
/// cover, and their bytes.
///
/// A wide character the range touches at all is taken whole. The one rule
/// for both the highlight and the copy, so the two cannot disagree on where
/// a selection starts or ends.
fn overlap(text: &str, from: usize, to: usize) -> Option<(Range<usize>, Range<usize>)> {
    let mut found: Option<(Range<usize>, Range<usize>)> = None;
    let mut column = 0;
    for (index, character) in text.char_indices() {
        if column > to {
            break;
        }
        let next = column + character.width().unwrap_or(0);
        if next > from {
            let byte_end = index + character.len_utf8();
            match &mut found {
                Some((columns, bytes)) => {
                    columns.end = next;
                    bytes.end = byte_end;
                }
                None => found = Some((column..next, index..byte_end)),
            }
        }
        column = next;
    }
    found
}

/// The first and last column of the run of non-blank characters covering
/// `column` of `text`, or `None` when that cell is blank or past the end.
///
/// Columns by the same width rule as [`overlap`], so the word found is the
/// word highlighted and copied.
fn word(text: &str, column: usize) -> Option<(usize, usize)> {
    let mut start = 0;
    let mut at = 0;
    let mut found = false;
    for character in text.chars() {
        let next = at + character.width().unwrap_or(0);
        if character.is_whitespace() {
            if found {
                break;
            }
            start = next;
        } else if next > column {
            found = true;
        }
        at = next;
    }
    (found && start <= column).then(|| (start, at - 1))
}

/// Write `pieces` into the bottom border, a space either side, ending one
/// short of the corner — where [`draw_behind`] counts the lines below.
///
/// Skipped when the border is too short to hold them.
fn draw_border_note(buffer: &mut Buffer, area: Rect, pieces: &[&str]) {
    let mut width = 2;
    for piece in pieces {
        width += piece.width();
    }
    let Some(x) = area.right().checked_sub(width as u16 + 1) else {
        return;
    };
    if x <= area.x {
        return;
    }

    let y = area.bottom().saturating_sub(1);
    let style = Style::new();
    let mut x = buffer.set_stringn(x, y, " ", 1, style).0;
    for piece in pieces {
        x = buffer.set_stringn(x, y, piece, piece.width(), style).0;
    }
    buffer.set_stringn(x, y, " ", 1, style);
}

/// Draw one line as the runs the log found in it.
///
/// Each run is a slice of the line's own text handed straight to the buffer,
/// so a frame of coloured output allocates nothing — the strings were built
/// when the region was copied and are not built again here.
///
/// A line with no runs is one `set_stringn`, which is nearly all of them.
fn draw_runs(buffer: &mut Buffer, line: &LogLine, area: Rect, y: u16, base: Style) {
    let right = area.right();
    let mut x = area.x;
    let mut written = 0;
    for (index, run) in line.styles.iter().enumerate() {
        // Text before the first run, which is a line that starts in no colour
        // and picks one up part way along.
        if run.index > written {
            x = buffer
                .set_stringn(x, y, &line.text[written..run.index], room(x, right), base)
                .0;
            written = run.index;
        }
        let end = line
            .styles
            .get(index + 1)
            .map_or(line.text.len(), |next| next.index);
        let style = styled(base, run.style);
        x = buffer
            .set_stringn(x, y, &line.text[written..end], room(x, right), style)
            .0;
        written = end;
    }
    if written < line.text.len() {
        buffer.set_stringn(x, y, &line.text[written..], room(x, right), base);
    }
}

/// What the log found, as the screen draws it.
///
/// Folded onto `base` rather than replacing it, so a note stays dim whatever
/// colour the process asked for.
fn styled(base: Style, style: LogStyle) -> Style {
    let mut out = base;
    if let Some(foreground) = style.foreground {
        out = out.fg(color(foreground));
    }
    if let Some(background) = style.background {
        out = out.bg(color(background));
    }
    for effect in style.effects {
        out = out.add_modifier(modifier(effect));
    }
    out
}

/// A colour a sequence named, as a terminal takes it.
///
/// An index stays an index: the sixteen named colours and the 256 palette are
/// one table, and which of its cells is which is the terminal's business and
/// not ours — the theme's own colours are the ones this program chooses.
fn color(color: LogColor) -> Color {
    match color {
        LogColor::Indexed(index) => Color::Indexed(index),
        LogColor::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

/// One decoration, as ratatui spells it.
fn modifier(effect: LogEffect) -> Modifier {
    match effect {
        LogEffect::Bold => Modifier::BOLD,
        LogEffect::Dim => Modifier::DIM,
        LogEffect::Italic => Modifier::ITALIC,
        LogEffect::Underline => Modifier::UNDERLINED,
        LogEffect::Reverse => Modifier::REVERSED,
        LogEffect::Strike => Modifier::CROSSED_OUT,
    }
}

/// Say how far below the pane the end of the log is, when it is not the pane.
///
/// Scrolled up, a log goes on without you and the pane stops changing —
/// which looks exactly like a process that has gone quiet. The confusion is
/// the wrong way round, since the one that looks broken is the one where
/// everything is working.
///
/// Drawn only while scrolled, at the bottom because that is the edge you are
/// away from. The number is exact: the scroll *is* how many lines sit between
/// the last row and the newest line.
fn draw_behind(buffer: &mut Buffer, theme: &UiTheme, area: Rect, scroll: usize) {
    if scroll == 0 {
        return;
    }
    let mut digits = [0u8; DIGITS_MAX];
    let count = decimal(scroll, &mut digits);

    // " ↓ 42 ", ending one short of the corner.
    let arrow = &theme.symbols.behind;
    let width = (arrow.width() + count.width() + 1) as u16;
    let Some(x) = area.right().checked_sub(width + 1) else {
        return;
    };
    if x <= area.x {
        return;
    }

    let y = area.bottom().saturating_sub(1);
    let style = Style::new();
    let mut x = buffer.set_stringn(x, y, arrow, arrow.width(), style).0;
    x = buffer
        .set_stringn(x, y, count, room(x, area.right()), style)
        .0;
    buffer.set_stringn(x, y, " ", 1, style);
}

/// The part of a fetched region the pane is actually showing.
///
/// The region is wider than the pane on both sides, so the visible rows are a
/// slice out of the middle of it. The lines run oldest first and the last is
/// at distance `region.line_start`, so the line at distance `d` sits
/// `d - region.line_start` from the end of the slice; the pane wants
/// distances from `scroll` upwards, so that is where its last row is.
///
/// Everything saturates because the region that came back need not be the one
/// asked for — the pane draws the overlap rather than nothing.
fn visible<'a>(log: &'a ViewLog<'a>, scroll: usize, rows: usize) -> &'a [LogLine] {
    let from_end = scroll.saturating_sub(log.region.line_start);
    let end = log.lines.len().saturating_sub(from_end);
    let start = end.saturating_sub(rows);
    &log.lines[start..end]
}
