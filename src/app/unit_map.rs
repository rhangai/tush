use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use smallvec::SmallVec;

use crate::{
    app::TARGET_SEPARATOR,
    config::{Config, ConfigProc},
    error::{AppConfigError, AppError, UnitMapError},
    log::LogReader,
    runner::RunnerState,
    unit::{UnitAction, UnitBehavior, UnitChoice, UnitEvent, UnitKey, UnitMap},
    util::{
        event::{EventDispatcher, EventListener},
        graph::{DependencyGraph, DependencyOrder},
        str::SmallStr,
    },
};

/// Every proc of a checked config, as a unit, under the key it was interned
/// as.
///
/// The layer over [`UnitMap`] that owns the interner, and so the only place a
/// name becomes an [`UnitKey`]: everything above it — the screen, the
/// commands it sends — addresses a unit by key and never by text.
pub struct AppUnitMap {
    unit_map: UnitMap,
    dependency_graph: DependencyGraph<UnitKey>,
    /// What each unit depends on directly, answered once here because the
    /// schedule asks it for every pending unit on every wake. Only units that
    /// depend on something are in it.
    dependencies: HashMap<UnitKey, UnitKeyVec>,
    groups: HashMap<SmallStr, UnitKeyVec>,
}

type UnitKeyVec = SmallVec<[UnitKey; 16]>;

impl AppUnitMap {
    /// Check `config`, intern every key, and build a unit per proc.
    ///
    /// Every problem is collected before any of them is reported — see
    /// [`AppConfigError::Errors`].
    pub fn new(config: &Config) -> Result<Self, AppConfigError> {
        let mut errors: Vec<AppConfigError> = Vec::new();
        let mut unit_map = UnitMap::with_capacity(16, config.log_size());

        let dependency_graph = Self::build_dep_graph(&mut errors, &mut unit_map, config);
        let mut groups: HashMap<SmallStr, UnitKeyVec> = HashMap::new();
        for proc in &config.procs {
            let Some(key) = unit_map.key(proc.key.as_ref()) else {
                errors.push(AppConfigError::LogicErrorKey(proc.key.clone()));
                continue;
            };
            let behavior = Self::build_behavior(proc);
            for group in &proc.groups {
                groups.entry(group.clone()).or_default().push(key);
            }
            unit_map.insert(key, behavior);
        }
        if !errors.is_empty() {
            return Err(AppConfigError::Errors(errors));
        }
        let mut dependencies: HashMap<UnitKey, UnitKeyVec> = HashMap::new();
        for key in unit_map.keys() {
            let deps: UnitKeyVec = dependency_graph.dependencies_copy(key).collect();
            if !deps.is_empty() {
                dependencies.insert(key, deps);
            }
        }
        Ok(Self {
            unit_map,
            dependency_graph,
            dependencies,
            groups,
        })
    }

