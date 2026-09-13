//! The screen, as widgets.
//!
//! Each pane is a [`Widget`] of its own, with whatever it remembers between
//! frames as its `StatefulWidget::State`. [`UiRender`] is the frame they
//! sit in: it owns the layout, holds their states, and places each one.
//!
//! None of them build text to hand to a widget that would render it. A `Line`
//! of `Span`s allocates in the building and again in the rendering, several
//! hundred times a second, to say what it said last frame — writing into the
//! buffer allocates nothing and so needs no cache either.

mod log;
mod menu;
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

use arcstr::ArcStr;

use crate::{log::LogRegion, ui::client::UiClient, unit::UnitChoice};

#[allow(unused_imports)]
pub use log::{UiRenderLog, UiRenderLogState};

#[allow(unused_imports)]
pub use menu::{UiMenuChoice, UiRenderMenu, UiRenderMenuState};

#[allow(unused_imports)]
pub use units::{Move, UiRenderUnits, UiRenderUnitsState};

use crate::ui::client::UiUnit;

/// How wide the units column is.
///
/// Fixed rather than a share of the terminal, because what has to fit is a
/// name and a name does not grow when the window does. It also keeps the list
/// still: a name lands in the same place whatever else is on screen.
const UNITS_WIDTH: u16 = 30;

/// The mark on a selected row. The trailing space is part of it, and every
/// row is indented by its width so the text stays in one column.
///
/// Up here because both lists use it.
const CURSOR: &str = "> ";

/// The frame the panes are drawn in.
///
/// Holds what a screen must remember between frames and nothing else: the
/// pane states, the layout, and the two things built once because building
/// them allocates.
pub struct UiRender {
    /// Where the units pane is looking from.
    units: UiRenderUnitsState,
    /// Where the log pane is looking from, and how big it came out.
    log: UiRenderLogState,
    /// What the action menu is showing, when it is open.
    menu: UiRenderMenuState,
    /// The areas last laid out, and the frame they came from. Kept because
    /// [`Layout::areas`] allocates on every call even when its solver cache
    /// hits, and they only change on a resize.
    areas: (Rect, [Rect; 3]),
    /// The two splits the frame is laid out with, built once: a [`Layout`]
    /// owns a `Vec` of its constraints, and the split never changes.
    body: Layout,
    panes: Layout,
    /// The border every pane is drawn in. Untitled, because a block's title
    /// is a [`Line`] and rendering one allocates — the panes write their own
    /// over the border instead.
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
            menu: UiRenderMenuState::default(),
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

        // Last, because it goes over both panes. Centred rather than pinned
        // to the row it acts on: the title says which unit it is for.
        if self.menu.is_open() {
            let (width, height) = self.menu.size();
            frame.render_stateful_widget(
                UiRenderMenu::new(&self.border),
                centered(frame.area(), width, height),
                &mut self.menu,
            );
        }
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

    /// The rectangle the log pane wants.
    pub fn log_region(&self) -> LogRegion {
        self.log.region()
    }

    /// Pull the log scroll back to what the client actually found.
    pub fn clamp_log(&mut self, held: usize) {
        self.log.clamp(held);
    }

    /// Whether the menu has the keys.
    pub fn menu_open(&self) -> bool {
        self.menu.is_open()
    }

    /// Lend out the menu's entry buffer to be refilled.
    pub fn take_menu_items(&mut self) -> Vec<UnitChoice> {
        self.menu.take_items()
    }

    /// Open the menu over `key`, titled `title`, listing `items`.
    pub fn open_menu(&mut self, key: ArcStr, title: ArcStr, items: Vec<UnitChoice>) {
        self.menu.open(key, title, items);
    }

    /// Give the keys back to the list.
    pub fn close_menu(&mut self) {
        self.menu.close();
    }

    /// Move the menu cursor to the next entry that can be chosen.
    pub fn select_menu(&mut self, movement: Move) {
        self.menu.select(movement);
    }

    /// What <kbd>Enter</kbd> on the menu comes to.
    pub fn menu_choice(&self) -> UiMenuChoice {
        self.menu.chosen()
    }
}

impl Default for UiRender {
    fn default() -> Self {
        Self::new()
    }
}

/// The footer: a line put down span by span, because [`Line`]'s own
/// rendering allocates and this one never changes.
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

/// A box of `width` by `height` in the middle of `area`, clamped to it —
/// a popup hanging off the frame is a popup missing a border.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// How many columns are left between `x` and `right`.
fn room(x: u16, right: u16) -> usize {
    right.saturating_sub(x) as usize
}

/// Put `text` down in at most `room` columns, ending in an ellipsis if it had
/// to be cut, and report where it ended.
///
/// The buffer clips on its own but will not say that it clipped, and a name
/// that simply stops looks like a name spelled that way.
fn set_clipped(buffer: &mut Buffer, x: u16, y: u16, text: &str, room: usize, style: Style) -> u16 {
    if text.width() <= room {
        return buffer.set_stringn(x, y, text, room, style).0;
    }
    // One column goes to the mark, so anything narrower has room for the mark
    // and nothing else.
    let (end, _) = buffer.set_stringn(x, y, text, room.saturating_sub(1), style);
    buffer.set_stringn(end, y, "…", 1, style).0
}

/// The most digits a [`usize`] can have.
const DIGITS_MAX: usize = 20;

/// A number as decimal digits, written into a buffer the caller owns.
///
/// The alternative is a `format!` per row per frame. Returns the used tail of
/// `digits`, which is why it fills from the back.
fn decimal(value: usize, digits: &mut [u8; DIGITS_MAX]) -> &str {
    let mut start = DIGITS_MAX;
    let mut left = value;
    loop {
        start -= 1;
        digits[start] = b'0' + (left % 10) as u8;
        left /= 10;
        if left == 0 {
            break;
        }
    }
    // Every byte written is an ASCII digit.
    std::str::from_utf8(&digits[start..]).unwrap_or("?")
}

/// The bottom line: what the keys do. When a command can report having been
/// refused, this is the line it will have to share.
fn key_hints() -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        key_hint("↑↓", "move"),
        key_hint("⏎", "actions"),
        key_hint("r", "(re)start"),
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
