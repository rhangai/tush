use std::{env, io, process::Stdio, time::Duration};

use crossterm::{
    clipboard::CopyToClipboard,
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
};
use ratatui::DefaultTerminal;
use tokio::{
    io::AsyncWriteExt,
    process::{Child, Command},
};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::{
    app::AppUnitKey,
    error::UiError,
    log::LogRegion,
    ui::{
        render::{Move, UiMenuChoice, UiRender},
        theme::UiTheme,
    },
    view::{ViewClient, ViewCommand, ViewUnit},
};

/// How many lines one notch of the wheel moves the log: three, which is what
/// a terminal scrolls by and so what a hand expects.
const WHEEL_LINES: isize = 3;

/// How long a clipboard tool gets to take the text before it is killed.
///
/// Taking it is a write to a pipe and a fork, over in milliseconds; one that
/// is still going after this is waiting on a display that is not there.
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);

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
    /// The last thing [`request_log`](Ui::request_log) told the client, and
    /// `None` before it has told it anything.
    asked: Option<(Option<AppUnitKey>, LogRegion)>,
    /// The text of the last selection copied, kept for its capacity.
    copy: String,
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
    ///
    /// `cancel` is the other way out, and it has to come through the loop
    /// rather than end the process: everything that gives the terminal back
    /// is below this line, and a signal that kills us here leaves a shell in
    /// raw mode looking at the alternate screen.
    pub async fn run(
        client: C,
        refresh: Duration,
        theme: UiTheme,
        cancel: CancellationToken,
    ) -> Result<(), UiError> {
        let mut ui = Self {
            client,
            render: UiRender::new(theme),
            running: true,
            asked: None,
            copy: String::new(),
        };
        let mut terminal = ratatui::init();
        // The wheel is not reported unless asked for, and asking costs the
        // terminal's own selection — which is why the log pane selects its
        // own text. The terminal's still works behind its override, `Shift`
        // in nearly all of them.
        let mouse = execute!(std::io::stdout(), EnableMouseCapture);
        let result = ui.main_loop(&mut terminal, refresh, cancel).await;
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
        cancel: CancellationToken,
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
                // Whoever wants us gone is waiting on the process, so the
                // frame in flight is not worth finishing.
                _ = cancel.cancelled() => break,
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

    /// Tell the client which log the pane shows, and which rectangle of it —
    /// when either of them has changed.
    ///
    /// Said once per change and not once per frame. The client is free to do
    /// nothing with a repeat, and both of them do, but the pane sits on the
    /// same unit and the same rectangle for as long as nobody touches the
    /// keyboard: at ten frames a second that is the same sentence said ten
    /// times, down a socket, to be answered the same way.
    fn request_log(&mut self) {
        // Copied out to end the borrow on the client before it is handed a
        // `&mut` of itself.
        let key = self.selected().map(|unit| unit.unit_key);
        let region = self.render.log_region();
        if self.asked == Some((key, region)) {
            return;
        }
        self.asked = Some((key, region));
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
            // The selection is cells of the pane as it was laid out, which a
            // resize moves.
            Event::Resize(..) => return self.render.clear_log_selection(),
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

    /// Act on the mouse: the wheel scrolls the log wherever the pointer is,
    /// that being the only thing on screen with more in it than fits, and the
    /// left button moves the cursor to a unit or selects the log's text.
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let (x, y) = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp => self.render.scroll_log_lines(WHEEL_LINES),
            MouseEventKind::ScrollDown => self.render.scroll_log_lines(-WHEEL_LINES),
            MouseEventKind::Moved => self.render.hover(x, y),
            // The menu sits over everything, so a press is the menu's: it
            // never reaches the list or the log behind it.
            MouseEventKind::Down(MouseButton::Left) if self.render.menu_open() => {
                self.render.click_menu(x, y);
            }
            _ if self.render.menu_open() => {}
            // A press selects the unit under it, or starts a selection in the
            // log; either way it clears the selection already up.
            MouseEventKind::Down(MouseButton::Left) => {
                self.render.click_unit(x, y);
                self.render.select_log_from(x, y, &self.client);
            }
            MouseEventKind::Drag(MouseButton::Left) => self.render.select_log_to(x, y),
            MouseEventKind::Up(MouseButton::Left) => self.end_selection(),
            _ => {}
        }
    }

    /// The button came up: hand whatever was selected to the terminal, which
    /// puts it on the clipboard, and to the desktop's clipboard tool as well.
    ///
    /// Both, every time: a terminal that ignores OSC 52 says nothing, so there
    /// is no failure to fall back from, and where both work they write the
    /// same text to the same clipboard. Written straight to stdout, which is
    /// safe here because events are handled between frames and never during
    /// one; a write that failed leaves no screen to say it on.
    fn end_selection(&mut self) {
        if !self.render.select_log_end(&mut self.copy) {
            return;
        }
        let _ = execute!(
            std::io::stdout(),
            CopyToClipboard::to_clipboard_from(&self.copy)
        );
        tokio::spawn(copy_with_tool(self.copy.clone()));
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

/// Hand `text` to the first clipboard tool that fits the session and starts.
///
/// Picked per copy rather than once at startup, so a tool installed while
/// tush runs is found. A tool that fails to start moves on to the next; one
/// that started is the last, whatever it does. Left detached on purpose: the
/// timeout ends it, and `kill_on_drop` takes a hung tool down with it rather
/// than leaving it behind — `wl-copy` and `xclip` fork to keep serving the
/// clipboard, and by then the child started here has already exited.
async fn copy_with_tool(text: String) {
    let wayland = env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = env::var_os("DISPLAY").is_some();
    let tools: [(bool, &str, &[&str]); 4] = [
        (wayland, "wl-copy", &[]),
        (x11, "xclip", &["-selection", "clipboard"]),
        (x11, "xsel", &["--clipboard", "--input"]),
        (cfg!(target_os = "macos"), "pbcopy", &[]),
    ];
    for (applies, program, args) in tools {
        if !applies {
            continue;
        }
        // Nothing of the tool's reaches the terminal: it would draw over
        // the screen.
        let started = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let Ok(child) = started else {
            continue;
        };
        let _ = tokio::time::timeout(CLIPBOARD_TIMEOUT, feed(child, &text)).await;
        return;
    }
}

/// Write `text` to the tool's stdin, close it, and wait for the tool to exit.
///
/// Closing is what tells the tool the text is complete.
async fn feed(mut child: Child, text: &str) -> io::Result<()> {
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).await?;
    }
    child.wait().await?;
    Ok(())
}
