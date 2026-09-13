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

/// How far the detail line sits in from the name above it, which is what
/// makes the two lines read as one row.
const DETAIL_INDENT: &str = "  ";

/// What separates a name from the mode it is running in.
const MODE_SEPARATOR: &str = " · ";

/// Its name, what qualifies it, and a gap. The gap belongs to the row: two
/// lines with nothing between them read as four rows rather than two.
const ROW_HEIGHT: usize = 3;

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

/// The units pane: one row per unit, two lines and a gap each.
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
                Rect::new(inner.x, y, inner.width, 2),
                index == state.cursor,
            );
        }
    }
}

/// One unit over the two lines of `area`: the name, then what qualifies it.
///
/// Two lines rather than one because on one the name and the status compete
/// for the same width, and in a narrow pane the name is what loses.
///
/// The mode shows even while stopped — it is the mode the menu will open its
/// cursor on, so the row is saying what the next start would run.
///
fn draw_unit(buffer: &mut Buffer, unit: &UiUnit, area: Rect, selected: bool) {
    // The selected row is bold throughout, so the style every piece of it is
    // written with starts from there rather than being applied after.
    let base = if selected {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let right = area.right();
    let left = area.x + CURSOR.width() as u16;

    if selected {
        buffer.set_stringn(area.x, area.y, CURSOR, CURSOR.width(), base);
    }
    set_clipped(buffer, left, area.y, &unit.name, room(left, right), base);

    let detail = area.y + 1;
    let mut x = buffer
        .set_stringn(left, detail, DETAIL_INDENT, DETAIL_INDENT.width(), base)
        .0;

    let (label, color) = status(unit.state);
    let status_style = base.fg(color);
    x = buffer
        .set_stringn(x, detail, label, room(x, right), status_style)
        .0;
    if let RunnerState::ExitError(Some(code)) = unit.state {
        let mut digits = [0u8; DIGITS_MAX];
        x = buffer
            .set_stringn(
                x,
                detail,
                decimal(code.get() as usize, &mut digits),
                room(x, right),
                status_style,
            )
            .0;
    }

    if let Some(mode) = unit.mode.as_deref() {
        let dim = base.add_modifier(Modifier::DIM);
        x = buffer
            .set_stringn(x, detail, MODE_SEPARATOR, room(x, right), dim)
            .0;
        set_clipped(buffer, x, detail, mode, room(x, right), dim);
    }
}

/// What a state is called on screen, and the colour it is called it in.
///
/// The three terminal states stay apart rather than collapsing into
/// "stopped": whether a unit finished, failed or was killed is the first
/// thing you look at a list like this to find out.
///
/// A failure with a code says `exit ` and leaves the number to [`decimal`],
/// which keeps every string here a literal.
fn status(state: RunnerState) -> (&'static str, Color) {
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