    /// Build the dependency graph, while validating
    fn build_dep_graph(
        errors: &mut Vec<AppConfigError>,
        unit_map: &mut UnitMap,
        config: &Config,
    ) -> DependencyGraph<UnitKey> {
        let mut graph: DependencyGraph<UnitKey> = DependencyGraph::new();
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
            let key = unit_map.reserve(proc.key.as_ref());
            graph.insert(key);
        }
        for proc in &config.procs {
            let Some(key) = unit_map.key(proc.key.as_ref()) else {
                errors.push(AppConfigError::Unknown("key should exist in interner"));
                continue;
            };
            for depends in &proc.depends {
                let Some(depends_key) = unit_map.key(depends) else {
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
                if let Some(key_str) = unit_map.key_str(*key) {
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
            UnitBehavior::run_many(name, Arc::new(run.commands.clone()))
                .with_working_dir(proc.working_dir.clone())
        } else if let Some(modes) = &proc.modes {
            UnitBehavior::modes(
                name,
                modes.iter().map(|mode| {
                    UnitBehavior::run_many(mode.name.clone(), Arc::new(mode.run.commands.clone()))
                        .with_short(mode.name_short.clone())
                        .with_working_dir(
                            mode.working_dir
                                .clone()
                                .or_else(|| proc.working_dir.clone()),
                        )
                }),
            )
        } else {
            UnitBehavior::noop(name)
        };
        behavior.with_short(short)
    }

    /// The key `name` was interned as, if a proc was declared under it.
    ///
    /// The one door from text to [`UnitKey`]: what comes off a command
    /// line or out of a config is a name, and everything past here is a key.
    pub fn key(&self, name: &str) -> Option<UnitKey> {
        self.unit_map.key(name)
    }

    pub fn start(&self, key: UnitKey) -> Result<(), AppError> {
        self.unit_map.start(key)?;
        Ok(())
    }

    pub fn ensure_created(&self, key: UnitKey) -> Result<(), AppError> {
        self.unit_map.ensure_created(key)?;
        Ok(())
    }

    pub fn start_or_resume(&self, key: UnitKey) -> Result<(), AppError> {
        self.unit_map.start_or_resume(key)?;
        Ok(())
    }

    /// Start `key` if it has never been started, and leave it alone
    /// otherwise — unlike [`start`](AppUnitMap::start), which restarts it.
    pub fn ensure_started(&self, key: UnitKey) -> Result<(), AppError> {
        self.unit_map.ensure_started(key)?;
        Ok(())
    }

    pub fn stop(&self, key: UnitKey) -> Result<(), AppError> {
        Ok(self.unit_map.stop(key)?)
    }

    pub fn state(&self, key: UnitKey) -> Result<RunnerState, AppError> {
        Ok(self.unit_map.state(key)?)
    }

    pub fn mode(&self, key: UnitKey) -> Result<Option<SmallStr>, AppError> {
        Ok(self.unit_map.mode(key)?)
    }

    pub fn mode_short(&self, key: UnitKey) -> Result<Option<SmallStr>, AppError> {
        Ok(self.unit_map.mode_short(key)?)
    }

    pub fn name(&self, key: UnitKey) -> Result<SmallStr, AppError> {
        Ok(self.unit_map.name(key)?)
    }

    pub fn name_short(&self, key: UnitKey) -> Result<Option<SmallStr>, AppError> {
        Ok(self.unit_map.name_short(key)?)
    }

    pub fn choices(&self, key: UnitKey, out: &mut Vec<UnitChoice>) -> Result<(), AppError> {
        self.unit_map.choices(key, out)?;
        Ok(())
    }

    pub fn log_reader(&self, key: UnitKey) -> Option<LogReader> {
        self.unit_map.log_reader(key)
    }

    pub fn log_reader_into(&self, key: UnitKey, reader: &mut LogReader) -> bool {
        self.unit_map.log_reader_into(key, reader)
    }

    pub fn log_capacity(&self) -> usize {
        self.unit_map.log_capacity()
    }

    /// Every unit's key, in the `HashMap`'s order, which is to say in none —
    /// a caller showing these to a person has to impose one.
    pub fn keys(&self) -> impl Iterator<Item = UnitKey> {
        self.unit_map.keys()
    }

    /// Fill `resolved` with every unit that has run to the end at least once.
    ///
    /// Clears it first and takes it by reference, so the loop that asks on
    /// every wake keeps one set instead of building a new one each time.
    pub fn write_resolved(&self, resolved: &mut HashSet<UnitKey>) {
        self.unit_map.write_resolved(resolved);
    }

    /// What `key` must wait for, one edge out.
    ///
    /// `None` is "nothing to wait for" — both a unit that depends on nothing
    /// and a key no unit was declared under, which want the same answer.
    pub fn direct_dependencies(&self, key: UnitKey) -> Option<&UnitKeyVec> {
        self.dependencies.get(&key)
    }

    /// Every unit declared under a group name, or `None` if none was.
    pub fn group(&self, group: &str) -> Option<&UnitKeyVec> {
        self.groups.get(group)
    }

    /// Everything `keys` need, transitively, in an order that starts them —
    /// see [`resolve_from_many`](DependencyGraph::resolve_from_many).
    pub fn resolve_dependency_chain(&self, keys: &[UnitKey]) -> DependencyOrder<UnitKey> {
        self.dependency_graph.resolve_from_many(keys)
    }

    pub fn create_listener(&self) -> EventListener {
        self.unit_map.create_listener()
    }

    pub fn event_dispatcher(&self) -> &EventDispatcher {
        self.unit_map.event_dispatcher()
    }

    /// Hand `event` to the unit under `key` and report back what it asks for.
    ///
    /// Reported and not done: a start has to go through the schedule, which
    /// is above this layer, so the action is carried out by
    /// [`App::dispatch`](crate::app::App::dispatch).
    pub fn dispatch(
        &self,
        key: UnitKey,
        event: UnitEvent,
    ) -> Result<Option<UnitAction>, UnitMapError> {
        self.unit_map.dispatch(key, event)
    }

    pub async fn shutdown(&self) {
        self.unit_map.shutdown().await;
    }
}
