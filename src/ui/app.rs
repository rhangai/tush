use std::sync::Arc;

use crate::{
    app::App,
    runner::RunnerState,
    ui::client::{UiClient, UiCommand, UiUnit},
};

/// A [`UiClient`] over a session running in this process.
///
/// The trivial implementation, and it is worth that it stays trivial: it is
/// how you can tell the trait was drawn around what a screen needs rather
/// than around what an [`App`] happens to expose. The socket client has to
/// fit through the same three methods, and it will have real work to do in
/// all of them.
///
/// Here there is none. [`sync`](UiClient::sync) reads a `HashMap` of atomics,
/// and [`send`](UiClient::send) is a direct call — so the row of states is
/// only ever one `sync` old, which is as fresh as anything in this file gets.
pub struct UiApp {
    app: Arc<App>,
    /// The rows, built once and then written over in place.
    units: Vec<UiUnit>,
}

impl UiApp {
    /// Show `app`.
    ///
    /// The rows are laid out here, in the order they will keep for the rest
    /// of the run: alphabetically by display name, because the map they come out of has no
    /// order of its own and an arbitrary one would put the list in a
    /// different sequence on every poll — the row under the cursor would stop
    /// being the row the user aimed at. It stands in for the order the config
    /// was written in, which is what a person would expect and which the
    /// config does not carry this far yet.
    ///
    /// Every row starts [`Stopped`](RunnerState::Stopped), which is also what
    /// a session that has not been started reports, and the first
    /// [`sync`](UiClient::sync) replaces them all anyway.
    ///
    /// The display name is taken once and kept, because a proc does not get
    /// renamed — unlike the mode, which is read on every sync. Falling back
    /// to the key is what the config itself does for a proc that gave no
    /// name, so the unreachable error arm lands on the right answer anyway.
    pub fn new(app: Arc<App>) -> Self {
        let units = app.units();
        let mut list: Vec<UiUnit> = units
            .keys()
            .map(|key| UiUnit {
                name: units.name(key).unwrap_or_else(|_| key.to_owned()),
                key: key.to_owned(),
                mode: None,
                state: RunnerState::Stopped,
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            app: app.clone(),
            units: list,
        }
    }
}

impl UiClient for UiApp {
    /// Re-read every state into the rows that are already there.
    ///
    /// Nothing is allocated and nothing can fail: the keys were taken from
    /// the map itself and the map cannot lose one, so the `Err` arm is
    /// unreachable rather than tolerated — but it is cheaper to skip the row
    /// than to prove that here.
    fn sync(&mut self) {
        let units = self.app.units();
        for unit in &mut self.units {
            if let Ok(state) = units.state(&unit.key) {
                unit.state = state;
            }
            if let Ok(mode) = units.mode(&unit.key) {
                unit.mode = mode;
            }
        }
    }

    fn units(&self) -> &[UiUnit] {
        &self.units
    }

    /// Do it, and drop whatever it had to say about it.
    ///
    /// Fire and forget is the contract, so the `Result` dies here. It is not
    /// covering anything up yet: the only failure these three have is a name
    /// the map does not hold, and the names came out of the map. When there
    /// is a channel for the session to report back through, this is where it
    /// gets written to.
    fn send(&self, command: UiCommand) {
        let units = self.app.units();
        let _ = match command {
            UiCommand::Start { key } => units.start(&key).map(|_| ()),
            UiCommand::Stop { key } => units.stop(&key),
            UiCommand::Dispatch { key, event } => self.app.dispatch(&key, event).map(|_| ()),
        };
    }
}
