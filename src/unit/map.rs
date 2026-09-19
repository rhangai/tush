use std::borrow::Borrow;
use std::hash::Hash;
use std::{collections::HashMap, sync::Arc};

use crate::error::UnitMapError;
use crate::util::event::{EventDispatcher, EventListener};
use crate::util::str::SmallStr;
use crate::{
    log::LogReader,
    runner::{RunnerHandle, RunnerState},
    unit::{UnitAction, UnitChoice, UnitEvent, behavior::UnitBehavior, unit::Unit},
};

/// Every [`Unit`] in a session, by name.
///
/// **Shared, not owned.** Every method takes `&self`, so a map behind an
/// `Arc` can be handed to the input task, the render loop and whatever
/// supervises a start, and all of them can address units without owning one.
///
/// **The units live here**, held by value and reached only through the map,
/// so nothing hands out a unit that could outlive its name and the log and
/// the current run have exactly one owner. That works because no method is
/// slow or async: a state is an atomic load, stopping is a cancellation that
/// does not wait, and starting hands the run to a task rather than doing it.
pub struct UnitMap<K> {
    /// The units, by name.
    units: HashMap<K, Unit>,
    /// The one dispatcher every unit in the map was given a clone of, so a
    /// screen watches the session rather than one proc at a time.
    event_dispatcher: EventDispatcher,
}

