use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use smallvec::SmallVec;
use string_interner::{DefaultStringInterner, DefaultSymbol, Symbol};
use tokio::task::JoinSet;

use crate::{
    app::TARGET_SEPARATOR,
    config::{Config, ConfigPanel, ConfigProc},
    error::{AppConfigError, AppError},
    log::{Log, LogReader},
    runner::RunnerState,
    unit::{Unit, UnitAction, UnitBehavior, UnitChoice, UnitEvent},
    util::{
        event::{EventDispatcher, EventListener},
        graph::{DependencyGraph, DependencyOrder},
        str::SmallStr,
    },
};

/// Every proc of a checked config, as a unit, under the key it was interned
/// as.
///
/// It owns the interner, and so is the only place a name becomes an
/// [`AppUnitKey`]: everything above it — the screen, the commands it sends —
/// addresses a unit by key and never by text.
///
/// Every method takes `&self` and the units are held by value, so one map
/// behind an `Arc` serves the input task, the render loop and whatever
/// supervises a start, while a log and the run writing to it still have
/// exactly one owner. That rests on no method being slow or async: a state is
/// an atomic load, stopping is a cancellation that does not wait, and
/// starting hands the run to a task rather than doing it.
pub struct AppUnitMap {
    /// The one interner every key in the session was minted against.
    interner: DefaultStringInterner,
    /// The units, by key.
    units: HashMap<AppUnitKey, Unit>,
    /// The one dispatcher every unit in the map was given a clone of, so a
    /// screen watches the session rather than one proc at a time.
    event_dispatcher: EventDispatcher,
    /// The largest log here, in chunks — what a shared reader has to be built
    /// at; see [`log_capacity`](AppUnitMap::log_capacity).
    log_capacity_max: usize,
    /// The whole `depends` relation, which is what orders a start.
    dependency_graph: DependencyGraph<AppUnitKey>,
    /// What each unit depends on directly, answered once here because the
    /// schedule asks it for every pending unit on every wake. Only units that
    /// depend on something are in it.
    dependencies: HashMap<AppUnitKey, UnitKeyVec>,
    /// The units declared under each group name, for addressing several at
    /// once.
    groups: HashMap<SmallStr, UnitKeyVec>,
    /// Which list each proc is drawn in, holding only the procs that are not
    /// in the default one — an exception list, the way `dependencies` is.
    panels: HashMap<AppUnitKey, ConfigPanel>,
    /// The procs that read escape sequences differently from the session, on
    /// the same terms as `panels`.
    parse_ansi: HashMap<AppUnitKey, bool>,
    /// What the session said, and so the answer for every proc not in
    /// `parse_ansi`.
    parse_ansi_session: bool,
}

type UnitKeyVec = SmallVec<[AppUnitKey; 16]>;

/// How a unit is addressed once the config has been checked.
///
/// The interned key rather than the name it was interned from: a key is read
/// out of a row, copied into a command and compared every frame, and a name
/// there is a string to clone and hash where this is an integer.
///
/// The symbol is private, so every key that exists came from
/// [`key`](AppUnitMap::key) or [`keys`](AppUnitMap::keys) — the one interner
/// it means anything against. It goes over the socket as that bare index,
/// which holds only because the far end is a client of the process that
/// minted it; anything naming a unit outside that sends the config key
/// instead — see [`key_str`](AppUnitMap::key_str).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AppUnitKey(DefaultSymbol);

impl serde::Serialize for AppUnitKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0.to_usize() as u64)
    }
}
impl<'de> serde::Deserialize<'de> for AppUnitKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = <usize as serde::Deserialize>::deserialize(deserializer)?;
        let value = DefaultSymbol::try_from_usize(raw)
            .ok_or_else(|| serde::de::Error::custom("not a unit key"))?;
        Ok(Self(value))
    }
}

