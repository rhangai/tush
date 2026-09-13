//! What the screen looks up instead of deciding for itself.
//!
//! Every fixed character and every colour on screen comes from here, so that
//! changing how `tush` looks is changing a value rather than hunting through
//! the panes for the literal that drew it.
//!
//! Read and never written: a pane borrows the theme for the frame it draws
//! and hands nothing back. Nothing here is behind a lock or an `Option` for
//! that reason — a screen always has a theme, even if it is the default one.

use arcstr::{ArcStr, literal};
use ratatui::style::Color;

use crate::runner::RunnerState;

/// How the units pane draws one unit.
///
/// The choice the pane makes every frame, which is why it is a plain enum and
/// not two widgets: the rows differ in what they say and where, not in what
/// they are.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum UiThemeUnits {
    /// One line each: the mark, the name, and one column on the right.
    ///
    /// Short names wherever the config gave one — this is the row they were
    /// asked for.
    #[default]
    Compact,
    /// Two lines and a gap: the name, then its state spelled out under it.
    ///
    /// Full names and full mode names, there being room for them here.
    Roomy,
}

/// How one run state is shown.
pub struct UiThemeStatus {
    /// The mark in the units pane's gutter.
    ///
    /// Carries its meaning in its shape and not only in its colour, because a
    /// list is read at a glance and a terminal's palette is the user's.
    pub mark: ArcStr,
    /// The same thing as a word, for the places wide enough to spell it out.
    pub label: ArcStr,
    /// What both are drawn in.
    pub color: Color,
}

impl UiThemeStatus {
    /// One row of the table in [`UiThemeStatuses::default`].
    pub fn new(mark: ArcStr, label: ArcStr, color: Color) -> Self {
        Self { mark, label, color }
    }
}

/// One entry per run state.
///
/// Spelled out rather than kept in a map: the set is closed, a theme that
/// forgets one is a theme that will not compile, and the lookup is a match.
pub struct UiThemeStatuses {
    pub stopped: UiThemeStatus,
    pub waiting: UiThemeStatus,
    pub starting: UiThemeStatus,
    pub running: UiThemeStatus,
    pub stopping: UiThemeStatus,
    pub done: UiThemeStatus,
    /// A failure that came with a code. Its label is a *prefix* — the number
    /// is written after it, so that no string here has to be built.
    pub exit: UiThemeStatus,
    /// A failure that came with no code.
    pub failed: UiThemeStatus,
    pub killed: UiThemeStatus,
}

impl UiThemeStatuses {
    /// The entry for `state`.
    ///
    /// The three terminal states stay apart rather than collapsing into
    /// "stopped": whether a unit finished, failed or was killed is the first
    /// thing you look at a list like this to find out.
    pub fn get(&self, state: RunnerState) -> &UiThemeStatus {
        match state {
            RunnerState::Stopped => &self.stopped,
            RunnerState::Waiting => &self.waiting,
            RunnerState::Started => &self.starting,
            RunnerState::Running => &self.running,
            RunnerState::Killing => &self.stopping,
            RunnerState::ExitSuccess => &self.done,
            RunnerState::ExitError(Some(_)) => &self.exit,
            RunnerState::ExitError(None) => &self.failed,
            RunnerState::Killed(_) => &self.killed,
        }
    }
}

impl Default for UiThemeStatuses {
    fn default() -> Self {
        Self {
            stopped: UiThemeStatus::new(literal!("○"), literal!("stopped"), Color::DarkGray),
            waiting: UiThemeStatus::new(literal!("◌"), literal!("waiting"), Color::Gray),
            starting: UiThemeStatus::new(literal!("◐"), literal!("starting"), Color::Yellow),
            running: UiThemeStatus::new(literal!("●"), literal!("running"), Color::Green),
            stopping: UiThemeStatus::new(literal!("◑"), literal!("stopping"), Color::Yellow),
            done: UiThemeStatus::new(literal!("✓"), literal!("done"), Color::Cyan),
            exit: UiThemeStatus::new(literal!("✗"), literal!("exit "), Color::Red),
            failed: UiThemeStatus::new(literal!("✗"), literal!("failed"), Color::Red),
            killed: UiThemeStatus::new(literal!("✗"), literal!("killed"), Color::Magenta),
        }
    }
}

/// The fixed characters the screen is drawn with.
pub struct UiThemeSymbols {
    /// The mark on a selected row, in both lists.
    ///
    /// A mark and not a bar across the row: a bar has to be painted in some
    /// colour, and every colour it could be is either a bet that the terminal
    /// is dark or a fight with the status marks.
    pub cursor: ArcStr,
    /// What joins the pieces of the log pane's title.
    pub separator: ArcStr,
    /// The corners where the units pane's border runs into the log pane's, so
    /// that the two share one wall instead of standing two next to each other.
    pub join_top: ArcStr,
    pub join_bottom: ArcStr,
    /// What the "how far below" count is arrowed with while the log is
    /// scrolled back.
    pub behind: ArcStr,
    /// What marks text that had to be cut. A name that simply stops looks
    /// like a name spelled that way.
    pub ellipsis: ArcStr,
}

impl Default for UiThemeSymbols {
    fn default() -> Self {
        Self {
            cursor: literal!(">"),
            separator: literal!(" · "),
            join_top: literal!("┬"),
            join_bottom: literal!("┴"),
            behind: literal!(" ↓ "),
            ellipsis: literal!("…"),
        }
    }
}

/// The colours that are not a status's.
pub struct UiThemeColors {
    /// The cursor mark. White by default, so the one coloured thing in a row
    /// is still the status mark.
    pub cursor: Color,
    /// The keys in the footer, apart from what they do — a line of evenly dim
    /// text is a line nobody picks a key out of.
    pub key: Color,
}

impl Default for UiThemeColors {
    fn default() -> Self {
        Self {
            cursor: Color::White,
            key: Color::Cyan,
        }
    }
}

/// How the screen looks.
#[derive(Default)]
pub struct UiTheme {
    /// How the units pane lays a unit out.
    pub units: UiThemeUnits,
    /// One entry per run state.
    pub status: UiThemeStatuses,
    /// The fixed characters.
    pub symbol: UiThemeSymbols,
    /// The colours that belong to no state.
    pub color: UiThemeColors,
}