impl<K> UnitMap<K>
where
    K: Eq + Hash,
{
    /// A map holding one unit per behavior.
    pub fn new(behaviors: HashMap<K, UnitBehavior>) -> Arc<Self> {
        let mut units: HashMap<K, Unit> = HashMap::new();
        let event_dispatcher = EventDispatcher::new();
        for (key, behavior) in behaviors {
            let mut unit = Unit::new(behavior);
            unit.set_event_dispatcher(event_dispatcher.clone());
            units.insert(key, unit);
        }
        Arc::new(Self {
            units,
            event_dispatcher,
        })
    }

    /// A listener that wakes whenever any unit here changes state.
    ///
    /// Which unit is not part of it: the answer is always to look at the map
    /// again, so saying more would only be something to keep in step.
    pub fn create_listener(&self) -> EventListener {
        self.event_dispatcher.create_listener()
    }

    /// Start the unit under `key`, using its own behavior.
    ///
    /// Restarts it if it was already running: see
    /// [`Unit::start`](crate::unit::Unit::start), which sees the old run out
    /// before the new one begins.
    pub fn start<Q>(&self, key: &Q) -> Result<Arc<RunnerHandle>, UnitMapError>
    where
        K: Borrow<Q> + Eq + Hash,
        Q: Hash + Eq + ?Sized,
    {
        let result = self.with(key, Unit::start)?;
        result.map_err(UnitMapError::UnitStart)
    }

    /// Stop the unit under `key`.
    ///
    /// Returns without waiting for the process to be gone; the unit keeps
    /// reporting its terminal state through [`state`](UnitMap::state).
    pub fn stop<Q>(&self, key: &Q) -> Result<(), UnitMapError>
    where
        K: Borrow<Q> + Eq + Hash,
        Q: Hash + Eq + ?Sized,
    {
        self.with(key, Unit::stop)
    }

    /// Hand `event` to the unit under `key`, and report what it asks for.
    pub fn dispatch<Q>(&self, key: &Q, event: UnitEvent) -> Result<Option<UnitAction>, UnitMapError>
    where
        K: Borrow<Q> + Eq + Hash,
        Q: Hash + Eq + ?Sized,
    {
        self.with(key, |unit| Unit::dispatch(unit, event))
    }

    /// Everything that can be asked of the unit under `key` right now.
    pub fn choices<Q>(&self, key: &Q, out: &mut Vec<UnitChoice>) -> Result<(), UnitMapError>
    where
        K: Borrow<Q> + Eq + Hash,
        Q: Hash + Eq + ?Sized,
    {
        self.with(key, |unit| Unit::choices(unit, out))
    }

    /// The handle for the current run of `key`, if it has one.
    pub fn clone_handle<Q>(&self, key: &Q) -> Result<Option<Arc<RunnerHandle>>, UnitMapError>
    where
        K: Borrow<Q> + Eq + Hash,
        Q: Hash + Eq + ?Sized,
    {
        self.with(key, Unit::clone_handle)
    }

    /// What the unit under `key` is called on screen.
    pub fn name<Q>(&self, key: &Q) -> Result<SmallStr, UnitMapError>
    where
        K: Borrow<Q> + Eq + Hash,
        Q: Hash + Eq + ?Sized,
    {
        self.with(key, Unit::name)
    }

    /// The shorter name for the unit under `key`, if it declared one.
    pub fn name_short<Q>(&self, key: &Q) -> Result<Option<SmallStr>, UnitMapError>
    where
        K: Borrow<Q> + Eq + Hash,
        Q: Hash + Eq + ?Sized,
    {
        self.with(key, Unit::name_short)
    }

    /// Which of its modes the unit under `key` is currently on, if it has any.
    pub fn mode<Q>(&self, key: &Q) -> Result<Option<SmallStr>, UnitMapError>
    where
        K: Borrow<Q> + Eq + Hash,
        Q: Hash + Eq + ?Sized,
    {
        self.with(key, Unit::mode)
    }

    /// That mode's short name, if it declared one.
    pub fn mode_short<Q>(&self, key: &Q) -> Result<Option<SmallStr>, UnitMapError>
    where
        K: Borrow<Q> + Eq + Hash,
        Q: Hash + Eq + ?Sized,
    {
        self.with(key, Unit::mode_short)
    }

    /// State of the unit under `key`.
    ///
    /// [`Stopped`](RunnerState::Stopped) for a unit that was declared and
    /// never started — which is a different thing from a name that was never
    /// declared, and that is the error.
    pub fn state<Q>(&self, key: &Q) -> Result<RunnerState, UnitMapError>
    where
        K: Borrow<Q> + Eq + Hash,
        Q: Hash + Eq + ?Sized,
    {
        self.with(key, Unit::state)
    }

    /// A new reader over the log of `key`.
    pub fn log_reader<Q>(&self, key: &Q) -> Option<LogReader>
    where
        K: Borrow<Q> + Eq + Hash,
        Q: Hash + Eq + ?Sized,
    {
        self.with(key, Unit::log_reader).ok()
    }

    /// Whether a unit was declared under `key`.
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q> + Eq + Hash,
        Q: Hash + Eq + ?Sized,
    {
        self.units.contains_key(key)
    }

    /// The name every unit was declared under.
    ///
    /// In no particular order — the map is a `HashMap`, and the declaration
    /// order did not survive being put into one. A caller that shows these to
    /// a person has to impose an order of its own, or the same session will
    /// list itself differently on every render.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.units.keys()
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
    /// An unknown name is a mistake in a config or a command and not a state
    /// a unit can be in, so it is an error rather than a quiet no-op — said
    /// once, here, for every method.
    fn with<Q, T>(&self, key: &Q, f: impl FnOnce(&Unit) -> T) -> Result<T, UnitMapError>
    where
        K: Borrow<Q> + Eq + Hash,
        Q: Hash + Eq + ?Sized,
    {
        let Some(unit) = self.units.get(key) else {
            return Err(UnitMapError::NotFound);
        };
        Ok(f(unit))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// A map with `keys` declared, each running nothing.
    fn map(keys: &[&str]) -> Arc<UnitMap<SmallStr>> {
        let mut behaviors: HashMap<SmallStr, UnitBehavior> = HashMap::new();
        for key in keys {
            let key = SmallStr::from(*key);
            behaviors.insert(key.clone(), UnitBehavior::noop(key));
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
