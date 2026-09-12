use std::{
    collections::{HashMap, hash_map::Entry},
    sync::{Arc, Weak},
};

use anyhow::{Context, Result};
use parking_lot::RwLock;

use crate::{
    log::LogReader,
    runner::{RunnerHandle, RunnerState},
    unit::{behavior::UnitBehavior, unit::Unit},
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
    /// A weak pointer back to itself (hence [`Arc::new_cyclic`]), so that the
    /// `UnitContext` handed to each unit can reach the map without a cycle
    /// that would leak it.
    ptr: Weak<UnitMap>,
    /// The units, by name.
    units: RwLock<HashMap<String, Unit>>,
}

impl UnitMap {
    /// Create an empty map, self referencing through a weak pointer.
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|ptr| Self {
            ptr: ptr.clone(),
            units: RwLock::new(HashMap::new()),
        })
    }

    /// Declare a unit under `key`.
    ///
    /// It is created stopped, with an empty log; nothing runs until
    /// [`start`](UnitMap::start).
    ///
    /// Adding over a name that is already taken replaces it, and the unit
    /// that was there is dropped — which aborts whatever it was running,
    /// without waiting for it to be gone.
    pub fn add(&self, key: impl Into<String>, behavior: UnitBehavior) -> Option<LogReader> {
        let mut lock = self.units.write();
        match lock.entry(key.into()) {
            Entry::Occupied(_) => None,
            Entry::Vacant(vacant_entry) => {
                let unit = Unit::new(behavior);
                Some(vacant_entry.insert(unit).log_reader())
            }
        }
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

    /// Run `f` on the unit under `key`, or fail naming what was asked for.
    ///
    /// An unknown name is a mistake in a config or a command, not a state a
    /// unit can be in, so it is reported as an error rather than silently
    /// doing nothing — once, here, for all three verbs.
    fn with<T>(&self, key: &str, f: impl FnOnce(&Unit) -> T) -> Result<T> {
        let units = self.units.read();
        let unit = units
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
        let map = UnitMap::new();
        for key in keys {
            map.add(*key, UnitBehavior::noop());
        }
        map
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

    /// Adding over a name puts a fresh unit there: the run that was under it
    /// is gone, not inherited.
    #[tokio::test]
    async fn adding_over_a_name_replaces_what_was_there() {
        let map = map(&["server"]);
        map.start("server").unwrap();

        map.add("server", UnitBehavior::noop());
        assert!(matches!(map.state("server"), Ok(RunnerState::Stopped)));
    }
}
