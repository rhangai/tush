//! The screen, as widgets.
//!
//! Each pane is a [`Widget`] of its own — [`UiRenderUnits`] and
//! [`UiRenderLog`] — with whatever it has to remember between frames as its
//! [`StatefulWidget::State`]. [`UiRender`] is the frame they sit in: it owns
//! the layout, holds their states, and puts each one in its area.
//!
//! # Why they are widgets
//!
//! Because that is the seam ratatui already draws, and it is a good one: a
//! pane that renders into an area and a buffer needs nothing else at all, so
//! it can be drawn on its own in a test and read straight back. The
//! alternative — one struct with a `draw_this` and a `draw_that` reaching
//! into its own fields — composes with nothing and can only be tested through
//! a whole terminal.
//!
//! # Why none of them build anything
//!
//! Every pane writes into the buffer rather than assembling text to hand to a
//! widget that will. A `Line` of `Span`s allocates in the building and again
//! in the rendering, several hundred times a second, to say what it said last
//! frame. Written directly there is nothing to allocate and so nothing to
//! cache — which is what took a cache, a copy of every row, and the
//! comparison that decided when the copy went stale, back out of this module.
//!
//! `drawing_a_frame_allocates_nothing` is where that is checked rather than
//! claimed.

mod log;
mod units;

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Widget},
};
use unicode_width::UnicodeWidthStr;

use crate::{log::LogRegion, ui::client::UiClient};

#[allow(unused_imports)]
pub use log::{UiRenderLog, UiRenderLogState};

#[allow(unused_imports)]
pub use units::{Move, UiRenderUnits, UiRenderUnitsState};

use crate::ui::client::UiUnit;

/// How wide the units column is, in columns.
///
/// A fixed width rather than a share of the terminal, because what has to fit
/// is a name — which does not grow when the window does. A proportional split
/// would spend half a wide terminal on whitespace that the log could have
/// used.
///
/// It is also what makes the list stay put: a name lands in the same place
/// whatever else is on screen, so the cursor is not somewhere new after a
/// resize.
const UNITS_WIDTH: u16 = 30;

/// The frame the panes are drawn in.
///
/// What it holds is what a screen has to remember between frames and nothing
/// else: the two pane states, the layout, and the two things that are built
/// once because building them allocates.
pub struct UiRender {
    /// Where the units pane is looking from.
    units: UiRenderUnitsState,
    /// Where the log pane is looking from, and how big it came out.
    log: UiRenderLogState,
    /// The areas the frame was last laid out into, and the frame they came
    /// from.
    ///
    /// [`Layout::areas`] allocates on every call even when its own solver
    /// cache hits, so the answer is kept rather than asked for again: it only
    /// changes when the terminal is resized.
    areas: (Rect, [Rect; 3]),
    /// The two splits the frame is laid out with.
    ///
    /// Built once because a [`Layout`] owns a `Vec` of its constraints, so
    /// constructing one per frame is two allocations to describe a split that
    /// is the same split every time.
    body: Layout,
    panes: Layout,
    /// The border both panes are drawn in, lent to each of them.
    ///
    /// Untitled: a block's title is a [`Line`], and rendering one allocates,
    /// so the log pane writes its own over the border.
    border: Block<'static>,
    /// The line of key bindings, which never changes at all.
    hints: Line<'static>,
}

