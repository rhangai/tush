use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use tokio_stream::StreamExt;

use crate::{
    ui::{
        client::{UiClient, UiCommand, UiUnit},
        render::{Move, UiRender},
    },
    unit::UnitEvent,
};

/// The terminal UI: a list of units, and the keys that act on the selected one.
///
/// Generic over its [`UiClient`] rather than holding a `dyn` one, so the
/// in-process path stays direct calls. The day `tush attach` needs to choose
/// an implementation at runtime, the choice is one match in `main` over which
/// `Ui<_>` to run — not a change here.
///
/// # What it keeps
///
/// The loop, the keys, and the two things they act on: the client, and the
/// screen. Nothing about the units is copied here — they belong to the client
/// — and nothing about the drawing is either, which is [`UiRender`]'s.
pub struct Ui<C: UiClient> {
    /// Where the units are read from and where the commands go.
    client: C,
    /// Everything about turning that into a screen: the cached list, the
    /// cursor, the log scroll, and how big the log pane came out.
    render: UiRender,
    /// Cleared by <kbd>q</kbd>, which is the only way out.
    running: bool,
}

impl<C: UiClient> Ui<C> {
    /// Take over the terminal, run until the user quits, and give it back.
    ///
    /// `refresh` is how long to wait between frames when nothing is being
    /// pressed. A run changes state without anybody asking — a process exits,
    /// a line is half written — and with a [`UiClient`] that cannot push, the
    /// only way to find out is to look.
    ///
    /// The terminal is restored whatever the loop did — including on the
    /// error path, which is the reason for the temporary rather than a `?` on
    /// the loop. A failure that leaves the terminal in raw mode with no
    /// cursor is a failure you cannot read the message of.
    pub async fn run(client: C, refresh: Duration) -> Result<()> {
        let mut ui = Self {
            client,
            render: UiRender::new(),
            running: true,
        };
        let mut terminal = ratatui::init();
        let result = ui.main_loop(&mut terminal, refresh).await;
        ratatui::restore();
        result
    }

    /// Ask, sync, draw, wait for whichever comes first — a key or the next
    /// tick — and repeat.
    ///
    /// Syncing at the top of the loop rather than after handling a key is
    /// what makes a command's effect visible: `send` returns before the
    /// session has acted on it, so the frame that shows the result is the
    /// next one through here, not the one the key press was in.
    async fn main_loop(&mut self, terminal: &mut DefaultTerminal, refresh: Duration) -> Result<()> {
        let mut events = EventStream::new();
        let mut ticks = tokio::time::interval(refresh);

        while self.running {
            // Ask before syncing, so that a client with a round trip to make
            // has been told about a scroll before it is asked what it has.
            self.request_log();
            self.client.sync();
            self.clamp_log();
            terminal.draw(|frame| self.render.draw(frame, &self.client))?;
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

    /// Tell the client which log the pane shows, and which rectangle of it.
    fn request_log(&mut self) {
        // Cloned to end the borrow on the client before it is handed a `&mut`
        // of itself.
        let key = self.selected().map(|unit| unit.key.clone());
        self.client.set_log(key, self.render.log_region());
    }

    /// Pull the log scroll back to what the client actually found.
    ///
    /// The client never says how much history it has; it says what it found,
    /// and coming back with fewer lines than were asked for is how a view
    /// learns it reached the top.
    fn clamp_log(&mut self) {
        let Some(log) = self.client.log() else {
            return;
        };
        let held = log.region.line_start + log.lines.len();
        self.render.clamp_log(held);
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
            KeyCode::Down | KeyCode::Char('j') => self.select(Move::Next),
            KeyCode::Up | KeyCode::Char('k') => self.select(Move::Previous),
            KeyCode::Home | KeyCode::Char('g') => self.select(Move::First),
            KeyCode::End | KeyCode::Char('G') => self.select(Move::Last),
            KeyCode::PageUp => self.render.scroll_log(1),
            KeyCode::PageDown => self.render.scroll_log(-1),
            KeyCode::Enter => self.send(|key| UiCommand::Dispatch {
                key,
                event: UnitEvent::Default,
            }),
            KeyCode::Backspace => self.send(|key| UiCommand::Stop { key }),
            _ => {}
        }
    }

    /// Move the cursor, bounded by however many units there are.
    fn select(&mut self, movement: Move) {
        self.render.select(movement, self.client.units().len());
    }

    /// Send the command `command` builds for the selected unit's key, if
    /// there is one selected.
    fn send(&mut self, command: impl FnOnce(arcstr::ArcStr) -> UiCommand) {
        let Some(unit) = self.selected() else {
            return;
        };
        self.client.send(command(unit.key.clone()));
    }

    /// The unit under the cursor, if the list is not empty.
    fn selected(&self) -> Option<&UiUnit> {
        self.render.selected_unit(&self.client)
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
}
