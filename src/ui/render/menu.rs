use arcstr::ArcStr;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Clear, StatefulWidget, Widget},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    ui::render::{CURSOR, Move, room, set_clipped},
    unit::{UnitChoice, UnitEvent},
};

/// What separates a verb from the mode it applies to.
const VERB_GAP: &str = " ";

/// Clear space inside the border, so the box does not read as cramped.
const PAD_X: u16 = 2;
const PAD_Y: u16 = 1;

/// The narrowest the menu may be, so that every one of them is the same
/// object rather than a box shrunk to fit the word `Stop`.
const MIN_WIDTH: u16 = 28;

/// The way out. Here and not in a behavior, because dismissing a popup asks
/// nothing of a process — an event for it would be one the unit layer has to
/// carry and ignore.
const CANCEL: &str = "cancel";

/// A blank row before the way out, so it is not what you hit aiming at
/// `Stop`.
const CANCEL_GAP: u16 = 1;

/// What the menu is showing, and which row the cursor is on.
///
/// The entries are read out of the session once, when it opens, and held
/// still here: rebuilt per frame they would renumber themselves under the
/// cursor the moment a process exited.
#[derive(Default)]
pub struct UiRenderMenuState {
    /// The unit it is open for, `None` when closed. Kept rather than re-read
    /// from the selection, which a resize can move underneath it.
    key: Option<ArcStr>,
    /// What that unit is called, for the title.
    title: ArcStr,
    /// The entries, kept across a close so the next open refills them.
    items: Vec<UnitChoice>,
    /// Which row: an entry, or [`CANCEL`] at `items.len()`. Only ever one
    /// that can be chosen.
    cursor: usize,
}

impl UiRenderMenuState {
    /// Whether the menu has the keys.
    pub fn is_open(&self) -> bool {
        self.key.is_some()
    }

    /// Lend out the entry buffer to be refilled, keeping its capacity.
    ///
    /// By value and not through a `&mut`, because the caller hands it to the
    /// client — a borrow of the screen held across a read of the session is
    /// the one shape the borrow checker will not have.
    pub fn take_items(&mut self) -> Vec<UnitChoice> {
        std::mem::take(&mut self.items)
    }

    /// Open it over `key`, titled `title`, listing `items`.
    ///
    /// The cursor starts on the mode the unit is already on, so two presses
    /// run what the row was offering; failing that the first enabled entry,
    /// failing that [`CANCEL`]. Between them it never lands on a disabled
    /// row, so <kbd>Enter</kbd> is never a press that does nothing.
    pub fn open(&mut self, key: ArcStr, title: ArcStr, items: Vec<UnitChoice>) {
        self.cursor = items
            .iter()
            .position(|item| item.current && item.enabled)
            .or_else(|| items.iter().position(|item| item.enabled))
            .unwrap_or(items.len());
        self.items = items;
        self.title = title;
        self.key = Some(key);
    }

    /// Give the keys back, keeping the buffer.
    pub fn close(&mut self) {
        self.key = None;
    }

    /// Move to the next row that can be chosen, stepping over the disabled
    /// ones. Neither end wraps, as in the units list.
    pub fn select(&mut self, movement: Move) {
        let cancel = self.items.len();
        let mut at = self.cursor;
        loop {
            at = match movement {
                Move::Next if at < cancel => at + 1,
                Move::Previous if at > 0 => at - 1,
                _ => return,
            };
            if at == cancel || self.items[at].enabled {
                self.cursor = at;
                return;
            }
        }
    }

    /// What <kbd>Enter</kbd> means where the cursor is.
    ///
    /// The disabled arm is unreachable rather than tolerated — [`open`] and
    /// [`select`](Self::select) keep the cursor off those rows — and answers
    /// `Cancel` so that a press can never leave the menu just sitting there.
    ///
    /// [`open`]: Self::open
    pub fn chosen(&self) -> UiMenuChoice {
        let Some(key) = self.key.as_ref() else {
            return UiMenuChoice::Cancel;
        };
        match self.items.get(self.cursor) {
            Some(item) if item.enabled => UiMenuChoice::Send {
                key: key.clone(),
                event: item.event,
            },
            _ => UiMenuChoice::Cancel,
        }
    }

