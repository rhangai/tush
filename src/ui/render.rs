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
    layout::{Constraint, Layout, Position, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Widget},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    log::LogRegion,
    ui::theme::UiTheme,
    unit::{UnitChoice, UnitKey},
    util::str::SmallStr,
    view::ViewClient,
};

#[allow(unused_imports)]
pub use log::{UiRenderLog, UiRenderLogState};

#[allow(unused_imports)]
pub use menu::{UiMenuChoice, UiRenderMenu, UiRenderMenuState};

#[allow(unused_imports)]
pub use units::{Move, UiRenderUnits, UiRenderUnitsState, minor_start};

use crate::view::ViewUnit;

/// How wide the units column is.
///
/// Fixed rather than a share of the terminal, because what has to fit is a
/// name and a name does not grow when the window does. It also keeps the list
/// still: a name lands in the same place whatever else is on screen.
const UNITS_WIDTH: u16 = 30;

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
    /// The border the log pane and the menu are drawn in. Untitled, because
    /// a block's title is a [`Line`](ratatui::text::Line) and rendering one
    /// allocates — they write
    /// their own over the border instead.
    border: Block<'static>,
    /// The same, less the right hand side: that wall is the log pane's left
    /// one, and one line is drawn once.
    units_border: Block<'static>,
    /// Every fixed character and colour the panes draw with.
    ///
    /// Owned rather than borrowed, so that a screen is one thing to hold and
    /// a theme read from somewhere else is moved in at construction and not
    /// kept alive alongside it.
    theme: UiTheme,
}

impl UiRender {
    /// A screen showing the first unit, following the end of its log, drawn
    /// the way `theme` says.
    pub fn new(theme: UiTheme) -> Self {
        Self {
            units: UiRenderUnitsState::default(),
            log: UiRenderLogState::default(),
            menu: UiRenderMenuState::default(),
            areas: (Rect::ZERO, [Rect::ZERO; 3]),
            body: Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]),
            panes: Layout::horizontal([Constraint::Length(UNITS_WIDTH), Constraint::Fill(1)]),
            border: Block::bordered().border_set(theme.symbols.border),
            units_border: Block::new()
                .borders(Borders::TOP | Borders::BOTTOM | Borders::LEFT)
                .border_set(theme.symbols.border),
            theme,
        }
    }

    /// Draw one frame: the units on the left, their log on the right, and a
    /// line at the bottom.
    ///
    /// All this does is place the panes. What each of them draws is its own,
    /// and what it takes from outside is the client — only to read.
    pub fn draw<C: ViewClient>(&mut self, frame: &mut Frame, client: &C) {
        let [footer, units_area, log_area] = self.areas(frame.area());

        frame.render_widget(UiRenderHints(&self.theme), footer);
        frame.render_stateful_widget(
            UiRenderLog::new(
                self.selected_unit(client),
                client.log(),
                &self.border,
                &self.theme,
            ),
            log_area,
            &mut self.log,
        );
        frame.render_stateful_widget(
            UiRenderUnits::new(client.units(), &self.units_border, &self.theme),
            units_area,
            &mut self.units,
        );
        join_borders(frame.buffer_mut(), log_area, &self.theme);

        // Last, because it goes over both panes. Centred rather than pinned
        // to the row it acts on: the title says which unit it is for.
        if self.menu.is_open() {
            let (width, height) = self.menu.size(&self.theme);
            frame.render_stateful_widget(
                UiRenderMenu::new(&self.border, &self.theme),
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
    pub fn selected_unit<'a, C: ViewClient>(&self, client: &'a C) -> Option<&'a ViewUnit> {
        client.units().get(self.units.cursor())
    }

    /// Move the cursor, and put the log pane back at the end.
    ///
    /// The length and not the split: the arrows cross the divider, so where
    /// one list ends is not something moving by one has to know.
    pub fn select(&mut self, movement: Move, units: usize) {
        self.units.select(movement, units);
        self.log.follow();
    }

    /// Jump to the top of the other list, which is a different unit selected —
    /// so the log pane goes back to the end, as it does for any other move.
    ///
    /// The slice and not its length, because this is the one move that has to
    /// know where the divider is.
    pub fn focus_other(&mut self, units: &[ViewUnit]) {
        self.units.focus_other(minor_start(units), units.len());
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

    /// The unit the menu is open for, if it is open.
    pub fn menu_key(&self) -> Option<UnitKey> {
        self.menu.key()
    }

    /// Put the menu's entries back, re-read — see
    /// [`refresh`](UiRenderMenuState::refresh).
    pub fn refresh_menu(&mut self, items: Vec<UnitChoice>) {
        self.menu.refresh(items);
    }

    /// Lend out the menu's entry buffer to be refilled.
    pub fn take_menu_items(&mut self) -> Vec<UnitChoice> {
        self.menu.take_items()
    }

    /// Open the menu over `key`, titled `title`, listing `items`.
    pub fn open_menu(&mut self, key: UnitKey, title: SmallStr, items: Vec<UnitChoice>) {
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
        Self::new(UiTheme::default())
    }
}

/// The footer: what the keys do, written straight into the buffer like every
/// other pane.
///
/// It used to be a [`Line`](ratatui::text::Line) built once, which is what a
/// line that never
/// changes wants to be — until its words came from the theme, which the
/// screen owns alongside it and so cannot be borrowed from for a `'static`.
struct UiRenderHints<'a>(&'a UiTheme);

