use unicode_width::UnicodeWidthStr;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, StatefulWidget, Widget},
};

use crate::{
    log::LogRegion,
    runner::RunnerState,
    ui::{
        client::{UiLog, UiUnit},
        render::{DIGITS_MAX, decimal, room, set_clipped},
        theme::UiTheme,
    },
};

/// How many lines beyond the pane are asked for, on each side.
///
/// The slack that makes a page of scrolling cost no round trip. On both sides
/// because scrolling goes both ways — stretched backwards only, it would make
/// scrolling down cost exactly what it was meant to save.
const LOG_MARGIN: usize = 100;

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
}

impl UiRenderLogState {
    /// Put the pane back at the end of the log, and follow it again.
    ///
    /// Called when the selection moves — a scroll is a position in one unit's
    /// output and means nothing in another's — and when the user asks,
    /// because getting back to the end should not mean holding a key down.
    pub fn follow(&mut self) {
        self.scroll = 0;
    }

    /// Whether the pane is showing the end of the log as it arrives.
    pub fn is_following(&self) -> bool {
        self.scroll == 0
    }

    /// Scroll back by `lines`, or forward by a negative number of them.
    pub fn scroll_lines(&mut self, lines: isize) {
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
    /// either side and clipped to its width.
    pub fn region(&self) -> LogRegion {
        let (rows, columns) = self.size;
        let start = self.scroll.saturating_sub(LOG_MARGIN);
        LogRegion::new(start..self.scroll + rows + LOG_MARGIN, 0..columns)
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
    pub fn clamp(&mut self, held: usize) {
        self.scroll = self.scroll.min(held.saturating_sub(self.size.0));
    }
}

/// What a pane with no lines in it says, so an empty one does not read as a
/// pane that failed to draw.
const EMPTY: &str = "no output yet";

/// The log pane: a unit's output, titled with the unit rather than with the
/// word "log", so it says which output it is about before it has any.
pub struct UiRenderLog<'a> {
    unit: Option<&'a UiUnit>,
    log: Option<UiLog<'a>>,
    border: &'a Block<'a>,
    theme: &'a UiTheme,
}

impl<'a> UiRenderLog<'a> {
    /// The pane for `unit`'s `log`, drawn inside `border`.
    pub fn new(
        unit: Option<&'a UiUnit>,
        log: Option<UiLog<'a>>,
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
        let separator = &theme.symbol.separator;
        let plain = Style::new();
        let dim = plain.add_modifier(Modifier::DIM);
        let right = inner.right();

        let mut x = buffer.set_stringn(inner.x, area.y, " ", 1, plain).0;
        let Some(unit) = self.unit else {
            let x = set_clipped(buffer, theme, x, area.y, "log", room(x, right), dim);
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

        let status = theme.status.get(unit.state);
        let style = plain.fg(status.color);
        x = buffer
            .set_stringn(x, area.y, &status.label, room(x, right), style)
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

        // The title goes over the top border rather than through the block:
        // a block's title is a `Line`, and both building one and rendering
        // one allocate.
        self.draw_title(buffer, area, inner);
        draw_behind(buffer, self.theme, area, state.scroll);

        let lines = self.log.as_ref().map_or(&[][..], |log| {
            visible(log, state.scroll, text.height as usize)
        });
        if lines.is_empty() {
            let style = Style::new().add_modifier(Modifier::DIM);
            buffer.set_stringn(text.x, text.y, EMPTY, room(text.x, text.right()), style);
            return;
        }
        for (row, line) in lines.iter().enumerate() {
            buffer.set_stringn(
                text.x,
                text.y + row as u16,
                line,
                text.width as usize,
                Style::new(),
            );
        }
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
    let arrow = &theme.symbol.behind;
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
fn visible<'a>(log: &'a UiLog<'a>, scroll: usize, rows: usize) -> &'a [String] {
    let from_end = scroll.saturating_sub(log.region.line_start);
    let end = log.lines.len().saturating_sub(from_end);
    let start = end.saturating_sub(rows);
    &log.lines[start..end]
}
