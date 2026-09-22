use std::time::Duration;

use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
};
use ratatui::DefaultTerminal;
use tokio_stream::StreamExt;

use crate::{
    app::AppUnitKey,
    error::UiError,
    ui::{
        render::{Move, UiMenuChoice, UiRender},
        theme::UiTheme,
    },
    view::{ViewClient, ViewCommand, ViewUnit},
};

/// How many lines one notch of the wheel moves the log: three, which is what
/// a terminal scrolls by and so what a hand expects.
const WHEEL_LINES: isize = 3;

/// The terminal UI: the redraw loop, the keys, and the two things they act
/// on — the client and the screen.
///
/// Generic over its [`ViewClient`] and not `dyn`, so the in-process path stays
/// direct calls. When `tush attach` has to choose at runtime, the choice is
/// one match in `main` over which `Ui<_>` to run.
pub struct Ui<C: ViewClient> {
    /// Where the units are read from and where the commands go.
    client: C,
    /// Everything about turning that into a screen: the cursor, the scroll,
    /// the open menu, and how big the panes came out.
    render: UiRender,
    /// Cleared by <kbd>q</kbd>, <kbd>Esc</kbd> or <kbd>Ctrl-C</kbd>.
    running: bool,
}

impl<C: ViewClient> Ui<C> {
    /// Take over the terminal, run until the user quits, and give it back.
    ///
    /// `refresh` is how long to wait between frames when nothing is pressed:
    /// a run changes state without anybody asking, and a [`ViewClient`] cannot
    /// push, so the only way to find out is to look.
    ///
    /// `theme` is taken here rather than read here: this is the screen, and
    /// where the values come from is the caller's business.
    ///
    /// The terminal is restored whatever the loop did, error path included —
    /// which is why the result is held rather than `?`-ed. A failure that
    /// leaves raw mode on is a failure you cannot read the message of.
    pub async fn run(client: C, refresh: Duration, theme: UiTheme) -> Result<(), UiError> {
        let mut ui = Self {
            client,
            render: UiRender::new(theme),
            running: true,
        };
        let mut terminal = ratatui::init();
        // The wheel is not reported unless asked for, and asking costs the
        // terminal's own selection: dragging over the log no longer selects
        // it, and copying a line takes the terminal's override, `Shift` in
        // nearly all of them.
        let mouse = execute!(std::io::stdout(), EnableMouseCapture);
        let result = ui.main_loop(&mut terminal, refresh).await;
        if mouse.is_ok() {
            let _ = execute!(std::io::stdout(), DisableMouseCapture);
        }
        ratatui::restore();
        result
    }

