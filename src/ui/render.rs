use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, HighlightSpacing, List, ListItem, ListState},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use arcstr::ArcStr;

use crate::{
    log::LogRegion,
    runner::RunnerState,
    ui::client::{UiClient, UiLog, UiUnit},
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

/// How wide the units column is, in columns.
///
/// A fixed width rather than a share of the terminal, because what has to fit
/// is a name — which does not grow when the window does. A proportional split
/// would spend half a wide terminal on whitespace that the log could have
/// used.
///
/// It is also what makes the list stay put: a name lands in the same place
/// whatever else is on screen, so the cursor is not somewhere new after a
/// resize.
const UNITS_WIDTH: u16 = 30;

/// What a frame is drawn from, kept between frames.
///
/// # Why anything is kept at all
///
/// A frame is a `List` of `ListItem`s of `Line`s of `Span`s, and building one
/// allocates every part of it: a string per status, per name, per mode, and a
/// vector per line. At four frames a second over a handful of units that is a
/// few hundred allocations a second to redraw text that did not change — and
/// it is text that mostly does not: a name never changes, a mode changes when
/// somebody presses a key, a state when a process does something.
///
/// So the list is built once and rendered by reference, and rebuilt only when
/// what it was built from is no longer what is being shown. A screen nobody
/// is touching allocates nothing.
///
/// # What is not kept
///
/// The log pane. Its text is new every time it changes, so caching it would
/// mean copying the client's strings instead of borrowing them — and it is
/// drawn straight into the buffer rather than through a widget, which
/// allocates nothing either way. See [`draw_log`].
pub struct UiRender {
    /// The list, owning its text so that it outlives the frame that drew it.
    list: List<'static>,
    /// What each row was built from, in order.
    rows: Vec<RowKey>,
    /// The pane width `list` was laid out for.
    width: usize,
    /// The cursor into the units, which the list widget scrolls with.
    cursor: ListState,
    /// How many lines back from the newest the log pane is showing.
    ///
    /// Zero follows the end of the log. Reset whenever the selection moves,
    /// because it is a position in one unit's output and means nothing in
    /// another's.
    log_scroll: usize,
    /// How many rows and columns of text the log pane had in the last frame.
    ///
    /// Measured while drawing and used by the *next* frame's request, since
    /// the size of a pane is not known until the layout that makes it. A
    /// frame of lag, and invisible: it sizes a region that already has
    /// [`LOG_MARGIN`] lines of slack either way, so a pane that just grew is
    /// still covered by what was fetched for the old one.
    log_size: (usize, usize),
}

/// How many lines beyond the log pane are asked for, on each side of it.
///
/// The part of the region that is a buffer rather than a request: with this
/// much slack either way, a page of scrolling in either direction is already
/// in hand and costs no round trip. Clipped to the pane's width it is a few
/// tens of kibibytes, whatever the log behind it is.
///
/// On both sides because scrolling goes both ways: a region that only
/// stretched backwards would make scrolling up free and scrolling down cost
/// exactly what it was meant to save.
const LOG_MARGIN: usize = 100;

/// What a row was built from.
///
/// Everything the drawing of one reads, and nothing else — so two of these
/// being equal means the row would come out identical, and there is no reason
/// to build it again. The name is in here even though a unit is never
/// renamed: a cache that depends on an invariant declared somewhere else is a
/// cache that goes wrong when that somewhere else changes.
#[derive(PartialEq)]
struct RowKey {
    name: ArcStr,
    mode: Option<ArcStr>,
    state: RunnerState,
}

impl RowKey {
    fn of(unit: &UiUnit) -> Self {
        Self {
            name: unit.name.clone(),
            mode: unit.mode.clone(),
            state: unit.state,
        }
    }
}

impl UiRender {
    /// A cache with nothing in it, which the first frame replaces.
    pub fn new() -> Self {
        Self {
            list: List::default(),
            rows: Vec::new(),
            width: 0,
            cursor: ListState::default().with_selected(Some(0)),
            log_scroll: 0,
            log_size: (0, 0),
        }
    }

    /// Draw one frame: the units on the left, their log on the right, and a
    /// line at the bottom.
    ///
    /// A method rather than a free function because of what drawing a frame
    /// needs: the cached list, the cursor, the scroll and the measured pane
    /// size are all here, and a function outside would have to be handed each
    /// of them through an accessor written for no other caller.
    ///
    /// What it takes from outside is the client, and only to read: the units
    /// to lay out, and the log lines to put in the pane.
    pub fn draw<C: UiClient>(&mut self, frame: &mut Frame, client: &C) {
        let [body, footer] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        let [units_area, log_area] =
            Layout::horizontal([Constraint::Length(UNITS_WIDTH), Constraint::Fill(1)]).areas(body);

        frame.render_widget(key_hints(), footer);
        self.draw_log(frame, client, log_area);

        // Brought up to date first, then borrowed: the widget wants the list
        // and the cursor at once, and they are different fields.
        self.sync_list(client.units(), units_area.width as usize);
        frame.render_stateful_widget(&self.list, units_area, &mut self.cursor);
    }

    /// The right hand pane: the selected unit's output.
    ///
    /// Titled with the unit rather than with the word "log", so the pane says
    /// which output it is about before it has any. A list with nothing
    /// selected has no unit to name, which only happens when there are no
    /// units at all.
    fn draw_log<C: UiClient>(&mut self, frame: &mut Frame, client: &C, area: Rect) {
        let title = match self.selected_unit(client) {
            Some(unit) => format!(" {} ", unit.name),
            None => " log ".to_owned(),
        };
        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        self.log_size = (inner.height as usize, inner.width as usize);

        let Some(log) = client.log() else {
            return;
        };

        // Straight into the buffer rather than through a `Paragraph`, which
        // would want a `Line` per row and a `Span` per line to hold what are
        // already plain strings. Nothing here is styled per row, nothing
        // wraps, and the buffer is cleared between frames — so there is
        // nothing a widget would do that a write does not, and a write
        // allocates nothing.
        let buffer = frame.buffer_mut();
        for (row, text) in visible(&log, self.log_scroll, inner.height as usize)
            .iter()
            .enumerate()
        {
            buffer.set_stringn(
                inner.x,
                inner.y + row as u16,
                text,
                inner.width as usize,
                Style::new(),
            );
        }
    }

    /// The unit the cursor is on.
    pub fn selected_unit<'a, C: UiClient>(&self, client: &'a C) -> Option<&'a UiUnit> {
        client.units().get(self.cursor.selected()?)
    }

    /// Move the cursor, and put the log pane back at the end.
    ///
    /// The scroll is a position in one unit's output. Carrying it over would
    /// land at an offset that means nothing in the next one — and a pane that
    /// opens in the middle of a log, for no reason the user can see, reads as
    /// output having gone missing.
    ///
    /// The cursor is pulled back inside the list afterwards:
    /// [`select_next`](ListState::select_next) and
    /// [`select_last`](ListState::select_last) do not bound what they set —
    /// `select_last` is literally `usize::MAX` — and leave it to the widget to
    /// correct while rendering. Which happens, but it would mean the
    /// selection is only right if a frame was drawn since the key that moved
    /// it.
    pub fn select(&mut self, movement: impl FnOnce(&mut ListState), units: usize) {
        movement(&mut self.cursor);
        let last = units.saturating_sub(1);
        match self.cursor.selected() {
            Some(index) if index > last => self.cursor.select(Some(last)),
            None => self.cursor.select(Some(0)),
            _ => {}
        }
        self.log_scroll = 0;
    }

    /// Scroll the log pane back by `pages`, or forward by a negative one.
    ///
    /// A page is the pane less a line of overlap, which is what makes a page
    /// turn readable: a line you have just read stays on screen to land on.
    pub fn scroll_log(&mut self, pages: isize) {
        let page = self.log_size.0.saturating_sub(1).max(1);
        self.log_scroll = if pages < 0 {
            self.log_scroll.saturating_sub(page)
        } else {
            self.log_scroll.saturating_add(page)
        };
    }

    /// The rectangle the log pane wants: what it draws, plus
    /// [`LOG_MARGIN`] lines either side and clipped to its width.
    pub fn log_region(&self) -> LogRegion {
        let (rows, columns) = self.log_size;
        let start = self.log_scroll.saturating_sub(LOG_MARGIN);
        LogRegion::new(start..self.log_scroll + rows + LOG_MARGIN, 0..columns)
    }

    /// Pull the log scroll back to what there is to show.
    ///
    /// `held` is how far back the lines that came in actually reach. The
    /// client never says how much history it has; it says what it found, and
    /// coming back with fewer lines than were asked for is how a view learns
    /// it reached the top.
    ///
    /// The limit is where the oldest line reaches the top of the pane, not
    /// the bottom: past that the window hangs off the end of the history and
    /// every further line of scroll buys a blank row. A log shorter than the
    /// pane therefore does not scroll at all.
    pub fn clamp_log(&mut self, held: usize) {
        self.log_scroll = self.log_scroll.min(held.saturating_sub(self.log_size.0));
    }

    /// The list to render, rebuilt first if it is no longer what it should
    /// be.
    ///
    /// The comparison is the whole point: it is a length, a width and a few
    /// short fields per row, against a rebuild that is a few dozen
    /// allocations. It is also the only place that decides — nothing else
    /// needs to know when to invalidate, because the answer is derived from
    /// the units themselves rather than signalled.
    ///
    fn sync_list(&mut self, units: &[UiUnit], width: usize) {
        if self.width != width || !self.is_current(units) {
            self.rebuild(units, width);
        }
    }

    /// Whether the rows already built are the rows these units want.
    fn is_current(&self, units: &[UiUnit]) -> bool {
        self.rows.len() == units.len()
            && std::iter::zip(&self.rows, units).all(|(row, unit)| *row == RowKey::of(unit))
    }

    fn rebuild(&mut self, units: &[UiUnit], width: usize) {
        // Two borders and the cursor's own column; what is left is what a row
        // has to lay itself out inside, which is why the rows are built here
        // rather than by a widget that never learns how wide it ended up.
        let row_width = width.saturating_sub(2).saturating_sub(CURSOR.width());

        let items: Vec<ListItem> = units.iter().map(|unit| item(unit, row_width)).collect();
        self.list = List::new(items)
            .block(Block::bordered())
            .highlight_symbol(CURSOR)
            // Always, so the cursor's column is reserved on every row and a
            // name does not shift sideways as the selection passes over it.
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::new().add_modifier(Modifier::BOLD));

        self.rows.clear();
        self.rows.extend(units.iter().map(RowKey::of));
        self.width = width;
    }
}