    /// Wide enough for the longest entry or the title, tall enough for all
    /// of them, plus padding and border.
    pub fn size(&self) -> (u16, u16) {
        let entries = self
            .items
            .iter()
            .map(|item| {
                item.verb.width()
                    + item
                        .mode
                        .as_deref()
                        .map_or(0, |mode| VERB_GAP.width() + mode.width())
            })
            .max()
            .unwrap_or(0);
        let width = (entries.max(CANCEL.width()) + CURSOR.width()) as u16 + PAD_X * 2;
        // The title sits in the top border with a space either side, and the
        // two corners are not room for anything.
        let width = width.max(self.title.width() as u16 + 2).max(MIN_WIDTH) + 2;
        let rows = self.items.len() as u16 + CANCEL_GAP + 1;
        (width, rows + PAD_Y * 2 + 2)
    }
}

/// What <kbd>Enter</kbd> on the menu comes to.
///
/// Not an `Option`, because closing is an answer and not the absence of one.
pub enum UiMenuChoice {
    /// Send this to the unit, and close.
    Send { key: ArcStr, event: UnitEvent },
    /// Close, and ask for nothing.
    Cancel,
}

/// The action menu: what can be asked of one unit, over the top of the rest.
///
/// The entries are not built here. The behavior wrote them, being the only
/// thing that knows which modes exist and which can be chosen from where the
/// run is; working that out from a name and a state would be a second copy of
/// those rules in the module least able to check them.
pub struct UiRenderMenu<'a> {
    border: &'a Block<'a>,
}

impl<'a> UiRenderMenu<'a> {
    /// The menu, drawn inside `border`.
    pub fn new(border: &'a Block<'a>) -> Self {
        Self { border }
    }
}

impl StatefulWidget for UiRenderMenu<'_> {
    type State = UiRenderMenuState;

    fn render(self, area: Rect, buffer: &mut Buffer, state: &mut Self::State) {
        if !state.is_open() {
            return;
        }
        // What is underneath is still drawn, and shows through otherwise.
        Clear.render(area, buffer);
        let inner = self.border.inner(area);
        self.border.render(area, buffer);

        // Over the top border in pieces, as the log pane does it: a block's
        // title is a `Line`, and both building and rendering one allocate.
        let plain = Style::new();
        let mut x = buffer.set_stringn(inner.x, area.y, " ", 1, plain).0;
        x = set_clipped(
            buffer,
            x,
            area.y,
            &state.title,
            room(x, inner.right()),
            plain,
        );
        buffer.set_stringn(x, area.y, " ", 1, plain);

        let entries = Rect {
            x: inner.x + PAD_X,
            y: inner.y + PAD_Y,
            width: inner.width.saturating_sub(PAD_X * 2),
            height: inner.height.saturating_sub(PAD_Y * 2),
        };
        for (row, item) in state.items.iter().take(entries.height as usize).enumerate() {
            draw_choice(
                buffer,
                item,
                Rect::new(entries.x, entries.y + row as u16, entries.width, 1),
                row == state.cursor,
            );
        }

        // Past the gap, on the row the cursor calls `items.len()`.
        let row = state.items.len() as u16 + CANCEL_GAP;
        if row < entries.height {
            draw_row(
                buffer,
                CANCEL,
                None,
                Rect::new(entries.x, entries.y + row, entries.width, 1),
                Style::new(),
                state.cursor == state.items.len(),
            );
        }
    }
}

/// One entry: the mark, the verb, and the mode it applies to.
fn draw_choice(buffer: &mut Buffer, item: &UnitChoice, area: Rect, selected: bool) {
    let style = match item.enabled {
        true => Style::new(),
        false => Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
    };
    draw_row(
        buffer,
        item.verb,
        item.mode.as_deref(),
        area,
        style,
        selected,
    );
}

/// A verb, and what it applies to, in the cursor column.
///
/// Shared with [`CANCEL`], which is no [`UnitChoice`] but has to line up with
/// them to the column.
fn draw_row(
    buffer: &mut Buffer,
    verb: &str,
    mode: Option<&str>,
    area: Rect,
    style: Style,
    selected: bool,
) {
    let style = match selected {
        true => style.add_modifier(Modifier::BOLD),
        false => style,
    };
    let right = area.right();
    if selected {
        buffer.set_stringn(area.x, area.y, CURSOR, CURSOR.width(), style);
    }
    let left = area.x + CURSOR.width() as u16;
    let mut x = set_clipped(buffer, left, area.y, verb, room(left, right), style);
    let Some(mode) = mode else {
        return;
    };
    x = buffer
        .set_stringn(x, area.y, VERB_GAP, room(x, right), style)
        .0;
    set_clipped(buffer, x, area.y, mode, room(x, right), style);
}
