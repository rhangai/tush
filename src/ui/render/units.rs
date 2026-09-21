use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, StatefulWidget, Widget},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    config::ConfigPanel,
    runner::RunnerState,
    ui::{
        render::{DIGITS_MAX, decimal, room, set_clipped},
        theme::{UiTheme, UiThemeMenuLayout},
    },
    view::ViewUnit,
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

/// How much of the pane the minor list may take.
///
/// Half, because the list above it is the one you are here for: twenty setup
/// steps must not push the procs off the screen. What does not fit scrolls,
/// the minor list having its own window like the other one.
const MINOR_SHARE: u16 = 2;

/// Where a key press moves the cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Move {
    Next,
    Previous,
}

/// Which of the two lists the keys go to.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum UiRenderUnitsFocus {
    #[default]
    Main,
    Minor,
}

/// One list's place: the row the cursor is on, and where its window starts.
///
/// Two of these rather than one cursor over the whole slice, because the
/// lists scroll separately and one `offset` cannot serve two windows of
/// different heights.
#[derive(Default)]
struct UiRenderUnitsSection {
    cursor: usize,
    offset: usize,
}

impl UiRenderUnitsSection {
    /// Move the cursor, wrapping at either end of this list alone.
    ///
    /// Wrapping, so holding `j` comes round rather than parking on the last
    /// row. Within the list and not across it, which is what makes the two
    /// lists two: [`focus_other`](UiRenderUnitsState::focus_other) is the only
    /// way between them.
    fn select(&mut self, movement: Move, units: usize) {
        let Some(last) = units.checked_sub(1) else {
            self.cursor = 0;
            return;
        };
        // Clamped first: a list that shrank under a cursor leaves it past the
        // end until the next draw, and wrapping from there lands anywhere.
        let cursor = self.cursor.min(last);
        self.cursor = match movement {
            Move::Next if cursor == last => 0,
            Move::Next => cursor + 1,
            Move::Previous if cursor == 0 => last,
            Move::Previous => cursor - 1,
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

/// Where the units pane is looking from — the part that survives a frame.
#[derive(Default)]
pub struct UiRenderUnitsState {
    main: UiRenderUnitsSection,
    minor: UiRenderUnitsSection,
    /// Which list the keys go to.
    ///
    /// Held rather than worked out from the cursor, because each list
    /// remembers its own row: away and back lands where you were, which one
    /// index into the whole slice could not say.
    focused: UiRenderUnitsFocus,
}

impl UiRenderUnitsState {
    /// Which unit the cursor is on, as an index into the whole slice.
    ///
    /// `split` is where the minor list starts. Folding the two sections back
    /// into one index here is what keeps the menu, the log pane and `send`
    /// addressing one selected unit and knowing nothing about the split.
    pub fn cursor(&self, split: usize) -> usize {
        match self.focused {
            UiRenderUnitsFocus::Main => self.main.cursor,
            UiRenderUnitsFocus::Minor => split + self.minor.cursor,
        }
    }

    /// Move the cursor within whichever list has the keys.
    pub fn select(&mut self, movement: Move, split: usize, units: usize) {
        match self.focused {
            UiRenderUnitsFocus::Main => self.main.select(movement, split),
            UiRenderUnitsFocus::Minor => self.minor.select(movement, units - split),
        }
    }

    /// Put the keys on the other list.
    ///
    /// A no-op when the list it would move to is empty, so Tab in a session
    /// that declared no `panel: minor` does nothing rather than selecting a
    /// row that is not there.
    pub fn focus_other(&mut self, split: usize, units: usize) {
        self.focused = match self.focused {
            UiRenderUnitsFocus::Main if units > split => UiRenderUnitsFocus::Minor,
            UiRenderUnitsFocus::Minor if split > 0 => UiRenderUnitsFocus::Main,
            focused => focused,
        };
    }
}

/// Where the minor list starts in the units slice.
///
/// A partition point and not a scan, because the slice is ordered by panel —
/// which is what [`units`](crate::view::ViewClient::units) promises, and what
/// lets two lists be drawn out of one.
pub fn minor_start(units: &[ViewUnit]) -> usize {
    units.partition_point(|unit| unit.panel == ConfigPanel::Main)
}

/// The units pane: one row per unit, laid out the way the theme says.
///
/// The rows are written into the buffer rather than built as a `List` of
/// `ListItem`s of `Line`s of `Span`s, every part of which allocates. Caching
/// hid only half of that, since `List::render` allocates per line it draws
/// whatever the cache did.
pub struct UiRenderUnits<'a> {
    units: &'a [ViewUnit],
    border: &'a Block<'a>,
    theme: &'a UiTheme,
}

impl<'a> UiRenderUnits<'a> {
    /// The pane for `units`, drawn inside `border` — lent rather than built
    /// here, a `Block` not being free to make.
    pub fn new(units: &'a [ViewUnit], border: &'a Block<'a>, theme: &'a UiTheme) -> Self {
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

    /// The main list, the row the rule goes on, and the minor list.
    ///
    /// `None` when there is no minor list, or no room for one — and then the
    /// pane is exactly what it was before there were two, which is what every
    /// config that declares no `panel:` gets.
    ///
    /// The minor list is sized to what it holds, so it takes no room it does
    /// not need, and capped at [`MINOR_SHARE`] so it cannot take the pane.
    fn areas(&self, inner: Rect, minor: usize, main_row: u16) -> Option<(Rect, u16, Rect)> {
        if minor == 0 {
            return None;
        }
        // The rule, and one row of the list above it: a pane that cannot
        // afford both has nothing to divide.
        let room = inner.height.checked_sub(main_row + 1)?;
        let minor_height = (minor as u16).min(room).min(inner.height / MINOR_SHARE);
        if minor_height == 0 {
            return None;
        }
        let main_height = inner.height - minor_height - 1;
        let rule_y = inner.y + main_height;
        Some((
            Rect {
                height: main_height,
                ..inner
            },
            rule_y,
            Rect {
                y: rule_y + 1,
                height: minor_height,
                ..inner
            },
        ))
    }
}

impl StatefulWidget for UiRenderUnits<'_> {
    type State = UiRenderUnitsState;

    fn render(self, area: Rect, buffer: &mut Buffer, state: &mut Self::State) {
        let inner = self.border.inner(area);
        self.border.render(area, buffer);

        let split = minor_start(self.units);
        let (main, minor) = self.units.split_at(split);
        let layout = self.layout(inner.height);

        let Some((main_area, rule_y, minor_area)) =
            self.areas(inner, minor.len(), row_height(layout))
        else {
            // Nothing below, so nothing can have the keys but the list that is
            // there — a pane that loses its minor list must not keep pointing
            // at it.
            state.focused = UiRenderUnitsFocus::Main;
            draw_list(
                buffer,
                self.theme,
                main,
                inner,
                layout,
                &mut state.main,
                true,
            );
            return;
        };

        let focused = state.focused;
        draw_list(
            buffer,
            self.theme,
            main,
            main_area,
            layout,
            &mut state.main,
            focused == UiRenderUnitsFocus::Main,
        );
        draw_rule(buffer, self.theme, inner, rule_y);
        // Compact whatever the theme says: this is the list you are not
        // reading, and a row of it spent on a second line is a row the list
        // above does not get.
        draw_list(
            buffer,
            self.theme,
            minor,
            minor_area,
            UiThemeMenuLayout::Compact,
            &mut state.minor,
            focused == UiRenderUnitsFocus::Minor,
        );
    }
}

/// One list into its own area, at its own scroll.
fn draw_list(
    buffer: &mut Buffer,
    theme: &UiTheme,
    units: &[ViewUnit],
    area: Rect,
    layout: UiThemeMenuLayout,
    state: &mut UiRenderUnitsSection,
    focused: bool,
) {
    let height = row_height(layout);
    let per_page = (area.height / height).max(1) as usize;
    state.scroll_into_view(per_page, units.len());

    let last = units.len().min(state.offset + per_page);
    for (row, index) in (state.offset..last).enumerate() {
        let y = area.y + row as u16 * height;
        let row_area = Rect::new(area.x, y, area.width, height);
        let unit = &units[index];
        let selected = index == state.cursor;
        match layout {
            UiThemeMenuLayout::Compact => {
                draw_compact(buffer, theme, unit, row_area, selected, focused)
            }
            UiThemeMenuLayout::Comfortable => {
                draw_comfortable(buffer, theme, unit, row_area, selected, focused)
            }
        }
    }
}

/// The line between the two lists.
///
/// Inset from the pane's own border rather than tee'd into it: a tee wants a
/// glyph no `border::Set` carries, and a rule that stops short reads as one
/// list divided rather than as a second box.
fn draw_rule(buffer: &mut Buffer, theme: &UiTheme, inner: Rect, y: u16) {
    let rule = theme.symbols.border.horizontal_top;
    let style = Style::new().add_modifier(Modifier::DIM);
    for x in (inner.x + PAD_X)..inner.right().saturating_sub(PAD_X) {
        if let Some(cell) = buffer.cell_mut(Position::new(x, y)) {
            cell.set_symbol(rule).set_style(style);
        }
    }
}

/// The cursor column and the status mark, which both layouts open with, and
/// where the name may start after them.
fn draw_gutter(
    buffer: &mut Buffer,
    theme: &UiTheme,
    unit: &ViewUnit,
    area: Rect,
    selected: bool,
    focused: bool,
) -> u16 {
    let cursor = &theme.symbols.cursor;
    let x = area.x + PAD_X;
    if selected {
        // Dim in the list that does not have the keys: the row Tab comes back
        // to is worth seeing, and worth not mistaking for the live one.
        let style = match focused {
            true => Style::new().fg(theme.colors.cursor),
            false => Style::new().add_modifier(Modifier::DIM),
        };
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
fn draw_compact(
    buffer: &mut Buffer,
    theme: &UiTheme,
    unit: &ViewUnit,
    area: Rect,
    selected: bool,
    focused: bool,
) {
    let left = draw_gutter(buffer, theme, unit, area, selected, focused);
    let right = area.right().saturating_sub(PAD_X);

    let mut digits = [0u8; DIGITS_MAX];
    let (label, number, color) = detail(theme, unit, &mut digits);
    let style = match color {
        Some(color) => Style::new().fg(color),
        None => Style::new().add_modifier(Modifier::DIM),
    };
    let name_right = set_right(buffer, area.y, left, right, label, number, style);

    let name = unit.name_short.as_ref().unwrap_or(&unit.name);
    let style = name_style(selected && focused);
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
    unit: &ViewUnit,
    area: Rect,
    selected: bool,
    focused: bool,
) {
    let left = draw_gutter(buffer, theme, unit, area, selected, focused);
    let right = area.right().saturating_sub(PAD_X);
    set_clipped(
        buffer,
        theme,
        left,
        area.y,
        &unit.name,
        room(left, right),
        name_style(selected && focused),
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
    unit: &'a ViewUnit,
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
