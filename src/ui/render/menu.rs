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

/// How much clear space the entries get inside the border.
///
/// A popup sized to its longest entry and nothing more reads as cramped
/// rather than as compact: the border ends up touching the text on three
/// sides, and the thing that is supposed to be sitting on top of the screen
/// looks like it is being squeezed by it.
const PAD_X: u16 = 2;
const PAD_Y: u16 = 1;

/// The narrowest the menu may be, however short its entries are.
///
/// `Start` and `Stop` are four and five columns wide, and a box drawn to fit
/// them is a box you do not notice has opened. The floor is what keeps a menu
/// looking like the same object whichever unit it belongs to.
const MIN_WIDTH: u16 = 28;

/// The way out, which every menu has and no behavior provides.
///
/// Written here and not by [`UnitBehavior`](crate::unit::UnitBehavior)
/// because it is not something asked of a unit: nothing happens to a process
/// when a popup is dismissed, and an event for it would be one the unit layer
/// has to carry and ignore.
///
/// It is a row rather than only a key because a key that is not written down
/// is a key that is not there. <kbd>Esc</kbd> still works, and now the menu
/// says so by having somewhere for it to land.
const CANCEL: &str = "cancel";

/// How many rows sit between the last entry and the way out.
///
/// One, and blank. Acting on a unit and walking away from the menu are
/// different kinds of thing, and the gap is what keeps `Cancel` from being
/// the row you hit when you meant `Stop`.
const CANCEL_GAP: u16 = 1;

/// What the menu is showing, and which entry the cursor is on.
///
/// Its own state for the usual reason — the widget is built and thrown away
/// every frame — and for one more: the entries are read out of the session
/// *once*, when the menu opens, and this is what holds them still afterwards.
/// A list rebuilt per frame would renumber itself under the cursor the moment
/// a process exited, which is the one thing a menu may never do.
#[derive(Default)]
pub struct UiRenderMenuState {
    /// The unit it is open for, and `None` when it is closed.
    ///
    /// Kept rather than re-read from the selection, so that moving the cursor
    /// underneath — which nothing can do while the menu has the keys, but a
    /// resize or a shrinking list can — cannot make the entries and the unit
    /// they act on come apart.
    key: Option<ArcStr>,
    /// What that unit is called, for the title.
    title: ArcStr,
    /// The entries, as the behavior wrote them.
    ///
    /// Kept when the menu closes, so that the next open refills this
    /// allocation instead of making one.
    items: Vec<UnitChoice>,
    /// Which row the cursor is on: an entry, or [`CANCEL`] just past the
    /// last of them.
    ///
    /// Only ever a row that can be chosen — an enabled entry, or the way out,
    /// which always can be.
    cursor: usize,
}

impl UiRenderMenuState {
    /// Whether the menu has the keys.
    pub fn is_open(&self) -> bool {
        self.key.is_some()
    }

    /// Lend out the entry buffer to be refilled, leaving an empty one behind.
    ///
    /// Taken by value rather than filled through a `&mut`, because the caller
    /// has to hand it to the client — and holding a `&mut` into the screen
    /// while reading the session is the one shape the borrow checker will not
    /// have. The capacity comes back with it on [`open`](Self::open).
    pub fn take_items(&mut self) -> Vec<UnitChoice> {
        std::mem::take(&mut self.items)
    }

    /// Open it over `key`, titled `title`, listing `items`.
    ///
    /// The cursor starts on the entry for the mode the unit is already on, so
    /// that <kbd>Enter</kbd> twice runs what the row was already offering —
    /// which is what one press used to do, and is worth leaving under the
    /// hand that learnt it. Failing that, the first entry that can be chosen
    /// at all.
    ///
    /// Failing *that* — a proc that declared no way to run, with no run to
    /// stop — it starts on [`CANCEL`], which is the only row left. That is
    /// what keeps the cursor off a disabled entry in every case, and so keeps
    /// <kbd>Enter</kbd> from ever being a press that does nothing.
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

    /// Move the cursor to the next entry that can be chosen.
    ///
    /// Disabled entries are stepped over rather than landed on: they are
    /// drawn so the list keeps its shape, not so they can be pressed, and a
    /// cursor that stops on one would make the list feel stuck. Neither end
    /// wraps, for the same reason the units list does not.
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
    /// Every row answers with one of the two, because the cursor cannot be
    /// anywhere else: [`open`](Self::open) and [`select`](Self::select)
    /// between them keep it on an enabled entry or on the way out. The
    /// disabled arm below is therefore unreachable rather than tolerated, and
    /// it answers `Cancel` because a press that can do nothing else should at
    /// least not leave the menu sitting there.
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

    /// How big a box it needs: wide enough for the longest entry or the
    /// title, tall enough for every entry, plus the border.
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
/// Two variants and not an `Option`, because closing is a real answer and not
/// the absence of one: the user picked the row that says so.
pub enum UiMenuChoice {
    /// Send this to the unit, and close.
    Send { key: ArcStr, event: UnitEvent },
    /// Close, and ask for nothing.
    Cancel,
}

/// The action menu: what can be asked of one unit, over the top of everything.
///
/// # Why the entries are not built here
///
/// Because the behavior wrote them, and it is the only thing that knows what
/// they are: which modes exist, which one is current, and which of them can
/// be chosen from where the run is. A pane that worked that out from a unit's
/// name and state would be a second copy of the rules, kept in the module
/// least able to check them.
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
        // What is underneath is still drawn, and would otherwise show through
        // the gaps in this one.
        Clear.render(area, buffer);
        let inner = self.border.inner(area);
        self.border.render(area, buffer);

        // The title over the top border, in pieces, for the reason the log
        // pane writes its own: a block's title is a `Line`, and building one
        // and rendering one both allocate.
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
///
/// # The two styles
///
/// Selected is bold and carries the mark, the same way a unit row is, so that
/// one idiom means one thing in both lists.
///
/// Disabled is grey, and it is grey rather than absent because that is what
/// keeps the menu the same shape every time it opens for the same unit. What
/// it costs is a row you cannot use; what it buys is that `Stop` is in the
/// place it was last time, which is what lets a menu be pressed without being
/// read.
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

/// A verb, and what it applies to, under the cursor column.
///
/// Shared by the entries and by [`CANCEL`], which is not a [`UnitChoice`] and
/// never will be, but is a row in the same list and has to line up with them
/// to the column.
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
