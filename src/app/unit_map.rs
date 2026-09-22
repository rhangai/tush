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
    log::{Log, LogReader, LogReaderSettings},
    unit::{Unit, UnitBehavior},
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
/// supervises a start, while a log and the run writing to it keep exactly one
/// owner — [`entry`](AppUnitMap::entry) lends a unit out for as long as the
/// map is borrowed, and never gives it away. That rests on no method being
/// slow or async: a state is an atomic load, stopping is a cancellation that
/// does not wait, and starting hands the run to a task rather than doing it.
pub struct AppUnitMap {
    /// The one interner every key in the session was minted against.
    interner: DefaultStringInterner,
    /// Every proc of the config, by key.
    units: HashMap<AppUnitKey, AppUnitEntry>,
    /// The one dispatcher every unit in the map was given a clone of, so a
    /// screen watches the session rather than one proc at a time.
    event_dispatcher: EventDispatcher,
    /// The largest log here, in chunks — what a shared reader has to be built
    /// at; see [`log_capacity`](AppUnitMap::log_capacity).
    log_capacity_max: usize,
    /// The whole `depends` relation, which is what orders a start.
    dependency_graph: DependencyGraph<AppUnitKey>,
    /// The units declared under each group name — the one lookup still keyed
    /// by something other than a unit, so it has no [`AppUnitEntry`] to sit in.
    groups: HashMap<SmallStr, UnitKeyVec>,
}

/// A handful of keys: the units of a group, or what one unit waits for.
/// Sixteen inline, so an ordinary list of either is a copy and not an
/// allocation.
type UnitKeyVec = SmallVec<[AppUnitKey; 16]>;

/// One declared proc, as a running session holds it.
///
/// Everything the config settled about one proc, in the entry the unit itself
/// is in. A map per setting is a parallel table to this one, and each needs
/// the session default kept beside it to answer for the procs that said
/// nothing; resolved once here, there is nothing left to fall back to.
pub struct AppUnitEntry {
    /// The name the config declared it under, kept because an
    /// [`AppUnitKey`] means nothing outside this process.
    key: SmallStr,
    /// The unit itself, lent out by [`unit`](AppUnitEntry::unit) and never
    /// moved out: the log and the current run have one owner, the map.
    unit: Unit,
    /// What the config said about it that it does not itself act on.
    settings: AppUnitSettings,
    /// What it waits for, one edge out, flattened from the graph because the
    /// schedule asks it for every pending unit on every wake. Empty for a
    /// proc that depends on nothing.
    dependencies: UnitKeyVec,
}

impl AppUnitEntry {
    /// The config key, for whatever has to name this unit outside the process.
    pub fn key(&self) -> &SmallStr {
        &self.key
    }
    /// The unit, for as long as the map is borrowed.
    pub fn unit(&self) -> &Unit {
        &self.unit
    }
    /// What the config settled about it — see [`AppUnitSettings`].
    pub fn settings(&self) -> &AppUnitSettings {
        &self.settings
    }
    /// What it waits for, empty when that is nothing.
    pub fn dependencies(&self) -> &UnitKeyVec {
        &self.dependencies
    }

    /// A new reader over this proc's log.
    ///
    /// The one place the unit and what the config said about it are put
    /// together: a caller that built the reader itself would have to pass
    /// [`parse_ansi`](AppUnitSettings::parse_ansi) by hand, and passing the
    /// wrong one is a screen quietly disagreeing with the file.
    pub fn log_reader(&self) -> LogReader {
        self.unit.log_reader(self.log_reader_settings())
    }

    /// [`log_reader`](AppUnitEntry::log_reader) into a reader that already
    /// exists — see [`Log::reader_into`](crate::log::Log::reader_into).
    pub fn log_reader_into(&self, reader: &mut LogReader) {
        self.unit
            .log_reader_into(reader, self.log_reader_settings());
    }

    /// What the config said, as a reader is told it.
    fn log_reader_settings(&self) -> LogReaderSettings {
        LogReaderSettings {
            parse_ansi: self.settings.parse_ansi,
        }
    }
}