impl AppUnitMap {
    /// Check `config`, intern every key, and build a unit per proc.
    ///
    /// Every problem is collected before any of them is reported — see
    /// [`AppConfigError::Errors`].
    pub fn new(config: &Config) -> Result<Self, AppConfigError> {
        let mut errors: Vec<AppConfigError> = Vec::new();
        let mut interner = DefaultStringInterner::new();
        let dependency_graph = Self::build_dep_graph(&mut errors, &mut interner, config);

        let event_dispatcher = EventDispatcher::new();
        let mut units: HashMap<AppUnitKey, Unit> = HashMap::with_capacity(config.procs.len());
        let mut log_capacity_max = 0;
        let mut groups: HashMap<SmallStr, UnitKeyVec> = HashMap::new();
        let mut panels: HashMap<AppUnitKey, ConfigPanel> = HashMap::new();
        let mut parse_ansi: HashMap<AppUnitKey, bool> = HashMap::new();
        let log_size_session = config.log_size();
        let parse_ansi_session = config.parse_ansi();
        for proc in &config.procs {
            let Some(key) = interner.get(proc.key.as_ref()).map(AppUnitKey) else {
                errors.push(AppConfigError::LogicErrorKey(proc.key.clone()));
                continue;
            };
            for group in &proc.groups {
                groups.entry(group.clone()).or_default().push(key);
            }
            if proc.panel != ConfigPanel::default() {
                panels.insert(key, proc.panel);
            }
            if proc.parse_ansi(parse_ansi_session) != parse_ansi_session {
                parse_ansi.insert(key, !parse_ansi_session);
            }

            let log_size = proc.log_size(log_size_session);
            let mut unit = Unit::new(Self::build_behavior(proc), log_size);
            unit.set_event_dispatcher(event_dispatcher.clone());
            units.insert(key, unit);
            log_capacity_max = log_capacity_max.max(Log::capacity_for_bytes(log_size));
        }
        if !errors.is_empty() {
            return Err(AppConfigError::Errors(errors));
        }
        let mut dependencies: HashMap<AppUnitKey, UnitKeyVec> = HashMap::new();
        for key in units.keys() {
            let deps: UnitKeyVec = dependency_graph.dependencies_copy(*key).collect();
            if !deps.is_empty() {
                dependencies.insert(*key, deps);
            }
        }
        Ok(Self {
            interner,
            units,
            event_dispatcher,
            log_capacity_max,
            dependency_graph,
            dependencies,
            groups,
            panels,
            parse_ansi,
            parse_ansi_session,
        })
    }

    /// Intern every proc key and wire its `depends` edges, collecting every
    /// config problem found on the way into `errors`.
    fn build_dep_graph(
        errors: &mut Vec<AppConfigError>,
        interner: &mut DefaultStringInterner,
        config: &Config,
    ) -> DependencyGraph<AppUnitKey> {
        let mut graph: DependencyGraph<AppUnitKey> = DependencyGraph::new();
        for proc in &config.procs {
            if proc.key.contains(TARGET_SEPARATOR) {
                errors.push(AppConfigError::ReservedCharacter {
                    name: proc.key.clone(),
                    character: TARGET_SEPARATOR,
                });
            }
            for group in &proc.groups {
                if group.contains(TARGET_SEPARATOR) {
                    errors.push(AppConfigError::ReservedCharacter {
                        name: group.clone(),
                        character: TARGET_SEPARATOR,
                    });
                }
            }
            if proc.run.is_some() && proc.modes.is_some() {
                errors.push(AppConfigError::RunAndModes(proc.key.clone()));
            }
            let key = AppUnitKey(interner.get_or_intern(proc.key.as_ref()));
            graph.insert(key);
        }
        for proc in &config.procs {
            let Some(key) = interner.get(proc.key.as_ref()).map(AppUnitKey) else {
                errors.push(AppConfigError::Unknown("key should exist in interner"));
                continue;
            };
            for depends in &proc.depends {
                let Some(depends_key) = interner.get(depends).map(AppUnitKey) else {
                    errors.push(AppConfigError::UnknownDependency {
                        proc: proc.key.clone(),
                        depends: depends.clone(),
                    });
                    continue;
                };
                if !graph.contains(&depends_key) {
                    errors.push(AppConfigError::UnknownDependency {
                        proc: proc.key.clone(),
                        depends: depends.clone(),
                    });
                    // Adding the edge anyway would invent the node it points
                    // at, and the phantom would then turn up in the start
                    // order as a proc nobody declared.
                    continue;
                }
                graph.add_dependency(key, depends_key);
            }
        }
        let resolved = graph.resolve();
        for cycle in resolved.cycles() {
            let mut cycle_procs = Vec::with_capacity(cycle.len());
            for key in cycle {
                if let Some(key_str) = interner.resolve(key.0) {
                    cycle_procs.push(SmallStr::new(key_str));
                };
            }
            errors.push(AppConfigError::Cycle(cycle_procs))
        }

        graph
    }

