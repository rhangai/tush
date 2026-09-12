use std::sync::Arc;

use crate::{
    app::App,
    log::{LogReader, LogRegion},
    runner::RunnerState,
    ui::client::{UiClient, UiCommand, UiLog, UiUnit},
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
/// The one log a [`UiApp`] is following, and the last rectangle read out of
/// it.
struct AppLog {
    /// Which unit it belongs to. A pane moving to another one throws this
    /// away rather than re-pointing it: a reader is a copy of one log's
    /// chunks, and the revision counted against it means nothing elsewhere.
    key: String,
    /// A reader over that unit's log, which is to say a mirror of it.
    ///
    /// The size is the reader's business and not the pane's — what bounds
    /// what the pane costs is the region, which is a rectangle either way.
    reader: LogReader,
    /// The region last asked for.
    wanted: LogRegion,
    /// The region `lines` actually is.
    ///
    /// The same as `wanted` here, always, because resolving one is a walk
    /// over memory this process already has. Carried anyway because it is
    /// what the trait promises, and a client that has to wait for its answers
    /// is who it is promised for.
    region: LogRegion,
    /// What the log had written when `lines` were taken.
    revision: u64,
    /// The lines of `region`, oldest first. Kept between syncs so the strings
    /// in it are refilled rather than reallocated.
    lines: Vec<String>,
}

pub struct UiApp {
    app: Arc<App>,
    /// The rows, built once and then written over in place.
    units: Vec<UiUnit>,
    /// The log the pane is showing, if it is showing one.
    log: Option<AppLog>,
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
                name: units.name(key).unwrap_or_else(|_| key.into()),
                key: key.to_owned(),
                mode: None,
                state: RunnerState::Stopped,
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            app: app.clone(),
            units: list,
            log: None,
        }
    }

    /// Take in what the followed log has, and cut the wanted rectangle out of
    /// it.
    ///
    /// Skipped entirely when neither the question nor the log has changed —
    /// which is most syncs, since a quiet unit does not move its revision and
    /// a still pane does not move its region. That test is the whole reason
    /// the revision is carried.
    fn sync_log(&mut self) {
        let Some(log) = &mut self.log else {
            return;
        };
        log.reader.sync();
        let revision = log.reader.seen();
        if revision == log.revision && log.region == log.wanted {
            return;
        }

        log.reader.copy_region(log.wanted, &mut log.lines);
        log.region = log.wanted;
        log.revision = revision;
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
        self.sync_log();
    }

    fn units(&self) -> &[UiUnit] {
        &self.units
    }

    /// Point the reader at `key`, and remember the rectangle wanted from it.
    ///
    /// A new reader only when the unit changed. Nothing else happens here —
    /// resolving the region is [`sync`](UiClient::sync)'s job, so that the
    /// lines and the states in one frame are taken at the same moment.
    ///
    /// The revision starts at zero for a new log, which no reader reports
    /// after a sync, so the first one always resolves.
    fn set_log(&mut self, key: Option<&str>, region: LogRegion) {
        let Some(key) = key else {
            self.log = None;
            return;
        };
        match &mut self.log {
            Some(log) if log.key == key => log.wanted = region,
            _ => {
                let Some(reader) = self.app.units().log_reader(key) else {
                    self.log = None;
                    return;
                };
                self.log = Some(AppLog {
                    key: key.to_owned(),
                    reader,
                    region,
                    wanted: region,
                    revision: 0,
                    lines: Vec::new(),
                });
            }
        }
    }

    fn log(&self) -> Option<UiLog<'_>> {
        let log = self.log.as_ref()?;
        Some(UiLog {
            region: log.region,
            revision: log.revision,
            lines: &log.lines,
        })
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