/// What the config said about a proc that the proc itself never acts on.
///
/// Apart from the [`Unit`] because a unit neither draws itself nor reads its
/// own log back: holding these here is what leaves `unit` with no reason to
/// know what a panel is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppUnitSettings {
    /// Which of the two lists on screen it is drawn in.
    pub panel: ConfigPanel,
    /// Whether its escape sequences are read as escape sequences.
    pub parse_ansi: bool,
}

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
/// instead — see [`key`](AppUnitEntry::key).
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
        let mut units: HashMap<AppUnitKey, AppUnitEntry> =
            HashMap::with_capacity(config.procs.len());
        let mut log_capacity_max = 0;
        let mut groups: HashMap<SmallStr, UnitKeyVec> = HashMap::new();
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

            let log_size = proc.log_size(log_size_session);
            let mut unit = Unit::new(Self::build_behavior(proc), log_size);
            unit.set_event_dispatcher(event_dispatcher.clone());
            units.insert(
                key,
                AppUnitEntry {
                    unit,
                    key: proc.key.clone(),
                    settings: AppUnitSettings {
                        panel: proc.panel,
                        parse_ansi: proc.parse_ansi(parse_ansi_session),
                    },
                    dependencies: dependency_graph.dependencies_copy(key).collect(),
                },
            );
            log_capacity_max = log_capacity_max.max(Log::capacity_for_bytes(log_size));
        }
        if !errors.is_empty() {
            return Err(AppConfigError::Errors(errors));
        }
        Ok(Self {
            interner,
            units,
            event_dispatcher,
            log_capacity_max,
            dependency_graph,
            groups,
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

    /// What a proc's `run` or `modes` add up to, with its names and working
    /// directories folded in.
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

    /// Start the unit under `key`, restarting it if it was already running —
    /// see [`Unit::start`](crate::unit::Unit::start), which sees the old run
    /// out before the new one begins.
    pub fn start(&self, key: AppUnitKey) -> Result<(), AppError> {
        let entry = self.entry(key)?;
        entry.unit().start()?;
        Ok(())
    }

    /// Build the unit's run without releasing it, so a unit still waiting on
    /// its dependencies exists and reports
    /// [`Waiting`](crate::runner::RunnerState::Waiting) instead of nothing at all.
    pub fn ensure_created(&self, key: AppUnitKey) -> Result<(), AppError> {
        let entry = self.entry(key)?;
        entry.unit().ensure_created()?;
        Ok(())
    }

    /// Release the run [`ensure_created`](AppUnitMap::ensure_created) parked,
    /// or restart the unit if that run is already gone — what the schedule
    /// gives a unit the caller named, against the
    /// [`ensure_started`](AppUnitMap::ensure_started) a dependency gets.
    pub fn start_or_resume(&self, key: AppUnitKey) -> Result<(), AppError> {
        let entry = self.entry(key)?;
        entry.unit().start_or_resume()?;
        Ok(())
    }

    /// Start `key` if it has never been started, and leave it alone
    /// otherwise — unlike [`start`](AppUnitMap::start), which restarts it.
    pub fn ensure_started(&self, key: AppUnitKey) -> Result<(), AppError> {
        let entry = self.entry(key)?;
        entry.unit().ensure_started()?;
        Ok(())
    }

    /// Stop the unit under `key`.
    ///
    /// Returns without waiting for the process to be gone; the unit keeps
    /// reporting its terminal state through [`Unit::state`](crate::unit::Unit::state).
    pub fn stop(&self, key: AppUnitKey) -> Result<(), AppError> {
        let entry = self.entry(key)?;
        entry.unit().stop();
        Ok(())
    }

    /// How many chunks the largest log here holds.
    ///
    /// The largest and not each, because it is what a caller reusing one
    /// reader across units builds it at: a reader never holds more than the
    /// size it was built for, so a smaller one would follow a shorter tail
    /// than the log it is put on — see [`LogReader`](crate::log::LogReader).
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
        for (value, entry) in &self.units {
            if entry.unit.resolved() {
                resolved.insert(*value);
            }
        }
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
    /// [`stop`](AppUnitMap::stop) only asks and `Drop` cannot await, so this
    /// is the only teardown a caller can observe — what a session that owns
    /// its children wants before its own process exits. Every unit is asked
    /// before any is waited on, so the grace periods overlap instead of
    /// queueing one shutdown at a time.
    pub async fn shutdown(&self) {
        let mut join_set: JoinSet<()> = JoinSet::new();
        for entry in self.units.values() {
            if let Some(handle) = entry.unit.clone_handle() {
                join_set.spawn(async move {
                    handle.abort_and_wait().await;
                });
            };
        }
        while join_set.join_next().await.is_some() {}
    }

    /// Run `f` on the unit under `key`, or say that the key addresses nothing.
    fn with<T>(&self, key: AppUnitKey, f: impl FnOnce(&Unit) -> T) -> Result<T, AppError> {
        let Some(entry) = self.units.get(&key) else {
            return Err(AppError::NotFound);
        };
        Ok(f(&entry.unit))
    }

    /// Everything the session holds about `key`: the unit, what the config
    /// said about it, and what it waits for.
    ///
    /// One lookup for all three, which is what a caller building a row on
    /// screen wants. An unknown key is an error and not an empty answer: it is
    /// a mistake in a config or a command, never a state a unit can be in.
    pub fn entry(&self, key: AppUnitKey) -> Result<&AppUnitEntry, AppError> {
        let Some(entry) = self.units.get(&key) else {
            return Err(AppError::NotFound);
        };
        Ok(entry)
    }
}
