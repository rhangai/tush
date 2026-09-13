use unicode_width::UnicodeWidthStr;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, StatefulWidget, Widget},
};

use crate::{
    log::LogRegion,
    ui::{
        client::UiLog,
        render::{DIGITS_MAX, decimal, room, set_clipped},
    },
};

/// How many lines beyond the pane are asked for, on each side.
///
/// The slack that makes a page of scrolling cost no round trip. On both sides
/// because scrolling goes both ways — stretched backwards only, it would make
/// scrolling down cost exactly what it was meant to save.
const LOG_MARGIN: usize = 100;

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

/// The log pane: a unit's output, titled with the unit rather than with the
/// word "log", so it says which output it is about before it has any.
pub struct UiRenderLog<'a> {
    title: &'a str,
    log: Option<UiLog<'a>>,
    border: &'a Block<'a>,
}

impl<'a> UiRenderLog<'a> {
    /// The pane for `log`, titled `title`, drawn inside `border`.
    pub fn new(title: &'a str, log: Option<UiLog<'a>>, border: &'a Block<'a>) -> Self {
        Self { title, log, border }
    }
}

impl StatefulWidget for UiRenderLog<'_> {
    type State = UiRenderLogState;

    fn render(self, area: Rect, buffer: &mut Buffer, state: &mut Self::State) {
        let inner = self.border.inner(area);
        self.border.render(area, buffer);
        state.size = (inner.height as usize, inner.width as usize);

        // The title goes over the top border, in the three pieces it is made
        // of, rather than through the block: a block's title is a `Line`, and
        // both building one and rendering one allocate.
        let mut x = buffer.set_stringn(inner.x, area.y, " ", 1, Style::new()).0;
        x = set_clipped(
            buffer,
            x,
            area.y,
            self.title,
            room(x, inner.right()),
            Style::new(),
        );
        buffer.set_stringn(x, area.y, " ", 1, Style::new());

        draw_behind(buffer, area, state.scroll);

        let Some(log) = self.log else {
            return;
        };
        for (row, text) in visible(&log, state.scroll, inner.height as usize)
            .iter()
            .enumerate()
        {
            buffer.set_stringn(
                inner.x,
                inner.y + row as u16,
                text,
                inner.width as usize,
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
fn draw_behind(buffer: &mut Buffer, area: Rect, scroll: usize) {
    if scroll == 0 {
        return;
    }
    let mut digits = [0u8; DIGITS_MAX];
    let count = decimal(scroll, &mut digits);

    // " ↓ 42 ", ending one short of the corner.
    let width = 4 + count.width() as u16;
    let Some(x) = area.right().checked_sub(width + 1) else {
        return;
    };
    if x <= area.x {
        return;
    }

    let y = area.bottom().saturating_sub(1);
    let style = Style::new();
    let mut x = buffer.set_stringn(x, y, " ↓ ", 3, style).0;
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
