use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{DefaultTerminal, widgets::ListState};
use tokio_stream::StreamExt;

use crate::{
    runner::RunnerState,
    ui::{
        client::{UiClient, UiCommand, UiUnit},
        render,
    },
    unit::UnitEvent,
};

/// How often the screen is redrawn when nothing is being pressed.
///
/// A run changes state without anybody asking — a process exits, a start
/// finally schedules — and with a [`UiClient`] that cannot push, the only way
/// to find out is to look. Four times a second is under the threshold where a
/// person notices the lag and far above what a
/// [`sync`](UiClient::sync) costs.
const REFRESH: Duration = Duration::from_millis(250);

/// The terminal UI: a list of units, and the keys that act on the selected one.
///
/// Generic over its [`UiClient`] rather than holding a `dyn` one, so the
/// in-process path stays direct calls. The day `tush attach` needs to choose
/// an implementation at runtime, the choice is one match in `main` over which
/// `Ui<_>` to run — not a change here.
///
/// # What it keeps
///
/// Only the cursor. The units belong to the client, which is the point of the
/// last round of this design: there is no second copy here to fall out of
/// step with the one that gets drawn, and no frame assembled out of two
/// different moments.
pub struct Ui<C: UiClient> {
    /// Where the units are read from and where the commands go.
    client: C,
    /// The cursor into the client's units, kept by the list widget so
    /// scrolling works.
    list: ListState,
    /// Cleared by <kbd>q</kbd>, which is the only way out.
    running: bool,
}

impl<C: UiClient> Ui<C> {
    /// Take over the terminal, run until the user quits, and give it back.
    ///
    /// The terminal is restored whatever the loop did — including on the
    /// error path, which is the reason for the temporary rather than a `?` on
    /// the loop. A failure that leaves the terminal in raw mode with no
    /// cursor is a failure you cannot read the message of.
    pub async fn run(client: C) -> Result<()> {
        let mut ui = Self {
            client,
            list: ListState::default().with_selected(Some(0)),
            running: true,
        };
        let mut terminal = ratatui::init();
        let result = ui.main_loop(&mut terminal).await;
        ratatui::restore();
        result
    }

    /// Sync, draw, wait for whichever comes first — a key or the next tick —
    /// and repeat.
    ///
    /// Syncing at the top of the loop rather than after handling a key is
    /// what makes a command's effect visible: `send` returns before the
    /// session has acted on it, so the frame that shows the result is the
    /// next one through here, not the one the key press was in.
    async fn main_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut events = EventStream::new();
        let mut ticks = tokio::time::interval(REFRESH);

        while self.running {
            self.client.sync();
            terminal.draw(|frame| render::draw(frame, self))?;
            tokio::select! {
                _ = ticks.tick() => {}
                event = events.next() => match event {
                    Some(event) => self.handle(event?),
                    // The terminal's input ended under us — a closed pty,
                    // usually. There is nobody left to draw for.
                    None => break,
                },
            }
        }
        Ok(())
    }

    /// Act on one terminal event.
    fn handle(&mut self, event: Event) {
        // A key press, and only a press: terminals that report releases and
        // repeats would otherwise run every binding two or three times.
        let Event::Key(key) = event else {
            return;
        };
        if key.kind != KeyEventKind::Press {
            return;
        }
        if Self::is_quit(&key) {
            self.running = false;
            return;
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.list.select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.list.select_previous(),
            KeyCode::Home | KeyCode::Char('g') => self.list.select_first(),
            KeyCode::End | KeyCode::Char('G') => self.list.select_last(),
            KeyCode::Enter => self.activate(),
            KeyCode::Backspace => self.send(|key| UiCommand::Stop { key }),
            _ => {}
        }
        self.clamp();
    }

    /// <kbd>Enter</kbd>: start the selected unit, or move it along if it is
    /// already running.
    ///
    /// The decision is made here, against the state that was on the screen
    /// when the key was pressed, rather than being a command of its own. Two
    /// reasons. The user aimed at what they could see, so the snapshot they
    /// aimed at is the right thing to read. And a behavior with modes answers
    /// [`UnitEvent::Default`] by switching to the next one and restarting —
    /// which is only what you want for something that is *already* running;
    /// on a stopped unit it would silently skip a mode before starting it.
    fn activate(&mut self) {
        let Some(unit) = self.selected() else {
            return;
        };
        let command = if matches!(unit.state, RunnerState::Stopped) {
            UiCommand::Start {
                key: unit.key.clone(),
            }
        } else {
            UiCommand::Dispatch {
                key: unit.key.clone(),
                event: UnitEvent::Default,
            }
        };
        self.client.send(command);
    }

    /// Send the command `command` builds for the selected unit's key, if
    /// there is one selected.
    fn send(&mut self, command: impl FnOnce(String) -> UiCommand) {
        let Some(unit) = self.selected() else {
            return;
        };
        self.client.send(command(unit.key.clone()));
    }

    /// The unit under the cursor, if the list is not empty.
    fn selected(&self) -> Option<&UiUnit> {
        self.client.units().get(self.list.selected()?)
    }

    /// Pull the cursor back inside the list.
    ///
    /// [`select_next`](ListState::select_next) and
    /// [`select_last`](ListState::select_last) do not bound what they set —
    /// `select_last` is literally `usize::MAX` — and leave it to the widget
    /// to correct while rendering. Which happens, but it means every read of
    /// the selection is only correct if a frame was drawn since the key that
    /// moved it. Bounding it here instead makes the cursor true the moment it
    /// moves, so [`selected`](Ui::selected) does not depend on the order the
    /// loop happens to do things in.
    fn clamp(&mut self) {
        let last = self.client.units().len().saturating_sub(1);
        match self.list.selected() {
            Some(index) if index > last => self.list.select(Some(last)),
            None => self.list.select(Some(0)),
            _ => {}
        }
    }

    /// <kbd>q</kbd>, <kbd>Esc</kbd> or <kbd>Ctrl-C</kbd>.
    ///
    /// `Ctrl-C` is in here because raw mode means the terminal no longer
    /// turns it into a signal — if the UI does not treat it as quit, nothing
    /// does.
    fn is_quit(key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => true,
            KeyCode::Char('c') => key.modifiers.contains(KeyModifiers::CONTROL),
            _ => false,
        }
    }

    /// What a frame is drawn from: the units, and the cursor into them.
    ///
    /// Handed out together because the list widget needs both at once — the
    /// rows borrow their names from the units and the widget writes its
    /// scroll offset back into the cursor — and two accessors would make that
    /// one immutable and one mutable borrow of the same `Ui`.
    pub(super) fn frame(&mut self) -> (&[UiUnit], &mut ListState) {
        (self.client.units(), &mut self.list)
    }
}
