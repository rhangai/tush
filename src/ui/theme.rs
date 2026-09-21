//! What the screen looks up instead of deciding for itself.
//!
//! Every fixed character, every fixed word and every colour on screen comes
//! from here, in three lists of one kind each. A theme is read down one list
//! — all the marks, then all the words, then all the colours — which is what
//! makes something like [`UiTheme::ascii`] a change to one of them and
//! nothing else.
//!
//! The price of that shape is that each list keys the run states itself, so
//! the same nine names are written three times. They have to stay in step.
//!
//! Read and never written: a pane borrows the theme for the frame it draws
//! and hands nothing back.

use ratatui::{style::Color, symbols::border};

use crate::{runner::RunnerState, util::str::SmallStr};

/// How the units pane draws one unit.
///
/// The choice the pane makes every frame, which is why it is a plain enum and
/// not two widgets: the rows differ in what they say and where, not in what
/// they are.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum UiThemeMenuLayout {
    /// One line each: the mark, the name, and one column on the right.
    ///
    /// Short names wherever the config gave one — this is the row they were
    /// asked for.
    Compact,
    /// Two lines and a gap: the name on its own, then its state under it.
    ///
    /// Full names and full mode names, there being room for them here.
    #[default]
    Comfortable,
}

/// A box drawn with the three characters a teletype had: `-`, `|` and `+`.
///
/// ratatui ships no ASCII set, and a theme that says ascii while the panes
/// are still boxed in `┌─┐` is not one.
const ASCII_BORDER: border::Set<'static> = border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

/// Every fixed character the screen is drawn with.
pub struct UiThemeSymbols {
    /// The mark on a selected row, in both lists.
    ///
    /// A mark and not a bar across the row: a bar has to be painted in some
    /// colour, and every colour it could be is either a bet that the terminal
    /// is dark or a fight with the status marks.
    pub cursor: SmallStr,
    /// What joins the pieces of the log pane's title.
    pub separator: SmallStr,
    /// The corners where the units pane's border runs into the log pane's, so
    /// that the two share one wall instead of standing two next to each other.
    pub join_top: SmallStr,
    pub join_bottom: SmallStr,
    /// What the "how far below" count is arrowed with while the log is
    /// scrolled back.
    pub behind: SmallStr,
    /// What marks text that had to be cut. A name that simply stops looks
    /// like a name spelled that way.
    pub ellipsis: SmallStr,
    /// The rules and corners the panes are boxed in.
    ///
    /// ratatui's own type, because ratatui draws them: a `Block` is handed
    /// the set and does the four corners and two rules itself. The tees where
    /// the panes meet are [`join_top`](Self::join_top) and
    /// [`join_bottom`](Self::join_bottom), which no set has a place for.
    pub border: border::Set<'static>,

    /// One mark per run state, for the units pane's gutter.
    ///
    /// Each carries its meaning in its shape and not only in its colour,
    /// because a list is read at a glance and a terminal's palette is the
    /// user's.
    pub stopped: SmallStr,
    pub waiting: SmallStr,
    pub starting: SmallStr,
    pub running: SmallStr,
    pub stopping: SmallStr,
    pub done: SmallStr,
    pub exit: SmallStr,
    pub failed: SmallStr,
    pub killed: SmallStr,

    /// What names each key in the status bar. Here and not in the texts
    /// because these are the glyphs a plain terminal cannot draw.
    pub statusbar_move: SmallStr,
    pub statusbar_panel: SmallStr,
    pub statusbar_actions: SmallStr,
    pub statusbar_start: SmallStr,
    pub statusbar_stop: SmallStr,
    pub statusbar_scroll: SmallStr,
    pub statusbar_follow: SmallStr,
    pub statusbar_quit: SmallStr,
}

