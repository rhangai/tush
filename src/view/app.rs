use std::sync::Arc;

use crate::{
    app::{App, AppUnitKey},
    log::{LogLine, LogReader, LogRegion},
    runner::RunnerState,
    unit::UnitChoice,
    view::client::{ViewClient, ViewCommand, ViewLog, ViewSettings, ViewUnit},
};

/// The one log a [`ViewApp`] is following, and the last rectangle read out of it.
struct AppLog {
    /// Which unit it belongs to. A pane moving to another one starts this
    /// again rather than carrying it across: everything counted here is
    /// counted against one log's chunks and means nothing against another's.
    unit_key: AppUnitKey,
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
    lines: Vec<LogLine>,
}

/// A [`ViewClient`] over a session running in this process.
///
/// Trivial, and worth staying that way: it is how you can tell the trait was
/// drawn around what a screen needs rather than around what an [`App`]
/// exposes. [`sync`](ViewClient::sync) reads a map of atomics and
/// [`send`](ViewClient::send) is a direct call — the socket client fits through
/// the same methods with real work to do in all of them.
pub struct ViewApp {
    app: Arc<App>,
    /// The rows, built once and then written over in place.
    units: Vec<ViewUnit>,
    /// The one reader, moved from log to log as the selection changes.
    ///
    /// Built once because a reader is a mirror of a whole log: one per switch
    /// is an allocation and a zeroing of `log_size` bytes every time the
    /// cursor moves, and holding the arrow key means one per frame.
    reader: LogReader,
    /// The log the pane is showing, if it is showing one.
    log: Option<AppLog>,
}

impl ViewApp {
    /// Show `app`.
    ///
    /// The rows are laid out once, by panel and then alphabetically by display
    /// name, and keep that order for the run. The map they come from has no order of its
    /// own, and a list that resequenced itself on every poll would move the
    /// row out from under the cursor. It stands in for the order the config
    /// was written in, which the config does not carry this far yet.
    ///
    /// The names are taken once because a proc is not renamed — unlike the
    /// mode, which is read every sync.
    pub fn new(app: Arc<App>) -> Self {
        let unit_map = app.unit_map();
        let mut list: Vec<ViewUnit> = unit_map
            .keys()
            .filter_map(|unit_key| {
                let entry = unit_map.entry(unit_key).ok()?;
                let unit = entry.unit();
                let settings = entry.settings();
                Some(ViewUnit {
                    key: entry.key().clone(),
                    name: unit.name(),
                    name_short: unit.name_short(),
                    unit_key,
                    mode: None,
                    mode_short: None,
                    state: RunnerState::Stopped,
                    parse_ansi: settings.parse_ansi,
                    panel: settings.panel,
                })
            })
            .collect();
        list.sort_by(|a, b| (a.panel, &a.name).cmp(&(b.panel, &b.name)));
        Self {
            reader: LogReader::empty(unit_map.log_capacity()),
            app,
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
        self.reader.sync();
        let revision = self.reader.version();
        if revision == log.revision && log.region == log.wanted {
            return;
        }

        self.reader.copy_region(log.wanted, &mut log.lines);
        log.region = log.wanted;
        log.revision = revision;
    }
}

impl ViewClient for ViewApp {
    /// Re-read every state into the rows that are already there.
    ///
    /// The `Err` arm is unreachable — the keys came out of the map and the
    /// map cannot lose one — but skipping the row is cheaper than proving it.
    fn sync(&mut self) {
        let unit_map = self.app.unit_map();
        for view in &mut self.units {
            let Ok(unit) = unit_map.entry(view.unit_key).map(|entry| entry.unit()) else {
                continue;
            };
            view.state = unit.state();
            view.mode = unit.mode();
            view.mode_short = unit.mode_short();
        }
        self.sync_log();
    }

    fn units(&self) -> &[ViewUnit] {
        &self.units
    }

    /// Straight off the session, which is where the config was read.
    fn settings(&self) -> ViewSettings {
        ViewSettings {
            colors: self.app.ui().colors,
        }
    }

    /// Point the reader at `key`, and remember the rectangle wanted from it.
    ///
    /// The reader moves only when the unit changed, and moving it is a reset
    /// rather than a build — its memory is the pane's for the run. Resolving
    /// the region is [`sync`](ViewClient::sync)'s job, so the lines and the
    /// states in one frame are taken at the same moment.
    fn set_log(&mut self, key: Option<AppUnitKey>, region: LogRegion) {
        let Some(key) = key else {
            self.log = None;
            return;
        };
        match &mut self.log {
            Some(log) if log.unit_key == key => log.wanted = region,
            _ => {
                let Ok(entry) = self.app.unit_map().entry(key) else {
                    self.log = None;
                    return;
                };
                entry.unit().log_reader_into(&mut self.reader);
                self.log = Some(AppLog {
                    unit_key: key,
                    region,
                    wanted: region,
                    revision: 0,
                    lines: Vec::new(),
                });
            }
        }
    }

    fn log(&self) -> Option<ViewLog<'_>> {
        let log = self.log.as_ref()?;
        Some(ViewLog {
            region: log.region,
            revision: log.revision,
            lines: &log.lines,
        })
    }

    /// Straight out of the unit, which holds the list this asks for.
    ///
    /// The `Err` arm is a key the map does not hold, and the keys came out of
    /// the map — so it leaves `out` empty rather than failing.
    fn choices(&self, key: AppUnitKey, out: &mut Vec<UnitChoice>) {
        let Ok(entry) = self.app.unit_map().entry(key) else {
            out.clear();
            return;
        };
        entry.unit().choices(out);
    }

    /// Do it, and drop whatever it had to say about it.
    ///
    /// Fire and forget is the contract, so nothing comes back — hiding
    /// nothing, since the only failure is a key the map does not hold and the
    /// keys came out of the map. A start is scheduled rather than performed:
    /// it waits on what the unit depends on, which is not a wait a screen can
    /// make. When the session gets a channel to report back through, this is
    /// where it is written to.
    fn send(&self, command: ViewCommand) {
        match command {
            ViewCommand::Start { key } => {
                self.app.schedule(key);
            }
            ViewCommand::Stop { key } => {
                self.app.stop(key);
            }
            ViewCommand::Dispatch { key, event } => {
                _ = self.app.dispatch(key, event);
            }
        };
    }
}
