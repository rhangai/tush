use std::collections::HashSet;
use std::hash::Hash;
use std::{collections::HashMap, sync::Arc};

use string_interner::{DefaultStringInterner, DefaultSymbol, Symbol};
use tokio::task::JoinSet;

use crate::error::UnitMapError;
use crate::unit::UnitHandle;
use crate::util::event::{EventDispatcher, EventListener};
use crate::util::str::SmallStr;
use crate::{
    log::{Log, LogReader},
    runner::RunnerState,
    unit::{UnitAction, UnitChoice, UnitEvent, behavior::UnitBehavior, unit::Unit},
};

/// Every [`Unit`] in a session, under the key its caller addresses it by.
///
/// Generic over that key so the caller can use whatever it already holds —
/// the app interns names and addresses units by the symbol, the tests here by
/// the text itself — while the map only ever hashes it.
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
pub struct UnitMap {
    /// Interner for unit keys
    interner: DefaultStringInterner,
    /// The units, by key.
    units: HashMap<DefaultSymbol, Unit>,
    /// The one dispatcher every unit in the map was given a clone of, so a
    /// screen watches the session rather than one proc at a time.
    event_dispatcher: EventDispatcher,
    /// How much output each unit's log keeps, in bytes.
    log_size: usize,
}

/// How a unit is addressed once the config has been checked.
///
/// The interned key rather than the name it was interned from: a key is read
/// out of a row, copied into a command and compared every frame, and a name
/// there is a string to clone and hash where this is an integer.
///
/// The symbol is private, so every key that exists came from
/// [`key`](UnitMap::key) or [`keys`](UnitMap::keys) — which is to say
/// from the one interner it means anything against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UnitKey {
    value: DefaultSymbol,
}

/// Out to a client as the opaque number it is.
///
/// Written out rather than derived because the symbol has no representation
/// of its own, and because what crosses is a *handle*: it means something
/// only against the interner that minted it, so a client echoes it back at
/// nothing — a URL names a unit by name. Fabricating one is harmless for the
/// same reason every lookup here answers `None` for a key it does not hold.
impl serde::Serialize for UnitKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.value.to_usize() as u64)
    }
}

impl<'de> serde::Deserialize<'de> for UnitKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = <u64 as serde::Deserialize>::deserialize(deserializer)?;
        let value = DefaultSymbol::try_from_usize(raw as usize)
            .ok_or_else(|| serde::de::Error::custom("not a unit key"))?;
        Ok(Self { value })
    }
}

impl UnitMap {
    /// A map holding one unit per behavior, each keeping `log_size` bytes.
    pub fn new<K>(behaviors: HashMap<K, UnitBehavior>, log_size: usize) -> Self
    where
        K: AsRef<str>,
    {
        let mut map = Self::with_capacity(behaviors.len(), log_size);
        for (key, behavior) in behaviors {
            map.add(key.as_ref(), behavior);
        }
        map
    }

    /// An empty map with room for `capacity` units.
    ///
    /// `log_size` is taken here and not per unit because every unit in one
    /// session gets the same: it is the config's answer to how far back the
    /// pane scrolls, and there is one config.
    pub fn with_capacity(capacity: usize, log_size: usize) -> Self {
        Self {
            interner: DefaultStringInterner::new(),
            units: HashMap::with_capacity(capacity),
            event_dispatcher: EventDispatcher::new(),
            log_size,
        }
    }

    /// Add a new unit
    pub fn add(&mut self, key: &str, behavior: UnitBehavior) -> UnitKey {
        let key = self.reserve(key);
        self.insert(key, behavior);
        key
    }

    /// Add a new unit
    pub fn insert(&mut self, key: UnitKey, behavior: UnitBehavior) {
        let mut unit = Unit::new(behavior, self.log_size);
        unit.set_event_dispatcher(self.event_dispatcher.clone());
        self.units.insert(key.value, unit);
    }

    /// Reserve a key
    pub fn reserve(&mut self, key: &str) -> UnitKey {
        let key = self.interner.get_or_intern(key);
        UnitKey { value: key }
    }

    /// Get the key
    pub fn key(&self, key: &str) -> Option<UnitKey> {
        let key = self.interner.get(key)?;
        Some(UnitKey { value: key })
    }

    /// Get the key str
    pub fn key_str(&self, key: UnitKey) -> Option<&str> {
        self.interner.resolve(key.value)
    }

    /// A listener that wakes whenever any unit here changes state.
    ///
    /// Which unit is not part of it: the answer is always to look at the map
    /// again, so saying more would only be something to keep in step.
    pub fn create_listener(&self) -> EventListener {
        self.event_dispatcher.create_listener()
    }

    /// The dispatcher the units here trigger, for something outside the map
    /// that has to wake the same listeners.
    pub fn event_dispatcher(&self) -> &EventDispatcher {
        &self.event_dispatcher
    }

    /// Start the unit under `key`, using its own behavior.
    ///
    /// Restarts it if it was already running: see
    /// [`Unit::start`](crate::unit::Unit::start), which sees the old run out
    /// before the new one begins.
    pub fn start(&self, key: UnitKey) -> Result<Arc<UnitHandle>, UnitMapError> {
        let result = self.with(key, Unit::start)?;
        result.map_err(UnitMapError::UnitStart)
    }

    /// Start the unit under `key` only if it has never been started.
    ///
    /// The other half of [`start`](UnitMap::start), for a caller that wants
    /// it running rather than wants it run.
    pub fn ensure_started(&self, key: UnitKey) -> Result<Arc<UnitHandle>, UnitMapError> {
        let result = self.with(key, Unit::ensure_started)?;
        result.map_err(UnitMapError::UnitStart)
    }

