use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, List, ListItem},
};

use crate::{
    runner::RunnerState,
    ui::{client::UiClient, ui::Ui},
};

/// How wide the status column is, in columns.
///
/// Fixed, so the names line up into a column of their own and the eye can run
/// down either one. Wide enough for the longest label plus an exit code.
const STATUS_WIDTH: usize = 10;

/// Draw one frame: the list of units, and a line at the bottom.
pub fn draw<C: UiClient>(frame: &mut Frame, ui: &mut Ui<C>) {
    let [main, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());

    frame.render_widget(key_hints(), footer);

    let (units, list_state) = ui.frame();
    let items: Vec<ListItem> = units
        .iter()
        .map(|unit| item(&unit.state, &unit.key))
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(" units "))
        .highlight_symbol("▌")
        .highlight_style(Style::new().add_modifier(Modifier::BOLD));
    frame.render_stateful_widget(list, main, list_state);
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
