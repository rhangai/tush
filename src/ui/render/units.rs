use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, StatefulWidget, Widget},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    runner::RunnerState,
    ui::{
        client::UiUnit,
        render::{DIGITS_MAX, decimal, room, set_clipped},
        theme::{UiTheme, UiThemeMenuLayout},
    },
};

/// Blank columns at each edge of a row, so the text is not against the
/// border and the gutter has somewhere to start.
const PAD_X: u16 = 1;

/// What separates the name from the column on the right, in a compact row.
const DETAIL_GAP: u16 = 2;

/// How little room the name may be left with before the right hand column is
/// dropped instead. A name is what the list is for; the mode is a reminder.
const NAME_MIN: u16 = 8;

/// How many lines a row of each layout takes.
///
/// A comfortable row is two lines and the gap after them: two lines with
/// nothing between them read as four rows rather than two. The theme picks
/// between the layouts; what a layout costs in lines is this pane's to know.
const COMPACT_HEIGHT: u16 = 1;
const COMFORTABLE_HEIGHT: u16 = 3;

/// How many lines `layout` spends on one unit.
fn row_height(layout: UiThemeMenuLayout) -> u16 {
    match layout {
        UiThemeMenuLayout::Compact => COMPACT_HEIGHT,
        UiThemeMenuLayout::Comfortable => COMFORTABLE_HEIGHT,
    }
}

/// Where a key press moves the cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Move {
    Next,
    Previous,
}

/// Where the units pane is looking from — the part that survives a frame.
#[derive(Default)]
pub struct UiRenderUnitsState {
    /// Which unit the cursor is on.
    cursor: usize,
    /// The first unit drawn, which scrolling moves.
    offset: usize,
}

impl UiRenderUnitsState {
    /// Which unit the cursor is on.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Move the cursor, bounded by however many units there are.
    pub fn select(&mut self, movement: Move, units: usize) {
        let last = units.saturating_sub(1);
        self.cursor = match movement {
            Move::Next => self.cursor.saturating_add(1).min(last),
            Move::Previous => self.cursor.saturating_sub(1),
        };
    }

    /// Keep the cursor on screen, and the screen on the units.
    fn scroll_into_view(&mut self, per_page: usize, units: usize) {
        self.cursor = self.cursor.min(units.saturating_sub(1));
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + per_page {
            self.offset = self.cursor + 1 - per_page;
        }
        // Never so far down that rows go begging at the bottom while there
        // are units above that could have filled them.
        self.offset = self.offset.min(units.saturating_sub(per_page));
    }
}

/// The units pane: one row per unit, laid out the way the theme says.
///
/// The rows are written into the buffer rather than built as a `List` of
/// `ListItem`s of `Line`s of `Span`s, every part of which allocates. Caching
/// hid only half of that, since `List::render` allocates per line it draws
/// whatever the cache did.
pub struct UiRenderUnits<'a> {
    units: &'a [UiUnit],
    border: &'a Block<'a>,
    theme: &'a UiTheme,
}

impl<'a> UiRenderUnits<'a> {
    /// The pane for `units`, drawn inside `border` — lent rather than built
    /// here, a `Block` not being free to make.
    pub fn new(units: &'a [UiUnit], border: &'a Block<'a>, theme: &'a UiTheme) -> Self {
        Self {
            units,
            border,
            theme,
        }
    }

    /// Which layout to actually use, which is the theme's unless the pane is
    /// too short for it.
    ///
    /// The fallback is one way: a comfortable row wants three lines, and a pane that
    /// cannot give one row all three would show a list of one unit. Compact
    /// asks for one line and so never has to fall back to anything.
    fn layout(&self, height: u16) -> UiThemeMenuLayout {
        let wanted = self.theme.menu_layout;
        match height >= row_height(wanted) {
            true => wanted,
            false => UiThemeMenuLayout::Compact,
        }
    }
}

impl StatefulWidget for UiRenderUnits<'_> {
    type State = UiRenderUnitsState;

    fn render(self, area: Rect, buffer: &mut Buffer, state: &mut Self::State) {
        let inner = self.border.inner(area);
        self.border.render(area, buffer);

        let layout = self.layout(inner.height);
        let height = row_height(layout);
        let per_page = (inner.height / height).max(1) as usize;
        state.scroll_into_view(per_page, self.units.len());

        let last = self.units.len().min(state.offset + per_page);
        for (row, index) in (state.offset..last).enumerate() {
            let y = inner.y + row as u16 * height;
            let area = Rect::new(inner.x, y, inner.width, height);
            let unit = &self.units[index];
            let selected = index == state.cursor;
            match layout {
                UiThemeMenuLayout::Compact => {
                    draw_compact(buffer, self.theme, unit, area, selected)
                }
                UiThemeMenuLayout::Comfortable => {
                    draw_comfortable(buffer, self.theme, unit, area, selected)
                }
            }
        }
    }
}

/// The cursor column and the status mark, which both layouts open with, and
/// where the name may start after them.
fn draw_gutter(
    buffer: &mut Buffer,
    theme: &UiTheme,
    unit: &UiUnit,
    area: Rect,
    selected: bool,
) -> u16 {
    let cursor = &theme.symbols.cursor;
    let x = area.x + PAD_X;
    if selected {
        let style = Style::new().fg(theme.colors.cursor);
        buffer.set_stringn(x, area.y, cursor, cursor.width(), style);
    }

    let mark = theme.symbols.status(unit.state);
    let x = buffer
        .set_stringn(
            x + cursor.width() as u16 + 1,
            area.y,
            mark,
            mark.width(),
            Style::new().fg(theme.colors.status(unit.state)),
        )
        .0;
    x + 1
}