impl UiThemeSymbols {
    /// The mark for `state`.
    pub fn status(&self, state: RunnerState) -> &SmallStr {
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

    /// The same marks with nothing outside ASCII, for a terminal whose font
    /// or encoding will not carry the rest.
    ///
    /// Shapes stay as far apart as seven printable characters allow, since
    /// shape is what the gutter is read by: coming up and going down are `^`
    /// and `v`, a failure is `!` and a kill is `x`.
    ///
    /// **Every field is written out, and none of it falls through to
    /// [`default`](Self::default).** A `..Self::default()` here would mean a
    /// mark added later is ASCII only until somebody remembers this function
    /// — and the way you find out is a user on a terminal that cannot draw
    /// it. Spelled out, the compiler is what remembers.
    pub fn ascii() -> Self {
        Self {
            cursor: SmallStr::literal(">"),
            separator: SmallStr::literal(" - "),
            join_top: SmallStr::literal("+"),
            join_bottom: SmallStr::literal("+"),
            behind: SmallStr::literal(" v "),
            ellipsis: SmallStr::literal("..."),
            border: ASCII_BORDER,

            stopped: SmallStr::literal("-"),
            waiting: SmallStr::literal("."),
            starting: SmallStr::literal("^"),
            running: SmallStr::literal("*"),
            stopping: SmallStr::literal("v"),
            done: SmallStr::literal("+"),
            exit: SmallStr::literal("!"),
            failed: SmallStr::literal("!"),
            killed: SmallStr::literal("x"),

            statusbar_move: SmallStr::literal("up/dn"),
            statusbar_panel: SmallStr::literal("tab"),
            statusbar_actions: SmallStr::literal("enter"),
            statusbar_start: SmallStr::literal("r"),
            statusbar_stop: SmallStr::literal("bksp"),
            statusbar_scroll: SmallStr::literal("pgup/dn"),
            statusbar_follow: SmallStr::literal("end"),
            statusbar_quit: SmallStr::literal("q"),
        }
    }
}

impl Default for UiThemeSymbols {
    fn default() -> Self {
        Self {
            cursor: SmallStr::literal(">"),
            separator: SmallStr::literal(" · "),
            join_top: SmallStr::literal("┬"),
            join_bottom: SmallStr::literal("┴"),
            behind: SmallStr::literal(" ↓ "),
            ellipsis: SmallStr::literal("…"),
            border: border::PLAIN,

            stopped: SmallStr::literal("○"),
            waiting: SmallStr::literal("◌"),
            starting: SmallStr::literal("◐"),
            running: SmallStr::literal("●"),
            stopping: SmallStr::literal("◑"),
            done: SmallStr::literal("✓"),
            exit: SmallStr::literal("✗"),
            failed: SmallStr::literal("✗"),
            killed: SmallStr::literal("✗"),

            statusbar_move: SmallStr::literal("↑↓"),
            statusbar_panel: SmallStr::literal("⇥"),
            statusbar_actions: SmallStr::literal("⏎"),
            statusbar_start: SmallStr::literal("r"),
            statusbar_stop: SmallStr::literal("⌫"),
            statusbar_scroll: SmallStr::literal("pgup/dn"),
            statusbar_follow: SmallStr::literal("end"),
            statusbar_quit: SmallStr::literal("q"),
        }
    }
}

/// Every fixed word the screen is drawn with.
pub struct UiThemeTexts {
    /// One word per run state, for the places wide enough to spell it out.
    ///
    /// The three terminal states stay apart rather than collapsing into
    /// "stopped": whether a unit finished, failed or was killed is the first
    /// thing you look at a list like this to find out.
    pub stopped: SmallStr,
    pub waiting: SmallStr,
    pub starting: SmallStr,
    pub running: SmallStr,
    pub stopping: SmallStr,
    pub done: SmallStr,
    /// A failure that came with a code. This one is a *prefix* — the number
    /// is written after it, so that no string here has to be built.
    pub exit: SmallStr,
    /// A failure that came with no code.
    pub failed: SmallStr,
    pub killed: SmallStr,

    /// The log pane's title when there is no unit to name.
    pub log: SmallStr,
    /// A log pane with nothing in it, so an empty one does not read as a pane
    /// that failed to draw.
    pub empty: SmallStr,
    /// The menu's way out.
    pub cancel: SmallStr,

