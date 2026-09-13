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
        render::{room, set_clipped},
    },
};

/// How many lines beyond the pane are asked for, on each side of it.
///
/// The part of the region that is a buffer rather than a request: with this
/// much slack either way, a page of scrolling in either direction is already
/// in hand and costs no round trip. Clipped to the pane's width it is a few
/// tens of kibibytes, whatever the log behind it is.
///
/// On both sides because scrolling goes both ways: a region that only
/// stretched backwards would make scrolling up free and scrolling down cost
/// exactly what it was meant to save.
const LOG_MARGIN: usize = 100;

/// Where the log pane is looking from, and how big it turned out.
///
/// The size is written back by the drawing and read by the *next* frame's
/// request, since the size of a pane is not known until the layout that makes
/// it. A frame of lag, and invisible: it sizes a region that already has
/// [`LOG_MARGIN`] lines of slack either way, so a pane that just grew is
/// still covered by what was fetched for the old one.
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
    /// Two callers, and they want it for the same reason from opposite ends.
    /// The selection moving: a scroll is a position in one unit's output and
    /// means nothing in another's, and a pane that opens in the middle of a
    /// log for no reason the user can see reads as output having gone
    /// missing. And the user asking: scrolled up, a log goes on without you,
    /// and getting back to the end of it should not be a matter of holding a
    /// key down.
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
    /// A page is the pane less a line of overlap, which is what makes a page
    /// turn readable: a line you have just read stays on screen to land on.
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
    /// `held` is how far back the lines that came in actually reach. The
    /// client never says how much history it has; it says what it found, and
    /// coming back with fewer lines than were asked for is how a view learns
    /// it reached the top.
    ///
    /// The limit is where the oldest line reaches the top of the pane, not
    /// the bottom: past that the window hangs off the end of the history and
    /// every further line of scroll buys a blank row. A log shorter than the
    /// pane therefore does not scroll at all.
    pub fn clamp(&mut self, held: usize) {
        self.scroll = self.scroll.min(held.saturating_sub(self.size.0));
    }
}

/// The log pane: a unit's output, and its name over the top border.
///
/// Titled with the unit rather than with the word "log", so the pane says
/// which output it is about before it has any.
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

/// The part of a fetched region the pane is actually showing.
///
/// The region is wider than the pane on both sides — that is the margin the
/// view scrolls within without asking again — so the visible rows are a slice
/// out of the middle of it, and finding them is arithmetic on distances from
/// the end of the log.
///
/// The lines run oldest first and the last is at distance
/// `region.line_start`, so the line at distance `d` sits
/// `d - region.line_start` from the end of the slice. The pane wants
/// distances from `scroll` upwards, so that is where its last row is.
///
/// Everything saturates because the region that came back need not be the one
/// asked for: a client behind a scroll hands back what it has, and the pane
/// draws the overlap rather than nothing.
fn visible<'a>(log: &'a UiLog<'a>, scroll: usize, rows: usize) -> &'a [String] {
    let from_end = scroll.saturating_sub(log.region.line_start);
    let end = log.lines.len().saturating_sub(from_end);
    let start = end.saturating_sub(rows);
    &log.lines[start..end]
}
