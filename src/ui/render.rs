use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, List, ListItem},
};

use crate::{
    runner::RunnerState,
    ui::{
        client::{UiClient, UiUnit},
        ui::Ui,
    },
};

/// How wide the status column is, in columns.
///
/// Fixed, so the names line up into a column of their own and the eye can run
/// down either one. Wide enough for the longest label plus an exit code.
const STATUS_WIDTH: usize = 10;

/// How wide the units column is, in columns.
///
/// A fixed width rather than a share of the terminal, because what has to fit
/// is the status column plus a name — neither of which grows when the window
/// does. A proportional split would spend half a wide terminal on whitespace
/// that the log could have used.
///
/// It is also what makes the list stay put: a name lands in the same place
/// whatever else is on screen, so the cursor is not somewhere new after a
/// resize.
const UNITS_WIDTH: u16 = 32;

/// Draw one frame: the units on the left, their log on the right, and a line
/// at the bottom.
pub fn draw<C: UiClient>(frame: &mut Frame, ui: &mut Ui<C>) {
    let [body, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    let [units_area, log_area] =
        Layout::horizontal([Constraint::Length(UNITS_WIDTH), Constraint::Fill(1)]).areas(body);

    frame.render_widget(key_hints(), footer);

    // Both halves of what a frame is drawn from, taken together: the title on
    // the right names the row the cursor is on, so the two panes have to be
    // reading the same selection as each other.
    let (units, list_state) = ui.frame();
    let selected = list_state.selected().and_then(|index| units.get(index));

    frame.render_widget(log_pane(selected), log_area);

    let items: Vec<ListItem> = units
        .iter()
        .map(|unit| item(&unit.state, &unit.key))
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(" units "))
        .highlight_symbol("▌")
        .highlight_style(Style::new().add_modifier(Modifier::BOLD));
    frame.render_stateful_widget(list, units_area, list_state);
}

/// The right hand pane, where the selected unit's output will go.
///
/// Empty so far — what fills it is a
/// [`LogReader`](crate::log::LogReader) walk, and a [`UiClient`] has no way
/// to hand one over yet. The frame is here first because the layout is the
/// part the rest has to fit into: a pane that appears later would move the
/// list sideways the moment it did.
///
/// Titled with the unit rather than with the word "log", so the pane says
/// which output it is about before it has any. A list with nothing selected
/// has no unit to name, which only happens when there are no units at all.
fn log_pane(unit: Option<&UiUnit>) -> Block<'static> {
    match unit {
        Some(unit) => Block::bordered().title(format!(" {} ", unit.key)),
        None => Block::bordered().title(" log "),
    }
}

/// One row: the status, then the name.
fn item<'a>(state: &RunnerState, key: &'a str) -> ListItem<'a> {
    let (label, color) = status(state);
    ListItem::new(Line::from(vec![
        Span::styled(format!(" {label:<STATUS_WIDTH$}"), Style::new().fg(color)),
        Span::raw(key),
    ]))
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