    /// What each key in the status bar does.
    pub statusbar_move: SmallStr,
    pub statusbar_panel: SmallStr,
    pub statusbar_actions: SmallStr,
    pub statusbar_start: SmallStr,
    pub statusbar_stop: SmallStr,
    pub statusbar_scroll: SmallStr,
    pub statusbar_follow: SmallStr,
    pub statusbar_quit: SmallStr,
}

impl UiThemeTexts {
    /// The word for `state`.
    pub fn status(&self, state: RunnerState) -> &SmallStr {
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

impl Default for UiThemeTexts {
    fn default() -> Self {
        Self {
            stopped: SmallStr::literal("stopped"),
            waiting: SmallStr::literal("waiting"),
            starting: SmallStr::literal("starting"),
            running: SmallStr::literal("running"),
            stopping: SmallStr::literal("stopping"),
            done: SmallStr::literal("done"),
            exit: SmallStr::literal("exit "),
            failed: SmallStr::literal("failed"),
            killed: SmallStr::literal("killed"),

            log: SmallStr::literal("log"),
            empty: SmallStr::literal("no output yet"),
            cancel: SmallStr::literal("cancel"),

            statusbar_move: SmallStr::literal("move"),
            statusbar_panel: SmallStr::literal("panel"),
            statusbar_actions: SmallStr::literal("actions"),
            statusbar_start: SmallStr::literal("(re)start"),
            statusbar_stop: SmallStr::literal("stop"),
            statusbar_scroll: SmallStr::literal("scroll"),
            statusbar_follow: SmallStr::literal("follow"),
            statusbar_quit: SmallStr::literal("quit"),
        }
    }
}

/// Every colour the screen is drawn with.
pub struct UiThemeColors {
    /// One colour per run state, shared by that state's mark and its word.
    pub stopped: Color,
    pub waiting: Color,
    pub starting: Color,
    pub running: Color,
    pub stopping: Color,
    pub done: Color,
    pub exit: Color,
    pub failed: Color,
    pub killed: Color,

    /// The cursor mark. White by default, so the one coloured thing in a row
    /// is still the status mark.
    pub cursor: Color,
    /// The keys named in the status bar, apart from what they do — a line of
    /// evenly dim text is a line nobody picks a key out of.
    pub statusbar_key: Color,
}

impl UiThemeColors {
    /// The colour for `state`.
    pub fn status(&self, state: RunnerState) -> Color {
        match state {
            RunnerState::Stopped => self.stopped,
            RunnerState::Waiting => self.waiting,
            RunnerState::Started => self.starting,
            RunnerState::Running => self.running,
            RunnerState::Killing => self.stopping,
            RunnerState::ExitSuccess => self.done,
            RunnerState::ExitError(Some(_)) => self.exit,
            RunnerState::ExitError(None) => self.failed,
            RunnerState::Killed(_) => self.killed,
        }
    }
}

impl Default for UiThemeColors {
    fn default() -> Self {
        Self {
            stopped: Color::DarkGray,
            waiting: Color::Gray,
            starting: Color::Yellow,
            running: Color::Green,
            stopping: Color::Yellow,
            done: Color::Cyan,
            exit: Color::Red,
            failed: Color::Red,
            killed: Color::Magenta,

            cursor: Color::White,
            statusbar_key: Color::Cyan,
        }
    }
}

/// How the screen looks.
#[derive(Default)]
pub struct UiTheme {
    /// How the units pane lays a unit out.
    pub menu_layout: UiThemeMenuLayout,
    /// Every fixed character.
    pub symbols: UiThemeSymbols,
    /// Every fixed word.
    pub texts: UiThemeTexts,
    /// Every colour.
    pub colors: UiThemeColors,
}

impl UiTheme {
    /// The default, with nothing outside ASCII drawn.
    ///
    /// Only the marks change: the words were ASCII already, and the colours
    /// are the terminal's own however old it is. Each is named anyway, for
    /// the reason [`UiThemeSymbols::ascii`] is.
    pub fn ascii() -> Self {
        Self {
            menu_layout: UiThemeMenuLayout::default(),
            symbols: UiThemeSymbols::ascii(),
            texts: UiThemeTexts::default(),
            colors: UiThemeColors::default(),
        }
    }
}
