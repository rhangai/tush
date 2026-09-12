use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, HighlightSpacing, List, ListItem, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    runner::RunnerState,
    ui::{
        client::{UiClient, UiLog, UiUnit},
        ui::Ui,
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

/// Draw one frame: the units on the left, their log on the right, and a line
/// at the bottom.
pub fn draw<C: UiClient>(frame: &mut Frame, ui: &mut Ui<C>) {
    let [body, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    let [units_area, log_area] =
        Layout::horizontal([Constraint::Length(UNITS_WIDTH), Constraint::Fill(1)]).areas(body);

    frame.render_widget(key_hints(), footer);

    draw_log(frame, ui, log_area);

    // Two borders and the cursor's own column; what is left is what a row has
    // to lay itself out inside, which is why the rows are built here rather
    // than by a widget that never learns how wide it ended up.
    let row_width = (units_area.width as usize)
        .saturating_sub(2)
        .saturating_sub(CURSOR.width());

    let (units, list_state) = ui.frame();
    let items: Vec<ListItem> = units.iter().map(|unit| item(unit, row_width)).collect();
    let list = List::new(items)
        .block(Block::bordered())
        .highlight_symbol(CURSOR)
        // Always, so the cursor's column is reserved on every row and a name
        // does not shift sideways as the selection passes over it.
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(Style::new().add_modifier(Modifier::BOLD));
    frame.render_stateful_widget(list, units_area, list_state);
}

/// The right hand pane: the selected unit's output.
///
/// Titled with the unit rather than with the word "log", so the pane says
/// which output it is about before it has any. A list with nothing selected
/// has no unit to name, which only happens when there are no units at all.
///
/// # Measuring before drawing
///
/// A pane's size is not known until the layout that makes it, and the region
/// it needs was asked for a frame earlier. So the size is recorded here for
/// the next request — a frame of lag that the margin in the region absorbs.
fn draw_log<C: UiClient>(frame: &mut Frame, ui: &mut Ui<C>, area: Rect) {
    let title = match ui.selected() {
        Some(unit) => format!(" {} ", unit.name),
        None => " log ".to_owned(),
    };
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    ui.set_log_size(inner.height as usize, inner.width as usize);

    let scroll = ui.log_scroll();
    let Some(log) = ui.log() else {
        return;
    };
    let lines: Vec<Line> = visible(&log, scroll, inner.height as usize)
        .iter()
        .map(|text| Line::raw(text.as_str()))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
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
    let from_end = scroll.saturating_sub(log.region.lines.start);
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
            region: LogRegion {
                lines: start..start + held,
                columns: 0..80,
            },
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
