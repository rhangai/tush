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
        render::{CURSOR, DIGITS_MAX, decimal, room, set_clipped},
    },
};

/// Blank columns at each edge of a row, so the text is not against the
/// border and the selection bar has something to be a bar of.
const PAD_X: u16 = 1;

/// What separates the name from the column on the right.
const DETAIL_GAP: u16 = 2;

/// How little room the name may be left with before the right hand column is
/// dropped instead. A name is what the list is for; the mode is a reminder.
const NAME_MIN: u16 = 8;

/// One line each. The status is a mark in the gutter rather than a word under
/// the name, which is what let the row lose the other two lines.
const ROW_HEIGHT: usize = 1;

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

/// The units pane: one row per unit — a status mark, its name, and whatever
/// qualifies it.
///
/// The rows are written into the buffer rather than built as a `List` of
/// `ListItem`s of `Line`s of `Span`s, every part of which allocates. Caching
/// hid only half of that, since `List::render` allocates per line it draws
/// whatever the cache did.
pub struct UiRenderUnits<'a> {
    units: &'a [UiUnit],
    border: &'a Block<'a>,
}

impl<'a> UiRenderUnits<'a> {
    /// The pane for `units`, drawn inside `border` — lent rather than built
    /// here, a `Block` not being free to make.
    pub fn new(units: &'a [UiUnit], border: &'a Block<'a>) -> Self {
        Self { units, border }
    }
}

impl StatefulWidget for UiRenderUnits<'_> {
    type State = UiRenderUnitsState;

    fn render(self, area: Rect, buffer: &mut Buffer, state: &mut Self::State) {
        let inner = self.border.inner(area);
        self.border.render(area, buffer);

        let per_page = (inner.height as usize / ROW_HEIGHT).max(1);
        state.scroll_into_view(per_page, self.units.len());

        let last = self.units.len().min(state.offset + per_page);
        for (row, index) in (state.offset..last).enumerate() {
            let y = inner.y + (row * ROW_HEIGHT) as u16;
            draw_unit(
                buffer,
                &self.units[index],
                Rect::new(inner.x, y, inner.width, 1),
                index == state.cursor,
            );
        }
    }
}

/// One unit on one line: the status mark, the name, and the right hand
/// column — set down in that order because the name takes whatever the other
/// two leave.
///
/// [`CURSOR`] gets a column of its own ahead of the mark, so that being
/// selected changes nothing about how the rest of the row is drawn — the
/// status marks keep their colours on every row, selected included.
///
/// The short names are taken wherever the config wrote one. This row is the
/// narrowest thing on the screen, which is why the choice is made here and
/// why nothing below it takes it: the log pane's title has the room and
/// spells both out in full.
fn draw_unit(buffer: &mut Buffer, unit: &UiUnit, area: Rect, selected: bool) {
    let right = area.right().saturating_sub(PAD_X);
    let x = area.x + PAD_X;
    if selected {
        // White, so the one coloured thing in a row is still the status mark.
        let style = Style::new().fg(Color::White);
        buffer.set_stringn(x, area.y, CURSOR, CURSOR.width(), style);
    }

    let (mark, color) = status_mark(unit.state);
    let x = buffer
        .set_stringn(
            x + CURSOR.width() as u16 + 1,
            area.y,
            mark,
            mark.width(),
            Style::new().fg(color),
        )
        .0;
    let left = x + 1;

    let mut digits = [0u8; DIGITS_MAX];
    let (label, number, color) = detail(unit, &mut digits);
    let style = match color {
        Some(color) => Style::new().fg(color),
        None => Style::new().add_modifier(Modifier::DIM),
    };
    let width = (label.width() + number.width()) as u16;
    let mut name_right = right;
    if width > 0 {
        let start = right.saturating_sub(width);
        if start >= left + NAME_MIN {
            let x = buffer
                .set_stringn(start, area.y, label, room(start, right), style)
                .0;
            buffer.set_stringn(x, area.y, number, room(x, right), style);
            name_right = start.saturating_sub(DETAIL_GAP);
        }
    }
    // Bold as well as marked, because a mark two columns wide is a thin
    // thing to find a row by.
    let name = match selected {
        true => Style::new().add_modifier(Modifier::BOLD),
        false => Style::new(),
    };
    let text = unit.name_short.as_ref().unwrap_or(&unit.name);
    set_clipped(buffer, left, area.y, text, room(left, name_right), name);
}

/// The right hand column, in the two pieces it is written in: the most
/// specific thing there is to say about the unit — why it stopped, when it
/// stopped badly, and otherwise which mode it is on.
///
/// A failure with a code says `exit ` and leaves the number to [`decimal`],
/// which keeps every string here a literal. `None` for the colour is the
/// mode, which is said quietly.
fn detail<'a>(
    unit: &'a UiUnit,
    digits: &'a mut [u8; DIGITS_MAX],
) -> (&'a str, &'a str, Option<Color>) {
    match unit.state {
        RunnerState::ExitError(Some(code)) => (
            "exit ",
            decimal(code.get() as usize, digits),
            Some(Color::Red),
        ),
        RunnerState::ExitError(None) => ("failed", "", Some(Color::Red)),
        RunnerState::Killed(_) => ("killed", "", Some(Color::Magenta)),
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

/// The mark a state gets in the gutter, and the colour it gets it in.
///
/// Shape as well as colour, because a list read at a glance is read by shape
/// first and because a terminal's colours are the user's, not ours.
pub fn status_mark(state: RunnerState) -> (&'static str, Color) {
    match state {
        RunnerState::Stopped => ("○", Color::DarkGray),
        RunnerState::Waiting => ("◌", Color::Gray),
        RunnerState::Started => ("◐", Color::Yellow),
        RunnerState::Running => ("●", Color::Green),
        RunnerState::Killing => ("◑", Color::Yellow),
        RunnerState::ExitSuccess => ("✓", Color::Cyan),
        RunnerState::ExitError(_) => ("✗", Color::Red),
        RunnerState::Killed(_) => ("✗", Color::Magenta),
    }
}

/// What a state is called, for the one place there is room to spell it out.
///
/// The three terminal states stay apart rather than collapsing into
/// "stopped": whether a unit finished, failed or was killed is the first
/// thing you look at a list like this to find out.
pub fn status_label(state: RunnerState) -> (&'static str, Color) {
    match state {
        RunnerState::Stopped => ("stopped", Color::DarkGray),
        RunnerState::Waiting => ("waiting", Color::Gray),
        RunnerState::Started => ("starting", Color::Yellow),
        RunnerState::Running => ("running", Color::Green),
        RunnerState::Killing => ("stopping", Color::Yellow),
        RunnerState::ExitSuccess => ("done", Color::Cyan),
        RunnerState::ExitError(Some(_)) => ("exit ", Color::Red),
        RunnerState::ExitError(None) => ("failed", Color::Red),
        RunnerState::Killed(_) => ("killed", Color::Magenta),
    }
}