impl UiRender {
    /// A screen showing the first unit, following the end of its log.
    pub fn new() -> Self {
        Self {
            units: UiRenderUnitsState::default(),
            log: UiRenderLogState::default(),
            areas: (Rect::ZERO, [Rect::ZERO; 3]),
            body: Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]),
            panes: Layout::horizontal([Constraint::Length(UNITS_WIDTH), Constraint::Fill(1)]),
            border: Block::bordered(),
            hints: key_hints(),
        }
    }

    /// Draw one frame: the units on the left, their log on the right, and a
    /// line at the bottom.
    ///
    /// All this does is place the panes. What each of them draws is its own,
    /// and what it takes from outside is the client — only to read.
    pub fn draw<C: UiClient>(&mut self, frame: &mut Frame, client: &C) {
        let [footer, units_area, log_area] = self.areas(frame.area());
        let title = self
            .selected_unit(client)
            .map_or("log", |unit| unit.name.as_str());

        frame.render_widget(UiRenderHints(&self.hints), footer);
        frame.render_stateful_widget(
            UiRenderLog::new(title, client.log(), &self.border),
            log_area,
            &mut self.log,
        );
        frame.render_stateful_widget(
            UiRenderUnits::new(client.units(), &self.border),
            units_area,
            &mut self.units,
        );
    }

    /// The footer, the units pane and the log pane, laid out from the whole
    /// frame.
    fn areas(&mut self, frame: Rect) -> [Rect; 3] {
        if self.areas.0 != frame {
            let [body, footer] = self.body.areas(frame);
            let [units, log] = self.panes.areas(body);
            self.areas = (frame, [footer, units, log]);
        }
        self.areas.1
    }

    /// The unit the cursor is on.
    pub fn selected_unit<'a, C: UiClient>(&self, client: &'a C) -> Option<&'a UiUnit> {
        client.units().get(self.units.cursor())
    }

    /// Move the cursor, and put the log pane back at the end.
    pub fn select(&mut self, movement: Move, units: usize) {
        self.units.select(movement, units);
        self.log.follow();
    }

    /// Scroll the log pane back by `lines`, or forward by a negative number.
    pub fn scroll_log_lines(&mut self, lines: isize) {
        self.log.scroll_lines(lines);
    }

    /// Scroll the log pane back by `pages`, or forward by a negative number.
    pub fn scroll_log_pages(&mut self, pages: isize) {
        self.log.scroll_pages(pages);
    }

    /// Put the log pane back at the end and follow it again.
    pub fn follow_log(&mut self) {
        self.log.follow();
    }

    /// Whether the log pane is showing the end as it arrives.
    pub fn is_following_log(&self) -> bool {
        self.log.is_following()
    }

    /// The rectangle the log pane wants.
    pub fn log_region(&self) -> LogRegion {
        self.log.region()
    }

    /// Pull the log scroll back to what the client actually found.
    pub fn clamp_log(&mut self, held: usize) {
        self.log.clamp(held);
    }
}

impl Default for UiRender {
    fn default() -> Self {
        Self::new()
    }
}

/// The footer: a line put down span by span.
///
/// A widget of its own because [`Line`]'s own rendering allocates, and this
/// one never changes — so it is written the way the panes are.
struct UiRenderHints<'a>(&'a Line<'a>);

impl Widget for UiRenderHints<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let mut x = area.x;
        for span in &self.0.spans {
            let room = room(x, area.right());
            if room == 0 {
                return;
            }
            (x, _) = buffer.set_stringn(x, area.y, &span.content, room, span.style);
        }
    }
}

/// How many columns are left between `x` and `right`.
fn room(x: u16, right: u16) -> usize {
    right.saturating_sub(x) as usize
}

/// Put `text` down in at most `room` columns, ending in an ellipsis if it had
/// to be cut, and report where it ended.
///
/// The buffer clips on its own; what it will not do is say that it clipped.
/// A name that simply stops looks like a name that is spelled that way, and
/// the mark is the difference.
fn set_clipped(buffer: &mut Buffer, x: u16, y: u16, text: &str, room: usize, style: Style) -> u16 {
    if text.width() <= room {
        return buffer.set_stringn(x, y, text, room, style).0;
    }
    // One column goes to the mark, so anything narrower has room for the mark
    // and nothing else.
    let (end, _) = buffer.set_stringn(x, y, text, room.saturating_sub(1), style);
    buffer.set_stringn(end, y, "…", 1, style).0
}

/// The bottom line: what the keys do.
///
/// There is nothing else competing for it yet. When a command can report
/// having been refused, this is the line it will have to share.
fn key_hints() -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        key_hint("↑↓", "move"),
        key_hint("⏎", "start"),
        key_hint("r", "restart"),
        key_hint("⌫", "stop"),
        key_hint("pgup/dn", "scroll"),
        key_hint("end", "follow"),
        key_hint("q", "quit"),
    ])
}

/// `key`, then what it does, dimmed, with a gap before the next one.
fn key_hint<'a>(key: &'a str, what: &'a str) -> Span<'a> {
    Span::raw(format!("{key} {what}   ")).dim()
}