/// Bold as well as marked, because a cursor two columns wide is a thin thing
/// to find a row by.
fn name_style(selected: bool) -> Style {
    match selected {
        true => Style::new().add_modifier(Modifier::BOLD),
        false => Style::new(),
    }
}

/// Put `first` and `second` against the right edge of row `y`, so long as
/// doing so still leaves [`NAME_MIN`] columns for the text coming from the
/// left, and report where that text now has to stop.
///
/// Two pieces because the one case that needs two is a failure with a code,
/// whose number is written straight out of a digit buffer rather than joined
/// onto the label.
fn set_right(
    buffer: &mut Buffer,
    y: u16,
    left: u16,
    right: u16,
    first: &str,
    second: &str,
    style: Style,
) -> u16 {
    let width = (first.width() + second.width()) as u16;
    if width == 0 {
        return right;
    }
    let start = right.saturating_sub(width);
    if start < left + NAME_MIN {
        return right;
    }
    let x = buffer
        .set_stringn(start, y, first, room(start, right), style)
        .0;
    buffer.set_stringn(x, y, second, room(x, right), style);
    start.saturating_sub(DETAIL_GAP)
}

/// One unit on one line: the status mark, the name, and the right hand
/// column — set down in that order because the name takes whatever the other
/// two leave.
///
/// The short names are taken wherever the config wrote one. This is the
/// narrowest row on the screen and the one they were asked for; the other
/// layout and the log pane's title spell everything out instead.
fn draw_compact(buffer: &mut Buffer, theme: &UiTheme, unit: &UiUnit, area: Rect, selected: bool) {
    let left = draw_gutter(buffer, theme, unit, area, selected);
    let right = area.right().saturating_sub(PAD_X);

    let mut digits = [0u8; DIGITS_MAX];
    let (label, number, color) = detail(theme, unit, &mut digits);
    let style = match color {
        Some(color) => Style::new().fg(color),
        None => Style::new().add_modifier(Modifier::DIM),
    };
    let name_right = set_right(buffer, area.y, left, right, label, number, style);

    let name = unit.name_short.as_ref().unwrap_or(&unit.name);
    let style = name_style(selected);
    set_clipped(
        buffer,
        theme,
        left,
        area.y,
        name,
        room(left, name_right),
        style,
    );
}

/// One unit over two lines and a gap: the name on its own, then its state
/// under it.
///
/// Both lines are anchored, which is what makes a column of these read as a
/// list rather than as text of ragged lengths: the name has the whole width
/// of the first line, and the second is the status word at the name's own
/// column with the mode against the right edge. Every row is those same two
/// marks in those same two places, whatever it has to say between them.
///
/// Full names and full modes. Two lines is the layout you pick when you would
/// rather read the list than fit it, so it takes the long form of everything
/// the config gave a short one for.
fn draw_comfortable(
    buffer: &mut Buffer,
    theme: &UiTheme,
    unit: &UiUnit,
    area: Rect,
    selected: bool,
) {
    let left = draw_gutter(buffer, theme, unit, area, selected);
    let right = area.right().saturating_sub(PAD_X);
    set_clipped(
        buffer,
        theme,
        left,
        area.y,
        &unit.name,
        room(left, right),
        name_style(selected),
    );

    // The gutter stays empty on the second line, so the two read as one row.
    let y = area.y + 1;
    let dim = Style::new().add_modifier(Modifier::DIM);
    let mode = unit.mode.as_deref().unwrap_or("");
    let status_right = set_right(buffer, y, left, right, mode, "", dim);

    let style = Style::new().fg(theme.colors.status(unit.state));
    let label = theme.texts.status(unit.state);
    let x = buffer
        .set_stringn(left, y, label, room(left, status_right), style)
        .0;
    if let RunnerState::ExitError(Some(code)) = unit.state {
        let mut digits = [0u8; DIGITS_MAX];
        let number = decimal(code.get() as usize, &mut digits);
        buffer.set_stringn(x, y, number, room(x, status_right), style);
    }
}

/// A compact row's right hand column, in the two pieces it is written in: the
/// most specific thing there is to say about the unit — why it stopped, when
/// it stopped badly, and otherwise which mode it is on.
///
/// A failure with a code leaves the number to [`decimal`], the theme's label
/// for it being a prefix, which keeps this from building a string.
/// `None` for the colour is the mode, which is said quietly.
fn detail<'a>(
    theme: &'a UiTheme,
    unit: &'a UiUnit,
    digits: &'a mut [u8; DIGITS_MAX],
) -> (&'a str, &'a str, Option<Color>) {
    let label = theme.texts.status(unit.state);
    let color = theme.colors.status(unit.state);
    match unit.state {
        RunnerState::ExitError(Some(code)) => (
            label.as_str(),
            decimal(code.get() as usize, digits),
            Some(color),
        ),
        RunnerState::ExitError(None) | RunnerState::Killed(_) => (label.as_str(), "", Some(color)),
        _ => (
            unit.mode_short
                .as_deref()
                .or(unit.mode.as_deref())
                .unwrap_or(""),
            "",
            None,
        ),
    }
}