    /// Get the behavior from the config
    fn build_behavior(proc: &ConfigProc) -> UnitBehavior {
        let name = proc.name.clone().unwrap_or_else(|| proc.key.clone());
        let short = proc.name_short.clone();

        // Every arm falls through to the one `with_short`: a `return` here
        // reads as the same thing and is not, since the proc's own short name
        // is applied below.
        // Where each run happens is settled here and nowhere else: a mode's
        // `working_dir` wins, the proc's stands in for a mode that named
        // none, and neither means the child inherits. Folding it once is what
        // keeps `unit` from holding a parent to ask.
        let behavior = if let Some(run) = &proc.run {
            UnitBehavior::run_many(
                name,
                Arc::new(run.commands.clone()),
                proc.working_dir.clone(),
            )
        } else if let Some(modes) = &proc.modes {
            UnitBehavior::modes(
                name,
                modes.iter().map(|mode| {
                    let commands = Arc::new(mode.run.commands.clone());
                    let working_dir = mode
                        .working_dir
                        .clone()
                        .or_else(|| proc.working_dir.clone());
                    UnitBehavior::run_many(mode.name.clone(), commands, working_dir)
                        .with_short(mode.name_short.clone())
                }),
            )
        } else {
            UnitBehavior::noop(name)
        };
        behavior.with_short(short)
    }

    /// The key `name` was interned as, if a proc was declared under it.
    ///
    /// The one door from text to [`AppUnitKey`]: what comes off a command
    /// line or out of a config is a name, and everything past here is a key.
    pub fn key(&self, name: &str) -> Option<AppUnitKey> {
        self.interner.get(name).map(AppUnitKey)
    }

    /// The text `key` was interned from, for whatever has to name a unit
    /// outside this process — where an [`AppUnitKey`] means nothing.
    pub fn key_str(&self, key: AppUnitKey) -> Option<&str> {
        self.interner.resolve(key.0)
    }

    /// Start the unit under `key`, restarting it if it was already running —
    /// see [`Unit::start`](crate::unit::Unit::start), which sees the old run
    /// out before the new one begins.
    pub fn start(&self, key: AppUnitKey) -> Result<(), AppError> {
        self.with(key, Unit::start)?.map_err(AppError::UnitStart)?;
        Ok(())
    }

    /// Build the unit's run without releasing it, so a unit still waiting on
    /// its dependencies exists and reports
    /// [`Waiting`](RunnerState::Waiting) instead of nothing at all.
    pub fn ensure_created(&self, key: AppUnitKey) -> Result<(), AppError> {
        self.with(key, Unit::ensure_created)?
            .map_err(AppError::UnitStart)?;
        Ok(())
    }

    /// Release the run [`ensure_created`](AppUnitMap::ensure_created) parked,
    /// or restart the unit if that run has already gone — what the schedule
    /// gives a unit the caller named, against the
    /// [`ensure_started`](AppUnitMap::ensure_started) one pulled in as a
    /// dependency gets.
    pub fn start_or_resume(&self, key: AppUnitKey) -> Result<(), AppError> {
        self.with(key, Unit::start_or_resume)?
            .map_err(AppError::UnitStart)?;
        Ok(())
    }

    /// Start `key` if it has never been started, and leave it alone
    /// otherwise — unlike [`start`](AppUnitMap::start), which restarts it.
    pub fn ensure_started(&self, key: AppUnitKey) -> Result<(), AppError> {
        self.with(key, Unit::ensure_started)?
            .map_err(AppError::UnitStart)?;
        Ok(())
    }

    /// Stop the unit under `key`.
    ///
    /// Returns without waiting for the process to be gone; the unit keeps
    /// reporting its terminal state through [`state`](AppUnitMap::state).
    pub fn stop(&self, key: AppUnitKey) -> Result<(), AppError> {
        self.with(key, Unit::stop)
    }

    /// State of the unit under `key`.
    ///
    /// [`Stopped`](RunnerState::Stopped) for a unit that was declared and
    /// never started — which is a different thing from a key nothing was
    /// declared under, and that is the error.
    pub fn state(&self, key: AppUnitKey) -> Result<RunnerState, AppError> {
        self.with(key, Unit::state)
    }

    /// Which of its modes the unit under `key` is currently on, if it has any.
    pub fn mode(&self, key: AppUnitKey) -> Result<Option<SmallStr>, AppError> {
        self.with(key, Unit::mode)
    }

    /// That mode's short name, if it declared one.
    pub fn mode_short(&self, key: AppUnitKey) -> Result<Option<SmallStr>, AppError> {
        self.with(key, Unit::mode_short)
    }

    /// What the unit under `key` is called on screen.
    pub fn name(&self, key: AppUnitKey) -> Result<SmallStr, AppError> {
        self.with(key, Unit::name)
    }

    /// The shorter name for the unit under `key`, if it declared one.
    pub fn name_short(&self, key: AppUnitKey) -> Result<Option<SmallStr>, AppError> {
        self.with(key, Unit::name_short)
    }

