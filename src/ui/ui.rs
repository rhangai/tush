use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{DefaultTerminal, widgets::ListState};
use tokio_stream::StreamExt;

use crate::{
    log::LogRegion,
    ui::{
        client::{UiClient, UiCommand, UiLog, UiUnit},
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

/// How many lines beyond the pane are asked for, on each side of it.
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
    /// How many rows and columns of text the log pane had in the last frame.
    ///
    /// Measured while rendering and used by the *next* frame's request, since
    /// the size of a pane is not known until the layout that makes it. A
    /// frame of lag, and invisible: it sizes a region that already has
    /// [`LOG_MARGIN`] lines of slack either way, so a pane that just grew is
    /// still covered by what was fetched for the old one.
    log_size: (usize, usize),
    /// How many lines back from the newest the log pane is showing.
    ///
    /// Zero follows the end of the log. Reset whenever the selection moves,
    /// because it is a position in one unit's output and means nothing in
    /// another's.
    log_scroll: usize,
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
            log_size: (0, 0),
            log_scroll: 0,
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
            // Ask before syncing, so that a client with a round trip to make
            // has been told about a scroll before it is asked what it has.
            self.request_log();
            self.client.sync();
            self.clamp_log();
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

    /// Tell the client which log the pane shows, and which rectangle of it.
    ///
    /// The rectangle is the pane plus [`LOG_MARGIN`] lines either side, so
    /// the lines a scroll is about to need are asked for before the key that
    /// needs them is pressed — and clipped to the pane's width, because a
    /// column nobody can see is bytes nobody needed.
    fn request_log(&mut self) {
        // Cloned to end the borrow on the client before it is handed a `&mut`
        // of itself.
        let key = self.selected().map(|unit| unit.key.clone());
        let (rows, columns) = self.log_size;
        let start = self.log_scroll.saturating_sub(LOG_MARGIN);
        let region = LogRegion::new(start..self.log_scroll + rows + LOG_MARGIN, 0..columns);
        self.client.set_log(key.as_deref(), region);
    }

    /// Pull the log scroll back to what there is to show.
    ///
    /// The client never says how much history it has; it says what it found,
    /// and coming back with fewer lines than were asked for is how a view
    /// learns it reached the top.
    ///
    /// The limit is where the oldest line reaches the top of the pane, not
    /// the bottom: past that the window hangs off the end of the history and
    /// every further line of scroll buys a blank row. A log shorter than the
    /// pane therefore does not scroll at all.
    fn clamp_log(&mut self) {
        let Some(log) = self.client.log() else {
            return;
        };
        let held = log.region.line_start + log.lines.len();
        let rows = self.log_size.0;
        self.log_scroll = self.log_scroll.min(held.saturating_sub(rows));
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
            KeyCode::Down | KeyCode::Char('j') => self.select(ListState::select_next),
            KeyCode::Up | KeyCode::Char('k') => self.select(ListState::select_previous),
            KeyCode::Home | KeyCode::Char('g') => self.select(ListState::select_first),
            KeyCode::End | KeyCode::Char('G') => self.select(ListState::select_last),
            KeyCode::PageUp => self.log_scroll = self.log_scroll.saturating_add(self.page()),
            KeyCode::PageDown => self.log_scroll = self.log_scroll.saturating_sub(self.page()),
            KeyCode::Enter => self.send(|key| UiCommand::Dispatch {
                key,
                event: UnitEvent::Default,
            }),
            KeyCode::Backspace => self.send(|key| UiCommand::Stop { key }),
            _ => {}
        }
    }

    /// Move the cursor, and put the log pane back at the end.
    ///
    /// The scroll is a position in one unit's output. Carrying it over would
    /// land at an offset that means nothing in the next one — and a pane that
    /// opens in the middle of a log, for no reason the user can see, reads as
    /// output having gone missing.
    fn select(&mut self, movement: impl FnOnce(&mut ListState)) {
        movement(&mut self.list);
        self.clamp();
        self.log_scroll = 0;
    }

    /// How far one press scrolls the log: a pane, less a line of overlap.
    ///
    /// The overlap is what makes a page turn readable — a line you have just
    /// read stays on screen to land on.
    fn page(&self) -> usize {
        self.log_size.0.saturating_sub(1).max(1)
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
    pub(super) fn selected(&self) -> Option<&UiUnit> {
        self.client.units().get(self.list.selected()?)
    }

    /// The lines the log pane should draw, as of the last sync.
    pub(super) fn log(&self) -> Option<UiLog<'_>> {
        self.client.log()
    }

    /// How many lines back from the newest the pane is showing.
    pub(super) fn log_scroll(&self) -> usize {
        self.log_scroll
    }

    /// Record how big the log pane turned out, for the next frame's request.
    pub(super) fn set_log_size(&mut self, rows: usize, columns: usize) {
        self.log_size = (rows, columns);
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
