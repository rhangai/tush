use std::sync::Arc;

use crate::{
    app::App,
    log::{LogReader, LogRegion},
    runner::RunnerState,
    ui::client::{UiClient, UiCommand, UiLog, UiUnit},
    unit::{UnitChoice, UnitKey},
};

/// The one log a [`UiApp`] is following, and the last rectangle read out of it.
struct AppLog {
    /// Which unit it belongs to. A pane moving to another one throws this
    /// away rather than re-pointing it: the revision counted against one
    /// log's chunks means nothing against another's.
    unit_key: UnitKey,
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
    /// The names are taken once because a proc is not renamed — unlike the
    /// mode, which is read every sync.
    pub fn new(app: Arc<App>) -> Self {
        let unit_map = app.unit_map();
        let mut list: Vec<UiUnit> = unit_map
            .keys()
            .map(|unit_key| UiUnit {
                name: unit_map.name(unit_key).unwrap_or_default(),
                name_short: unit_map.name_short(unit_key).unwrap_or(None),
                unit_key,
                mode: None,
                mode_short: None,
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
        let unit_map = self.app.unit_map();
        for unit in &mut self.units {
            if let Ok(state) = unit_map.state(unit.unit_key) {
                unit.state = state;
            }
            if let Ok(mode) = unit_map.mode(unit.unit_key) {
                unit.mode = mode;
            }
            if let Ok(short) = unit_map.mode_short(unit.unit_key) {
                unit.mode_short = short;
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
    fn set_log(&mut self, key: Option<UnitKey>, region: LogRegion) {
        let Some(key) = key else {
            self.log = None;
            return;
        };
        match &mut self.log {
            Some(log) if log.unit_key == key => log.wanted = region,
            _ => {
                let Some(reader) = self.app.unit_map().log_reader(key) else {
                    self.log = None;
                    return;
                };
                self.log = Some(AppLog {
                    unit_key: key,
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
    /// The `Err` arm is a key the map does not hold, and the keys came out of
    /// the map — so it leaves `out` empty rather than failing.
    fn choices(&self, key: UnitKey, out: &mut Vec<UnitChoice>) {
        let unit_map = self.app.unit_map();
        if unit_map.choices(key, out).is_err() {
            out.clear();
        }
    }

    /// Do it, and drop whatever it had to say about it.
    ///
    /// Fire and forget is the contract, so nothing comes back — hiding
    /// nothing, since the only failure is a key the map does not hold and the
    /// keys came out of the map. A start is scheduled rather than performed:
    /// it waits on what the unit depends on, which is not a wait a screen can
    /// make. When the session gets a channel to report back through, this is
    /// where it is written to.
    fn send(&self, command: UiCommand) {
        match command {
            UiCommand::Start { key } => {
                self.app.schedule(key);
            }
            UiCommand::Stop { key } => {
                self.app.stop(key);
            }
            UiCommand::Dispatch { key, event } => {
                _ = self.app.dispatch(key, event);
            }
        };
    }
}