    pub fn ensure_created(&self, key: UnitKey) -> Result<Arc<UnitHandle>, UnitMapError> {
        let result = self.with(key, Unit::ensure_created)?;
        result.map_err(UnitMapError::UnitStart)
    }

    pub fn start_or_resume(&self, key: UnitKey) -> Result<Arc<UnitHandle>, UnitMapError> {
        let result = self.with(key, Unit::start_or_resume)?;
        result.map_err(UnitMapError::UnitStart)
    }

    /// Stop the unit under `key`.
    ///
    /// Returns without waiting for the process to be gone; the unit keeps
    /// reporting its terminal state through [`state`](UnitMap::state).
    pub fn stop(&self, key: UnitKey) -> Result<(), UnitMapError> {
        self.with(key, Unit::stop)
    }

    /// Hand `event` to the unit under `key`, and report what it asks for.
    pub fn dispatch(
        &self,
        key: UnitKey,
        event: UnitEvent,
    ) -> Result<Option<UnitAction>, UnitMapError> {
        self.with(key, |unit| Unit::dispatch(unit, event))
    }

    /// Everything that can be asked of the unit under `key` right now.
    pub fn choices(&self, key: UnitKey, out: &mut Vec<UnitChoice>) -> Result<(), UnitMapError> {
        self.with(key, |unit| Unit::choices(unit, out))
    }

    /// The handle for the current run of `key`, if it has one.
    pub fn clone_handle(&self, key: UnitKey) -> Result<Option<Arc<UnitHandle>>, UnitMapError> {
        self.with(key, Unit::clone_handle)
    }

    /// What the unit under `key` is called on screen.
    pub fn name(&self, key: UnitKey) -> Result<SmallStr, UnitMapError> {
        self.with(key, Unit::name)
    }

    /// The shorter name for the unit under `key`, if it declared one.
    pub fn name_short(&self, key: UnitKey) -> Result<Option<SmallStr>, UnitMapError> {
        self.with(key, Unit::name_short)
    }

    /// Which of its modes the unit under `key` is currently on, if it has any.
    pub fn mode(&self, key: UnitKey) -> Result<Option<SmallStr>, UnitMapError> {
        self.with(key, Unit::mode)
    }

    /// That mode's short name, if it declared one.
    pub fn mode_short(&self, key: UnitKey) -> Result<Option<SmallStr>, UnitMapError> {
        self.with(key, Unit::mode_short)
    }

    /// State of the unit under `key`.
    ///
    /// [`Stopped`](RunnerState::Stopped) for a unit that was declared and
    /// never started — which is a different thing from a key nothing was
    /// declared under, and that is the error.
    pub fn state(&self, key: UnitKey) -> Result<RunnerState, UnitMapError> {
        self.with(key, Unit::state)
    }

    /// A new reader over the log of `key`.
    pub fn log_reader(&self, key: UnitKey) -> Option<LogReader> {
        self.with(key, Unit::log_reader).ok()
    }

    /// Put `reader` on the log of `key`, in place of building one.
    ///
    /// `false` is a key nothing was declared under, and leaves the reader on
    /// whatever it was following — a caller that cannot address a unit has
    /// nothing to show anyway.
    pub fn log_reader_into(&self, key: UnitKey, reader: &mut LogReader) -> bool {
        self.with(key, |unit| unit.log_reader_into(reader)).is_ok()
    }

    /// How many chunks every log in this map holds.
    ///
    /// One figure for the map because `log_size` is — see
    /// [`with_capacity`](UnitMap::with_capacity). It is what a caller sizes a
    /// reader by when it means to reuse one across units.
    pub fn log_capacity(&self) -> usize {
        Log::capacity_for_bytes(self.log_size)
    }

    /// Whether a unit was declared under `key`.
    pub fn contains(&self, key: UnitKey) -> bool {
        self.units.contains_key(&key.value)
    }

    /// Every key a unit was declared under.
    ///
    /// In no particular order — the map is a `HashMap`, and the declaration
    /// order did not survive being put into one. A caller that shows these to
    /// a person has to impose an order of its own, or the same session will
    /// list itself differently on every render.
    pub fn keys(&self) -> impl Iterator<Item = UnitKey> {
        self.units.keys().map(|k| UnitKey { value: *k })
    }

    /// Fill `resolved` with every unit that has run to the end at least once
    /// — see [`Unit::resolved`](crate::unit::Unit::resolved).
    ///
    /// Clears it first and takes it by reference, so the caller that asks
    /// this on every wake keeps one set instead of building a new one each
    /// time.
    pub fn write_resolved(&self, resolved: &mut HashSet<UnitKey>) {
        resolved.clear();
        for (key, unit) in &self.units {
            if unit.resolved() {
                resolved.insert(UnitKey { value: *key });
            }
        }
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
        let mut join_set: JoinSet<()> = JoinSet::new();
        for unit in self.units.values() {
            if let Some(handle) = unit.clone_handle() {
                join_set.spawn(async move {
                    handle.abort_and_wait().await;
                });
            };
        }
        while join_set.join_next().await.is_some() {}
    }

    /// Run `f` on the unit under `key`, or fail naming what was asked for.
    ///
    /// An unknown key is a mistake in a config or a command and not a state
    /// a unit can be in, so it is an error rather than a quiet no-op — said
    /// once, here, for every method.
    fn with<T>(&self, key: UnitKey, f: impl FnOnce(&Unit) -> T) -> Result<T, UnitMapError> {
        let Some(unit) = self.units.get(&key.value) else {
            return Err(UnitMapError::NotFound);
        };
        Ok(f(unit))
    }
}