    /// Everything that can be asked of the unit under `key` right now.
    pub fn choices(&self, key: AppUnitKey, out: &mut Vec<UnitChoice>) -> Result<(), AppError> {
        self.with(key, |unit| Unit::choices(unit, out))
    }

    /// Hand `event` to the unit under `key` and report back what it asks for.
    ///
    /// Reported and not done: a start has to go through the schedule, which
    /// is above this layer, so the action is carried out by
    /// [`App::dispatch`](crate::app::App::dispatch).
    pub fn dispatch(
        &self,
        key: AppUnitKey,
        event: UnitEvent,
    ) -> Result<Option<UnitAction>, AppError> {
        self.with(key, |unit| Unit::dispatch(unit, event))
    }

    /// A new reader over the log of `key`.
    pub fn log_reader(&self, key: AppUnitKey) -> Option<LogReader> {
        self.with(key, Unit::log_reader).ok()
    }

    /// Put `reader` on the log of `key`, in place of building one.
    ///
    /// `false` is a key nothing was declared under, and leaves the reader on
    /// whatever it was following — a caller that cannot address a unit has
    /// nothing to show anyway.
    pub fn log_reader_into(&self, key: AppUnitKey, reader: &mut LogReader) -> bool {
        self.with(key, |unit| unit.log_reader_into(reader)).is_ok()
    }

    /// How many chunks the largest log here holds.
    ///
    /// The largest and not each, because this is what a caller reusing one
    /// reader across units builds it at: a reader holds its ring to whichever
    /// log it is put on but never past the size it was built for, so one
    /// built smaller would follow a shorter tail than the log it is on — see
    /// [`LogReader`](crate::log::LogReader).
    pub fn log_capacity(&self) -> usize {
        self.log_capacity_max
    }

    /// Every unit's key, in the `HashMap`'s order, which is to say in none —
    /// a caller showing these to a person has to impose one.
    pub fn keys(&self) -> impl Iterator<Item = AppUnitKey> {
        self.units.keys().copied()
    }

    /// Fill `resolved` with every unit that has run to the end at least once
    /// — see [`Unit::resolved`](crate::unit::Unit::resolved).
    ///
    /// Clears it first and takes it by reference, so the loop that asks on
    /// every wake keeps one set instead of building a new one each time.
    pub fn write_resolved(&self, resolved: &mut HashSet<AppUnitKey>) {
        resolved.clear();
        for (value, unit) in &self.units {
            if unit.resolved() {
                resolved.insert(*value);
            }
        }
    }

    /// What `key` must wait for, one edge out.
    ///
    /// `None` is "nothing to wait for" — both a unit that depends on nothing
    /// and a key no unit was declared under, which want the same answer.
    pub fn direct_dependencies(&self, key: AppUnitKey) -> Option<&UnitKeyVec> {
        self.dependencies.get(&key)
    }

    /// Which list `key` is drawn in.
    ///
    /// [`Main`](ConfigPanel::Main) for a key no proc was declared under, which
    /// wants the same answer as a proc that said nothing: there is one list
    /// until a config asks for two.
    pub fn panel(&self, key: AppUnitKey) -> ConfigPanel {
        self.panels.get(&key).copied().unwrap_or_default()
    }

    /// Whether `key`'s escape sequences are read as escape sequences.
    ///
    /// The session's answer for a key no proc was declared under, which is
    /// what a proc that said nothing gets too.
    pub fn parse_ansi(&self, key: AppUnitKey) -> bool {
        self.parse_ansi
            .get(&key)
            .copied()
            .unwrap_or(self.parse_ansi_session)
    }

    /// Every unit declared under a group name, or `None` if none was.
    pub fn group(&self, group: &str) -> Option<&UnitKeyVec> {
        self.groups.get(group)
    }

    /// Everything `keys` need, transitively, in an order that starts them —
    /// see [`resolve_from_many`](DependencyGraph::resolve_from_many).
    pub fn resolve_dependency_chain(&self, keys: &[AppUnitKey]) -> DependencyOrder<AppUnitKey> {
        self.dependency_graph.resolve_from_many(keys)
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

    /// Stop every unit, and wait until each one is really gone.
    ///
    /// [`stop`](AppUnitMap::stop) only asks, and dropping the map does no
    /// better since `Drop` cannot await. This is the teardown you can
    /// observe, which is what a session that owns its children wants before
    /// its own process exits.
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
    fn with<T>(&self, key: AppUnitKey, f: impl FnOnce(&Unit) -> T) -> Result<T, AppError> {
        let Some(unit) = self.units.get(&key) else {
            return Err(AppError::NotFound);
        };
        Ok(f(unit))
    }
}