impl Default for UiRender {
    fn default() -> Self {
        Self::new()
    }
}

/// The part of a fetched region the pane is actually showing.
///
/// The region is wider than the pane on both sides — that is the margin the
/// view scrolls within without asking again — so the visible rows are a slice
/// out of the middle of it, and finding them is arithmetic on distances from
/// the end of the log.
///
/// The lines run oldest first and the last is at distance
/// `region.lines.start`, so the line at distance `d` sits `d -
/// region.lines.start` from the end of the slice. The pane wants distances
/// from `scroll` upwards, so that is where its last row is.
///
/// Everything saturates because the region that came back need not be the one
/// asked for: a client behind a scroll hands back what it has, and the pane
/// draws the overlap rather than nothing.
fn visible<'a>(log: &'a UiLog<'a>, scroll: usize, rows: usize) -> &'a [String] {
    let from_end = scroll.saturating_sub(log.region.line_start);
    let end = log.lines.len().saturating_sub(from_end);
    let start = end.saturating_sub(rows);
    &log.lines[start..end]
}

/// One row, over two lines: the name, and under it everything that qualifies
/// it.
///
/// # Why two lines and not one
///
/// Because on one line the name and the status compete for the same width,
/// and the loser is whichever the pane is too narrow for. Given a line of its
/// own the name gets the whole column and is cut only when it genuinely does
/// not fit, while the detail line underneath has room for as many facets as
/// turn up without ever being the reason a name lost its tail.
///
/// It costs rows, which a pane of a few procs has to spare — and the third,
/// blank line is part of the bargain: two lines with nothing between them
/// read as four rows rather than two.
///
/// # The detail line
///
/// The status first, in its colour, because it is the word the row exists to
/// tell you. Then the mode, dimmed, for a unit that has modes — the one thing
/// about a running proc that its name and its state do not already say, since
/// two units can both be `running` and be doing entirely different work.
///
/// More facets will want this line — a restart count, a port, the group that
/// started it — which is why it is a run of spans rather than a pair of
/// fields.
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
fn item(unit: &UiUnit, width: usize) -> ListItem<'static> {
    let (label, color) = status(&unit.state);
    let used = DETAIL_INDENT.width() + label.width();

    let mut detail = vec![
        Span::raw(DETAIL_INDENT),
        Span::styled(label, Style::new().fg(color)),
    ];
    let running_mode = match unit.state {
        RunnerState::Stopped => None,
        _ => unit.mode.as_deref(),
    };
    if let Some(mode) = running_mode {
        let mode = fit(
            &format!("{MODE_SEPARATOR}{mode}"),
            width.saturating_sub(used),
        );
        detail.push(Span::styled(mode, Style::new().dim()));
    }

    ListItem::new(vec![
        Line::from(fit(&unit.name, width)),
        Line::from(detail),
        Line::default(),
    ])
}

