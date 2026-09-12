use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result};
use arcstr::ArcStr;

use crate::{
    log::LogReader,
    runner::{RunnerHandle, RunnerState},
    unit::{UnitAction, UnitEvent, behavior::UnitBehavior, unit::Unit},
};

/// Every [`Unit`] in a session, by name.
///
/// This is what the CLI, and later the config loader, talk to: units are
/// [`add`](UnitMap::add)ed once and then addressed by name —
/// [`start`](UnitMap::start), [`stop`](UnitMap::stop),
/// [`state`](UnitMap::state).
///
/// # Shared, not owned
///
/// Every method takes `&self`. A map behind an `Arc` can be handed to the
/// input task, the render loop and whatever supervises a start, and all of
/// them can add and address units without any of them owning it — the same
/// bargain a `DashMap` offers, which is what this is shaped after.
///
/// # One lock, not a sharded one
///
/// A real `DashMap` splits the map into shards so that writers to different
/// keys do not meet. That buys nothing here. The writes are the units being
/// declared — a config file's worth, at startup, and then a rare add — while
/// the reads are a render loop asking for state. Readers do not exclude each
/// other under an [`RwLock`], so the contention sharding exists to fix is
/// contention this workload does not have, and one lock is less to reason
/// about.
///
/// # The units live here
///
/// A unit is held by value, and reached only through the map. That is what
/// keeps the three verbs honest: nothing hands out a unit that could outlive
/// its name, and the log and the current run have exactly one owner.
///
/// It works because none of the three is slow or async. Asking for a state is
/// an atomic load, stopping is a cancellation that does not wait, and
/// starting hands the run to a task rather than performing it — so the read
/// lock is held for that and no longer, and never across an await.
pub struct UnitMap {
    /// The units, by name.
    units: HashMap<String, Unit>,
}

impl UnitMap {
    /// Create an empty map, self referencing through a weak pointer.
    pub fn new(behaviors: HashMap<String, UnitBehavior>) -> Arc<Self> {
        let mut units: HashMap<String, Unit> = HashMap::new();
        for (key, behavior) in behaviors {
            units.insert(key, Unit::new(behavior));
        }
        Arc::new(Self { units })
    }

    /// Start the unit under `key`, using its own behavior.
    ///
    /// Restarts it if it was already running: see
    /// [`Unit::start`](crate::unit::Unit::start), which sees the old run out
    /// before the new one begins.
    pub fn start(&self, key: &str) -> Result<Arc<RunnerHandle>> {
        self.with(key, Unit::start)?
    }

    /// Stop the unit under `key`.
    ///
    /// Returns without waiting for the process to be gone; the unit keeps
    /// reporting its terminal state through [`state`](UnitMap::state).
    pub fn stop(&self, key: &str) -> Result<()> {
        self.with(key, Unit::stop)
    }

    pub fn dispatch(&self, key: &str, event: UnitEvent) -> Result<Option<UnitAction>> {
        self.with(key, |unit| Unit::dispatch(unit, event))
    }

    pub fn clone_handle(&self, key: &str) -> Result<Option<Arc<RunnerHandle>>> {
        self.with(key, Unit::clone_handle)
    }

    /// What the unit under `key` is called on screen.
    pub fn name(&self, key: &str) -> Result<ArcStr> {
        self.with(key, Unit::name)
    }

    /// Which of its modes the unit under `key` is currently on, if it has any.
    pub fn mode(&self, key: &str) -> Result<Option<ArcStr>> {
        self.with(key, Unit::mode)
    }

    /// State of the unit under `key`.
    ///
    /// [`Stopped`](RunnerState::Stopped) for a unit that was declared and
    /// never started — which is a different thing from a name that was never
    /// declared, and that is the error.
    pub fn state(&self, key: &str) -> Result<RunnerState> {
        self.with(key, Unit::state)
    }

    /// Get a new LogReader for the unit
    pub fn log_reader(&self, key: &str) -> Option<LogReader> {
        self.with(key, Unit::log_reader).ok()
    }

    /// The name every unit was declared under.
    ///
    /// In no particular order — the map is a `HashMap`, and the declaration
    /// order did not survive being put into one. A caller that shows these to
    /// a person has to impose an order of its own, or the same session will
    /// list itself differently on every render.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.units.keys().map(String::as_str)
    }

    /// Stop every unit, and wait until each one is really gone.
    ///
    /// [`stop`](UnitMap::stop) only asks; the process is still on its way out
    /// when it returns, and dropping the map does no better — `Drop` cannot
    /// await. This is the teardown you can observe, which is what a session
    /// that owns its children wants before its own process exits.
    ///
    /// Every unit is asked to stop before any of them is waited on, so the
    /// grace periods overlap instead of queueing up one shutdown at a time.
    pub async fn shutdown(&self) {
        let handles: Vec<Arc<RunnerHandle>> = self
            .units
            .values()
            .filter_map(|unit| {
                unit.stop();
                unit.clone_handle()
            })
            .collect();
        for handle in handles {
            handle.wait().await;
        }
    }

    /// Run `f` on the unit under `key`, or fail naming what was asked for.
    ///
    /// An unknown name is a mistake in a config or a command, not a state a
    /// unit can be in, so it is reported as an error rather than silently
    /// doing nothing — once, here, for all three verbs.
    fn with<T>(&self, key: &str, f: impl FnOnce(&Unit) -> T) -> Result<T> {
        let unit = self
            .units
            .get(key)
            .with_context(|| format!("no unit named `{key}`"))?;
        Ok(f(unit))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// A map with `keys` declared, each running nothing.
    fn map(keys: &[&str]) -> Arc<UnitMap> {
        let mut behaviors: HashMap<String, UnitBehavior> = HashMap::new();
        for key in keys {
            behaviors.insert((*key).to_owned(), UnitBehavior::noop(*key));
        }
        UnitMap::new(behaviors)
    }

    /// A unit that was declared and never started is stopped, which is a
    /// state; a name that was never declared is not, and that is the error.
    #[test]
    fn a_name_nobody_declared_is_an_error_rather_than_a_state() {
        let map = map(&["server"]);
        assert!(matches!(map.state("server"), Ok(RunnerState::Stopped)));

        let error = map.state("nope").unwrap_err().to_string();
        assert!(error.contains("nope"), "the name should be in: {error}");
        assert!(map.start("nope").is_err());
        assert!(map.stop("nope").is_err());
    }

    #[tokio::test]
    async fn starting_by_name_runs_the_unit_under_it() {
        let map = map(&["server"]);
        let handle = map.start("server").expect("it was just declared");

        assert!(matches!(
            handle.wait().await,
            Some(RunnerState::ExitSuccess)
        ));
        assert!(map.state("server").unwrap().is_finished());
    }

    /// Stopping reaches the run through the map, and the terminal state stays
    /// readable afterwards — the unit is not taken out of the map by it.
    #[tokio::test]
    async fn stopping_by_name_ends_the_run_and_leaves_the_unit() {
        let map = map(&["server"]);
        let handle = map.start("server").expect("it was just declared");
        map.stop("server").unwrap();

        handle.wait().await;
        assert!(map.state("server").unwrap().is_finished());
    }
}