impl Widget for UiRenderHints<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let theme = self.0;
        let key = Style::new().fg(theme.colors.statusbar_key);
        let what = Style::new().add_modifier(Modifier::DIM);
        let mut x = area.x + 1;
        for (symbol, text) in hints(theme) {
            for (piece, style, gap) in [(symbol, key, " "), (text, what, "   ")] {
                let left = room(x, area.right());
                if left == 0 {
                    return;
                }
                x = buffer.set_stringn(x, area.y, piece, left, style).0;
                x = buffer
                    .set_stringn(x, area.y, gap, room(x, area.right()), style)
                    .0;
            }
        }
    }
}

/// The bindings the footer lists, in the order it lists them: the symbol that
/// names each key, and the text that says what it does.
///
/// The two halves come from different lists because they are different kinds
/// of thing — `⏎` is a character a terminal may not have, `actions` is a word.
fn hints(theme: &UiTheme) -> [(&SmallStr, &SmallStr); 8] {
    let (symbols, texts) = (&theme.symbols, &theme.texts);
    [
        (&symbols.statusbar_move, &texts.statusbar_move),
        (&symbols.statusbar_panel, &texts.statusbar_panel),
        (&symbols.statusbar_actions, &texts.statusbar_actions),
        (&symbols.statusbar_start, &texts.statusbar_start),
        (&symbols.statusbar_stop, &texts.statusbar_stop),
        (&symbols.statusbar_scroll, &texts.statusbar_scroll),
        (&symbols.statusbar_follow, &texts.statusbar_follow),
        (&symbols.statusbar_quit, &texts.statusbar_quit),
    ]
}

/// Turn the log pane's two left hand corners into the tees they are.
///
/// The panes share one wall rather than standing two of them next to each
/// other: a double rule down the middle of the screen is the first thing the
/// eye catches, and it means nothing. Drawn over the blocks afterwards,
/// there being no way to ask one for a tee.
fn join_borders(buffer: &mut Buffer, log: Rect, theme: &UiTheme) {
    let bottom = log.bottom().saturating_sub(1);
    let joins = [
        (log.y, &theme.symbols.join_top),
        (bottom, &theme.symbols.join_bottom),
    ];
    for (y, symbol) in joins {
        if let Some(cell) = buffer.cell_mut(Position::new(log.x, y)) {
            cell.set_symbol(symbol);
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

/// Put `text` down in at most `room` columns, ending in the theme's ellipsis
/// if it had to be cut, and report where it ended.
///
/// The buffer clips on its own but will not say that it clipped.
fn set_clipped(
    buffer: &mut Buffer,
    theme: &UiTheme,
    x: u16,
    y: u16,
    text: &str,
    room: usize,
    style: Style,
) -> u16 {
    if text.width() <= room {
        return buffer.set_stringn(x, y, text, room, style).0;
    }
    // The mark takes its own columns, so anything narrower has room for the
    // mark and nothing else.
    let mark = &theme.symbols.ellipsis;
    let width = mark.width();
    let (end, _) = buffer.set_stringn(x, y, text, room.saturating_sub(width), style);
    buffer.set_stringn(end, y, mark, width, style).0
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