    /// Ask, sync, draw, wait for whichever comes first — a key or the next
    /// tick — and repeat.
    ///
    /// Syncing at the top rather than after a key is what makes a command's
    /// effect visible: `send` returns before the session has acted on it, so
    /// the frame that shows the result is the next one through here.
    async fn main_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        refresh: Duration,
    ) -> Result<(), UiError> {
        let mut events = EventStream::new();
        let mut ticks = tokio::time::interval(refresh);

        while self.running {
            // Ask before syncing, so that a client with a round trip to make
            // has been told about a scroll before it is asked what it has.
            self.request_log();
            self.client.sync();
            self.refresh_menu();
            self.clamp_log();
            terminal
                .draw(|frame| self.render.draw(frame, &self.client))
                .map_err(UiError::DrawError)?;
            tokio::select! {
                _ = ticks.tick() => {}
                event = events.next() => match event {
                    Some(event) => self.handle(event.map_err(UiError::EventError)?),
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
        // Copied out to end the borrow on the client before it is handed a
        // `&mut` of itself. The flag is the unit's and the rectangle is the
        // pane's, and this is where the two meet.
        let unit = self.selected();
        let key = unit.map(|unit| unit.unit_key);
        let parse_ansi = unit.is_none_or(|unit| unit.parse_ansi);
        let region = self.render.log_region().with_parse_ansi(parse_ansi);
        self.client.set_log(key, region);
    }

    /// Pull the log scroll back to what the client actually found.
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
        let key = match event {
            Event::Key(key) => key,
            Event::Mouse(mouse) => return self.handle_mouse(mouse),
            _ => return,
        };
        if key.kind != KeyEventKind::Press {
            return;
        }
        // Before anything else, the menu included.
        if Self::is_interrupt(&key) {
            self.running = false;
            return;
        }
        // The menu takes every other key while it is up: `q` closes it
        // rather than quitting, `j` moves within it rather than under it.
        if self.render.menu_open() {
            return self.handle_menu(key);
        }
        if Self::is_quit(&key) {
            self.running = false;
            return;
        }
        // Shift with an arrow scrolls the log a line at a time, which is the
        // fine adjustment a page is too coarse for and the wheel is the mouse
        // version of.
        let fine = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Up if fine => self.render.scroll_log_lines(1),
            KeyCode::Down if fine => self.render.scroll_log_lines(-1),
            KeyCode::Down | KeyCode::Char('j') => self.select(Move::Next),
            KeyCode::Up | KeyCode::Char('k') => self.select(Move::Previous),
            KeyCode::PageUp => self.render.scroll_log_pages(1),
            KeyCode::PageDown => self.render.scroll_log_pages(-1),
            // `G` for the same reason vi has it: the end of the thing you
            // are reading. It is free now that it no longer moves the list —
            // which was the wrong thing for it to move.
            KeyCode::End | KeyCode::Char('G') => self.render.follow_log(),
            // The two lists are two loops, so this is the only way between
            // them — see `UiRenderUnitsState::focus_other`.
            KeyCode::Tab => self.render.focus_other(self.client.units()),
            KeyCode::Enter => self.open_menu(),
            // The accelerator for the entry the menu opens on: start, or
            // restart in the mode it is already in.
            KeyCode::Char('r' | 'R') => self.send(|key| ViewCommand::Start { key }),
            // Both, because they are one key to a hand: whichever of them the
            // keyboard put under the finger that means "get rid of this".
            KeyCode::Backspace | KeyCode::Delete => self.send(|key| ViewCommand::Stop { key }),
            _ => {}
        }
    }

    /// Act on one key while the menu has them.
    ///
    /// A chosen entry is sent and the menu closes: left open, the cursor
    /// would sit on a label the press itself just made wrong.
    fn handle_menu(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.render.close_menu(),
            KeyCode::Down | KeyCode::Char('j') => self.render.select_menu(Move::Next),
            KeyCode::Up | KeyCode::Char('k') => self.render.select_menu(Move::Previous),
            KeyCode::Enter => match self.render.menu_choice() {
                UiMenuChoice::Send { unit, event } => {
                    self.render.close_menu();
                    self.client.send(ViewCommand::Dispatch { key: unit, event });
                }
                UiMenuChoice::Cancel => self.render.close_menu(),
            },
            _ => {}
        }
    }

    /// Read the open menu's entries again, so the verbs say what the unit is
    /// doing now and not what it was doing when it opened.
    ///
    /// After the sync, so the entries and the rows behind them are taken from
    /// the same moment. The buffer goes out and comes back as it does at
    /// open — a borrow of the screen held across a read of the session is the
    /// one shape the borrow checker will not have.
    fn refresh_menu(&mut self) {
        let Some(key) = self.render.menu_key() else {
            return;
        };
        let mut items = self.render.take_menu_items();
        self.client.choices(key, &mut items);
        self.render.refresh_menu(items);
    }

    /// Open the action menu over the selected unit.
    ///
    /// The buffer goes out to the client and comes back rather than being
    /// filled through a `&mut`: a borrow of the screen held across a read of
    /// the session is the one shape the borrow checker will not have. It
    /// keeps its capacity, so a second open allocates nothing.
    fn open_menu(&mut self) {
        let Some(unit) = self.selected() else {
            return;
        };
        let key = unit.unit_key;
        let title = unit.name.clone();
        let mut items = self.render.take_menu_items();
        self.client.choices(key, &mut items);
        self.render.open_menu(key, title, items);
    }

    /// Act on the wheel: it scrolls the log wherever the pointer is, that
    /// being the only thing on screen with more in it than fits.
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.render.scroll_log_lines(WHEEL_LINES),
            MouseEventKind::ScrollDown => self.render.scroll_log_lines(-WHEEL_LINES),
            _ => {}
        }
    }

    /// Move the cursor, bounded by however many units there are.
    fn select(&mut self, movement: Move) {
        self.render.select(movement, self.client.units().len());
    }

    /// Send the command `command` builds for the selected unit's key, if
    /// there is one selected.
    fn send(&mut self, command: impl FnOnce(AppUnitKey) -> ViewCommand) {
        let Some(unit) = self.selected() else {
            return;
        };
        self.client.send(command(unit.unit_key));
    }

    /// The unit under the cursor, if the list is not empty.
    fn selected(&self) -> Option<&ViewUnit> {
        self.render.selected_unit(&self.client)
    }

    /// <kbd>Ctrl-C</kbd>, which quits from anywhere.
    ///
    /// Apart from [`is_quit`](Self::is_quit) because the menu does not get to
    /// take it: raw mode means nothing else turns this into a signal.
    fn is_interrupt(key: &KeyEvent) -> bool {
        key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
    }

    /// <kbd>q</kbd> or <kbd>Esc</kbd>, which quit only from the list.
    fn is_quit(key: &KeyEvent) -> bool {
        matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
    }
}
