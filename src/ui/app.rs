use std::sync::Arc;

use arcstr::ArcStr;

use crate::{
    app::App,
    log::{LogReader, LogRegion},
    runner::RunnerState,
    ui::client::{UiClient, UiCommand, UiLog, UiUnit},
    unit::UnitChoice,
};

/// The one log a [`UiApp`] is following, and the last rectangle read out of it.
struct AppLog {
    /// Which unit it belongs to. A pane moving to another one throws this
    /// away rather than re-pointing it: the revision counted against one
    /// log's chunks means nothing against another's.
    key: ArcStr,
    /// A reader over that unit's log, which is to say a mirror of it.
    reader: LogReader,
    /// The region last asked for.
    wanted: LogRegion,
    /// The region `lines` actually is — always `wanted` here, since resolving
    /// one is a walk over memory this process already has. Carried because
    /// the trait promises it for the client that cannot say the same.
    region: LogRegion,
    /// What the log had written when `lines` were taken.
    revision: u64,
    /// The lines of `region`, oldest first. Kept between syncs so the strings
    /// are refilled rather than reallocated.
    lines: Vec<String>,
}

/// A [`UiClient`] over a session running in this process.
///
/// Trivial, and worth staying that way: it is how you can tell the trait was
/// drawn around what a screen needs rather than around what an [`App`]
/// exposes. [`sync`](UiClient::sync) reads a map of atomics and
/// [`send`](UiClient::send) is a direct call — the socket client fits through
/// the same methods with real work to do in all of them.
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
    /// The rows are laid out once, alphabetically by display name, and keep
    /// that order for the run. The map they come from has no order of its
    /// own, and a list that resequenced itself on every poll would move the
    /// row out from under the cursor. It stands in for the order the config
    /// was written in, which the config does not carry this far yet.
    ///
    /// The name is taken once because a proc is not renamed — unlike the
    /// mode, which is read every sync.
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

    /// Take in what the followed log has, and cut the wanted rectangle out.
    ///
    /// Skipped when neither the question nor the log has changed, which is
    /// most syncs — and that test is the whole reason a revision is carried.
    fn sync_log(&mut self) {
        let Some(log) = &mut self.log else {
            return;
        };
        log.reader.sync();
        let revision = log.reader.version();
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
    /// The `Err` arm is unreachable — the keys came out of the map and the
    /// map cannot lose one — but skipping the row is cheaper than proving it.
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
    /// A new reader only when the unit changed. Resolving the region is
    /// [`sync`](UiClient::sync)'s job, so the lines and the states in one
    /// frame are taken at the same moment.
    fn set_log(&mut self, key: Option<ArcStr>, region: LogRegion) {
        let Some(key) = key else {
            self.log = None;
            return;
        };
        match &mut self.log {
            Some(log) if log.key == key => log.wanted = region,
            _ => {
                let Some(reader) = self.app.units().log_reader(key.as_str()) else {
                    self.log = None;
                    return;
                };
                self.log = Some(AppLog {
                    key,
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

    /// Straight out of the unit, which holds the list this asks for.
    ///
    /// The `Err` arm is a name the map does not have, and the names came from
    /// the map — so it leaves `out` empty rather than failing.
    fn choices(&self, key: &str, out: &mut Vec<UnitChoice>) {
        if self.app.units().choices(key, out).is_err() {
            out.clear();
        }
    }

    /// Do it, and drop whatever it had to say about it.
    ///
    /// Fire and forget is the contract, so the `Result` dies here — hiding
    /// nothing, since the only failure is a name the map does not hold and
    /// the names came out of the map. When the session gets a channel to
    /// report back through, this is where it is written to.
    fn send(&self, command: UiCommand) {
        let units = self.app.units();
        let _ = match command {
            UiCommand::Start { key } => units.start(&key).map(|_| ()),
            UiCommand::Stop { key } => units.stop(&key),
            UiCommand::Dispatch { key, event } => self.app.dispatch(&key, event).map(|_| ()),
        };
    }
}
