use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
};
use ratatui::DefaultTerminal;
use tokio_stream::StreamExt;

use crate::ui::{
    client::{UiClient, UiCommand, UiUnit},
    render::{Move, UiMenuChoice, UiRender},
};

/// How many lines one notch of the wheel moves the log.
///
/// Three, which is what a terminal scrolls by and therefore what a hand
/// expects from one.
const WHEEL_LINES: isize = 3;

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
        // The wheel is not reported unless it is asked for. What asking costs
        // is the terminal's own selection: with the mouse captured, dragging
        // over the log no longer selects it, and copying out a line takes
        // whatever the terminal's override is — `Shift` in nearly all of
        // them.
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
        let key = match event {
            Event::Key(key) => key,
            Event::Mouse(mouse) => return self.handle_mouse(mouse),
            _ => return,
        };
        if key.kind != KeyEventKind::Press {
            return;
        }
        // Before anything else, the menu included: raw mode means the
        // terminal no longer turns this into a signal, so if the UI does not
        // quit on it, nothing does.
        if Self::is_interrupt(&key) {
            self.running = false;
            return;
        }
        // The menu takes every other key while it is up, which is what makes
        // it one: `q` closes it rather than quitting the session, and `j`
        // moves within it rather than under it.
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
            KeyCode::Enter => self.open_menu(),
            // The accelerator for the entry the menu would open on: start it,
            // or restart it in the mode it is already in. Not a dispatch,
            // because it asks for no mode to change — `Start` sees the old
            // run out before the new one begins, which is the whole of it.
            KeyCode::Char('r' | 'R') => self.send(|key| UiCommand::Start { key }),
            // Both, because they are one key to a hand: whichever of them the
            // keyboard put under the finger that means "get rid of this".
            KeyCode::Backspace | KeyCode::Delete => self.send(|key| UiCommand::Stop { key }),
            _ => {}
        }
    }

    /// Act on one key while the menu has them.
    ///
    /// A chosen entry is sent and the menu closes. Leaving it open would
    /// leave the cursor on a label that the press itself just made wrong —
    /// `Start Watch` becomes the restart of a run that is now under way.
    ///
    /// <kbd>Esc</kbd> and <kbd>q</kbd> are the same answer as the `Cancel`
    /// row, which is there so that this paragraph is not the only place the
    /// way out is written down.
    fn handle_menu(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.render.close_menu(),
            KeyCode::Down | KeyCode::Char('j') => self.render.select_menu(Move::Next),
            KeyCode::Up | KeyCode::Char('k') => self.render.select_menu(Move::Previous),
            KeyCode::Enter => match self.render.menu_choice() {
                UiMenuChoice::Send { key: unit, event } => {
                    self.render.close_menu();
                    self.client.send(UiCommand::Dispatch { key: unit, event });
                }
                UiMenuChoice::Cancel => self.render.close_menu(),
            },
            _ => {}
        }
    }

    /// Open the action menu over the selected unit.
    ///
    /// The entries are read out of the client once, here, and then held
    /// still: a menu is a question about a moment, and one that rebuilt
    /// itself per frame would renumber its entries when a process exited —
    /// moving the one under the cursor between a finger going down and coming
    /// up.
    ///
    /// The buffer goes out to the client and comes back rather than being
    /// filled through a `&mut`, because filling it in place means holding a
    /// borrow of the screen across a read of the session, which is the one
    /// shape the borrow checker will not have. It keeps its capacity either
    /// way, so opening a menu again allocates nothing.
    fn open_menu(&mut self) {
        let Some(unit) = self.selected() else {
            return;
        };
        let (key, title) = (unit.key.clone(), unit.name.clone());
        let mut items = self.render.take_menu_items();
        self.client.choices(&key, &mut items);
        self.render.open_menu(key, title, items);
    }

    /// Act on the wheel.
    ///
    /// It scrolls the log wherever the pointer is. The list is four rows of
    /// names that all fit; the log is the thing with more in it than the
    /// screen, so it is the thing a wheel is for.
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

    /// <kbd>Ctrl-C</kbd>, which quits from anywhere.
    ///
    /// Apart from [`is_quit`](Self::is_quit) because it is the one key the
    /// menu does not get to take: a popup that could swallow the only way out
    /// of a raw mode terminal is a popup that can strand you in one.
    fn is_interrupt(key: &KeyEvent) -> bool {
        key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
    }

    /// <kbd>q</kbd> or <kbd>Esc</kbd>, which quit only from the list.
    fn is_quit(key: &KeyEvent) -> bool {
        matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
    }
}