/// `text` in at most `width` columns, ending in an ellipsis if it had to be
/// cut.
///
/// Counted in columns rather than characters because that is what the
/// terminal lays out: a name with a wide character in it takes two columns
/// for it, and measuring in `char`s would leave the status column a column
/// short for every one of them.
fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    // One column goes to the ellipsis, so anything narrower than that has
    // room for the mark and nothing else.
    let room = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > room {
            break;
        }
        out.push(ch);
        used += w;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

/// The bottom line: what the keys do.
///
/// There is nothing else competing for it yet. When a command can report
/// having been refused, this is the line it will have to share.
fn key_hints() -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        key_hint("↑/↓", "move"),
        key_hint("enter", "start / next mode"),
        key_hint("backspace", "stop"),
        key_hint("q", "quit"),
    ])
}

/// `key`, then what it does, dimmed, with a gap before the next one.
fn key_hint<'a>(key: &'a str, what: &'a str) -> Span<'a> {
    Span::raw(format!("{key} {what}   ")).dim()
}

/// What a state is called on screen, and the colour it is called it in.
///
/// The three terminal states are kept apart rather than collapsed into
/// "stopped": whether a unit finished, failed, or was killed by the user is
/// the first thing you look at a list like this to find out.
fn status(state: &RunnerState) -> (String, Color) {
    match state {
        RunnerState::Stopped => ("stopped".to_owned(), Color::DarkGray),
        RunnerState::Waiting => ("waiting".to_owned(), Color::Gray),
        RunnerState::Started => ("starting".to_owned(), Color::Yellow),
        RunnerState::Running => ("running".to_owned(), Color::Green),
        RunnerState::Killing => ("stopping".to_owned(), Color::Yellow),
        RunnerState::ExitSuccess => ("done".to_owned(), Color::Cyan),
        RunnerState::ExitError(code) => (
            match code {
                Some(code) => format!("exit {code}"),
                None => "failed".to_owned(),
            },
            Color::Red,
        ),
        RunnerState::Killed(_) => ("killed".to_owned(), Color::Magenta),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::log::LogRegion;

    fn unit(name: &str, mode: Option<&str>, state: RunnerState) -> UiUnit {
        UiUnit {
            key: name.into(),
            name: name.into(),
            mode: mode.map(Into::into),
            state,
        }
    }

    fn units() -> Vec<UiUnit> {
        vec![
            unit("server", Some("Build"), RunnerState::Running),
            unit("setup", None, RunnerState::ExitSuccess),
        ]
    }

    /// Nothing changed, so nothing is rebuilt — which is the whole point:
    /// four frames a second over a still screen should allocate nothing.
    #[test]
    fn a_drawn_list_stays_current_while_its_units_do() {
        let mut render = UiRender::new();
        let units = units();
        render.sync_list(&units, 30);
        assert!(render.is_current(&units));
    }

    /// Every field a row reads has to invalidate it, or the screen goes on
    /// showing something that stopped being true.
    #[test]
    fn every_field_a_row_shows_invalidates_it() {
        let mut render = UiRender::new();
        render.sync_list(&units(), 30);

        for changed in [
            unit("server", Some("Build"), RunnerState::Killed(None)),
            unit("server", Some("Watch"), RunnerState::Running),
            unit("server", None, RunnerState::Running),
            unit("renamed", Some("Build"), RunnerState::Running),
        ] {
            let mut units = units();
            units[0] = changed;
            assert!(!render.is_current(&units), "{:?}", units[0]);
        }
    }

    /// A unit appearing or going away is a different list, even if every unit
    /// that stayed is untouched.
    #[test]
    fn a_different_number_of_units_is_not_current() {
        let mut render = UiRender::new();
        render.sync_list(&units(), 30);

        let mut more = units();
        more.push(unit("extra", None, RunnerState::Stopped));
        assert!(!render.is_current(&more));
        assert!(!render.is_current(&units()[..1]));
    }

    /// The rows are laid out to a width, so a resize rebuilds them even
    /// though no unit moved.
    #[test]
    fn a_resize_rebuilds_the_rows() {
        let mut render = UiRender::new();
        let units = units();
        render.sync_list(&units, 30);
        assert_eq!(render.width, 30);

        render.sync_list(&units, 44);
        assert_eq!(render.width, 44, "the new width should have been taken");
        assert!(render.is_current(&units));
    }

    /// A fetched region of `held` lines, the oldest of them at distance
    /// `start + held - 1` from the end of the log.
    fn lines(start: usize, held: usize) -> Vec<String> {
        // Numbered by distance from the end, so a slice says where it came
        // from: `d0` is the newest line the region holds.
        (0..held)
            .map(|n| format!("d{}", start + held - 1 - n))
            .collect()
    }

    fn visible_texts(scroll: usize, rows: usize, start: usize, held: usize) -> Vec<String> {
        let lines = lines(start, held);
        let log = UiLog {
            region: LogRegion::new(start..start + held, 0..80),
            revision: 0,
            lines: &lines,
        };
        visible(&log, scroll, rows).to_vec()
    }

    /// Following the end: the pane's last row is the newest line.
    #[test]
    fn the_pane_ends_at_the_line_the_scroll_names() {
        assert_eq!(visible_texts(0, 3, 0, 10), ["d2", "d1", "d0"]);
        assert_eq!(visible_texts(4, 3, 0, 10), ["d6", "d5", "d4"]);
    }

    /// The region is wider than the pane on both sides, so the visible rows
    /// come out of the middle — the margin is fetched and not drawn.
    #[test]
    fn the_margin_is_fetched_and_not_drawn() {
        // Region covers distances 5..15, pane shows 8..11.
        assert_eq!(visible_texts(8, 3, 5, 10), ["d10", "d9", "d8"]);
    }

    /// Less history than the pane has room for fills what it can rather than
    /// sliding off the end of what there is.
    #[test]
    fn a_short_log_gives_back_every_line_it_has() {
        assert_eq!(visible_texts(0, 10, 0, 3), ["d2", "d1", "d0"]);
    }

    /// A client that has not caught up with a scroll hands back the region it
    /// still has. The pane draws the overlap rather than blanking, and draws
    /// nothing only when there is none.
    #[test]
    fn a_stale_region_is_drawn_where_it_actually_belongs() {
        // Asked to show 8..11, but the client still holds 0..10: the two meet
        // at distances 8 and 9, so those are the rows that get drawn.
        assert_eq!(visible_texts(8, 3, 0, 10), ["d9", "d8"]);
        // And a region entirely newer than the scroll has no overlap at all.
        assert!(visible_texts(50, 3, 0, 10).is_empty());
    }
}
