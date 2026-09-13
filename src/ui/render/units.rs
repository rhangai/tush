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
        render::{room, set_clipped},
    },
};

/// How far the detail line sits in from the name above it.
///
/// The indent is what makes two lines read as one row: the name is the
/// heading and everything under it hangs off it, so the eye finds the names
/// by running down the left edge and never has to separate them from what
/// qualifies them.
const DETAIL_INDENT: &str = "  ";

/// What separates a name from the mode it is running in.
const MODE_SEPARATOR: &str = " · ";

/// The mark on the selected row, and the column it lives in.
///
/// The trailing space is part of it: the mark needs to not touch the name,
/// and every row is indented by however wide this is, selected or not, so the
/// names stay in one column as the cursor moves over them.
const CURSOR: &str = "> ";

/// How many rows one unit takes: its name, what qualifies it, and a gap.
///
/// The gap is part of the row rather than something between rows, because two
/// lines with nothing between them read as four rows rather than two.
const ROW_HEIGHT: usize = 3;

/// Where a key press moves the cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Move {
    Next,
    Previous,
    First,
    Last,
}

/// Where the units pane is looking from.
///
/// Its own type because it is what survives a frame: the pane itself is built
/// and thrown away every time, and this is the part that has to still be
/// there next time. Which is what [`StatefulWidget`] is for.
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
            Move::First => 0,
            Move::Last => last,
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
/// # Why the rows are written rather than built
///
/// This was a `List` of `ListItem`s of `Line`s of `Span`s, and every part of
/// that allocates — on a screen nobody is touching, several hundred times a
/// second to say what it said last time. Caching the widget only hid half of
/// it, because `List::render` allocates per line it draws whatever the cache
/// did.
///
/// A row is a name, a status in its colour, and a mode that qualifies it:
/// three runs of text at known positions. Written straight into the buffer it
/// is three calls and no allocation, and the widget was not doing anything
/// else.
pub struct UiRenderUnits<'a> {
    units: &'a [UiUnit],
    border: &'a Block<'a>,
}

impl<'a> UiRenderUnits<'a> {
    /// The pane for `units`, drawn inside `border`.
    ///
    /// The border is lent rather than made here because a `Block` is not free
    /// to build and this one never changes.
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

/// One unit, over the two lines of `area`: the name, and under it everything
/// that qualifies it.
///
/// # Why two lines and not one
///
/// Because on one line the name and the status compete for the same width,
/// and the loser is whichever the pane is too narrow for. Given a line of its
/// own the name gets the whole column and is cut only when it genuinely does
/// not fit, while the detail line underneath has room for as many facets as
/// turn up without ever being the reason a name lost its tail.
///
/// # The detail line
///
/// The status first, in its colour, because it is the word the row exists to
/// tell you. Then the mode, dimmed, for a unit that has modes — the one thing
/// about a running proc that its name and its state do not already say, since
/// two units can both be `running` and be doing entirely different work.
///
/// # Two reasons there is no mode to show
///
/// A unit with no modes at all, which is most of them: its
/// [`mode`](UiUnit::mode) is `None` and nothing is invented for it.
///
/// And a unit that is [`Stopped`](RunnerState::Stopped), which is the one
/// state where no run exists — not one that failed or was killed, but none at
/// all. The behavior is still sitting on a mode and will start in it, but a
/// mode is something a run is in, so a stopped row saying `stopped · Build`
/// claims work that is not happening. The terminal states are the other way
/// round: `exit 2 · Build` is which mode it was that failed, and that is
/// worth keeping.
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
        let mut digits = [0u8; 3];
        x = buffer
            .set_stringn(
                x,
                detail,
                decimal(code.get(), &mut digits),
                room(x, right),
                status_style,
            )
            .0;
    }

    let mode = match unit.state {
        RunnerState::Stopped => None,
        _ => unit.mode.as_deref(),
    };
    if let Some(mode) = mode {
        let dim = base.add_modifier(Modifier::DIM);
        x = buffer
            .set_stringn(x, detail, MODE_SEPARATOR, room(x, right), dim)
            .0;
        set_clipped(buffer, x, detail, mode, room(x, right), dim);
    }
}

/// What a state is called on screen, and the colour it is called it in.
///
/// The three terminal states are kept apart rather than collapsed into
/// "stopped": whether a unit finished, failed, or was killed by the user is
/// the first thing you look at a list like this to find out.
///
/// A failure with a code says `exit ` and leaves the number to [`decimal`],
/// which is what keeps every one of these a literal and this whole function
/// free of allocation.
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

/// A byte as decimal digits, written into a buffer the caller owns.
///
/// Three digits is every `u8` there is. Done this way because the alternative
/// is a `format!` on a path that runs once per failed unit per frame, to say
/// a number that has not changed since the process exited.
fn decimal(value: u8, digits: &mut [u8; 3]) -> &str {
    let mut len = 0;
    if value >= 100 {
        digits[len] = b'0' + value / 100;
        len += 1;
    }
    if value >= 10 {
        digits[len] = b'0' + (value / 10) % 10;
        len += 1;
    }
    digits[len] = b'0' + value % 10;
    len += 1;
    // Every byte written is an ASCII digit.
    std::str::from_utf8(&digits[..len]).unwrap_or("?")
}
