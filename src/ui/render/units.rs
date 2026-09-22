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

/// Where the units pane is looking from — the part that survives a frame.
///
/// One cursor and two offsets, which is the whole of what the split costs:
/// the lists are drawn from one slice ordered by panel, so a row is one index
/// into it and which list has the keys is simply where that index fell. What
/// cannot be shared is the scroll, two windows of different heights having
/// nothing to say to each other.
#[derive(Default)]
pub struct UiRenderUnitsState {
    /// The row the cursor is on, as an index into the whole slice.
    cursor: usize,
    /// Where each list's window starts.
    main_offset: usize,
    minor_offset: usize,
}

impl UiRenderUnitsState {
    /// Which unit the cursor is on.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Move one row, crossing the divider and coming round at the ends.
    ///
    /// The divider is a division of one list and not a wall: the arrows walk
    /// every unit on screen in the order they are drawn, so there is never a
    /// row you can see and cannot reach.
    pub fn select(&mut self, movement: Move, units: usize) {
        let Some(last) = units.checked_sub(1) else {
            return;
        };
        // Clamped: a list that shrank under the cursor leaves it past the end
        // until the next draw, and stepping from there lands anywhere.
        let cursor = self.cursor.min(last);
        self.cursor = match movement {
            Move::Next if cursor == last => 0,
            Move::Next => cursor + 1,
            Move::Previous if cursor == 0 => last,
            Move::Previous => cursor - 1,
        };
    }

    /// Jump to the top of the other list.
    ///
    /// The top and not the row last left there: the arrows already walk both
    /// lists, so this is the shortcut past a long one, and a shortcut that
    /// lands somewhere you have to look for is not one.
    ///
    /// A no-op when the list it would jump to is empty, so Tab in a session
    /// that declared no `panel: minor` does nothing rather than selecting a
    /// row that is not there.
    pub fn focus_other(&mut self, split: usize, units: usize) {
        self.cursor = match self.cursor < split {
            true if units > split => split,
            false if split > 0 => 0,
            _ => self.cursor,
        };
    }
}

/// Keep `offset` on a window that holds `cursor`, and on the units.
///
/// `None` is the list without the cursor: it still has to be pulled back onto
/// what there is, or a list that shrank leaves a window past its end.
fn scroll_into_view(offset: &mut usize, cursor: Option<usize>, per_page: usize, units: usize) {
    if let Some(cursor) = cursor {
        if cursor < *offset {
            *offset = cursor;
        } else if cursor >= *offset + per_page {
            *offset = cursor + 1 - per_page;
        }
    }
    // Never so far down that rows go begging at the bottom while there are
    // units above that could have filled them.
    *offset = (*offset).min(units.saturating_sub(per_page));
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

        state.cursor = state.cursor.min(self.units.len().saturating_sub(1));
        let split = minor_start(self.units);
        let (main, minor) = self.units.split_at(split);
        let layout = self.layout(inner.height);

        // The cursor belongs to exactly one list, which is what says which one
        // has the keys — there is no second mark to tell apart from it.
        let main_cursor = (state.cursor < split).then_some(state.cursor);
        let minor_cursor = (state.cursor >= split).then(|| state.cursor - split);

        // A rule divides two lists, so one of them being empty leaves nothing
        // to divide — an all-`minor` config is one list, drawn quietly.
        let divided = !main.is_empty() && !minor.is_empty();
        let Some((main_area, rule_y, minor_area)) = divided
            .then(|| self.areas(inner, minor.len(), row_height(layout)))
            .flatten()
        else {
            // One list, either because that is all there is or because the
            // pane cannot afford the rule. Which one falls out of where the
            // units are — a blank pane is what broken looks like.
            let (rows, offset, cursor, quiet) = match main.is_empty() {
                true => (minor, &mut state.minor_offset, minor_cursor, true),
                false => (main, &mut state.main_offset, main_cursor, false),
            };
            draw_list(
                buffer, self.theme, rows, inner, layout, offset, cursor, quiet,
            );
            return;
        };

        draw_list(
            buffer,
            self.theme,
            main,
            main_area,
            layout,
            &mut state.main_offset,
            main_cursor,
            false,
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
            &mut state.minor_offset,
            minor_cursor,
            true,
        );
    }
}

/// One list into its own area, at its own scroll.
///
/// `cursor` is `None` for the list the keys are not in, which is what makes
/// the mark appear exactly once on the screen.
#[allow(clippy::too_many_arguments)]
fn draw_list(
    buffer: &mut Buffer,
    theme: &UiTheme,
    units: &[ViewUnit],
    area: Rect,
    layout: UiThemeMenuLayout,
    offset: &mut usize,
    cursor: Option<usize>,
    quiet: bool,
) {
    let height = row_height(layout);
    let per_page = (area.height / height).max(1) as usize;
    scroll_into_view(offset, cursor, per_page, units.len());

    let last = units.len().min(*offset + per_page);
    for (row, index) in (*offset..last).enumerate() {
        let y = area.y + row as u16 * height;
        let row_area = Rect::new(area.x, y, area.width, height);
        let unit = &units[index];
        let selected = cursor == Some(index);
        match layout {
            UiThemeMenuLayout::Compact => {
                draw_compact(buffer, theme, unit, row_area, selected, quiet)
            }
            UiThemeMenuLayout::Comfortable => {
                draw_comfortable(buffer, theme, unit, row_area, selected, quiet)
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

/// Bold under the live cursor, quiet throughout the minor list, plain
/// otherwise.
///
/// The minor list is written quietly by standing and not by focus: it is the
/// list you said you would not be watching, and a block of dim names under a
/// rule reads as subordinate before you have read a word of it. That is what
/// stops the two lists looking like peers, which is what made a single cursor
/// carry the whole job of saying which one was live.
///
/// Focus itself is the cursor being there or not — never this — so a terminal
/// that ignores `DIM` still says which list has the keys. And the row under
/// the cursor goes bold whichever list it is in, so it comes out of the quiet
/// block rather than being lost in it.
///
/// Bold as well as marked, because a cursor two columns wide is a thin thing
/// to find a row by.
fn name_style(live: bool, quiet: bool) -> Style {
    if live {
        return Style::new().add_modifier(Modifier::BOLD);
    }
    match quiet {
        true => Style::new().add_modifier(Modifier::DIM),
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
    quiet: bool,
) {
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
    let style = name_style(selected, quiet);
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
    quiet: bool,
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
        name_style(selected, quiet),
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
