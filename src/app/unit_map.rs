use std::{collections::HashMap, sync::Arc};

use smallvec::SmallVec;

use crate::{
    app::TARGET_SEPARATOR,
    config::{Config, ConfigProc},
    error::{AppConfigError, AppError},
    log::LogReader,
    runner::RunnerState,
    unit::{UnitAction, UnitBehavior, UnitChoice, UnitEvent, UnitKey, UnitMap},
    util::{graph::DependencyGraph, str::SmallStr},
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
    groups: GroupHashMap,
}

type GroupHashMap = HashMap<SmallStr, SmallVec<[UnitKey; 16]>>;

impl AppUnitMap {
    /// Check `config`, intern every key, and build a unit per proc.
    ///
    /// Every problem is collected before any of them is reported — see
    /// [`AppConfigError::Errors`].
    pub fn new(config: &Config) -> Result<Self, AppConfigError> {
        let mut errors: Vec<AppConfigError> = Vec::new();
        let mut unit_map = UnitMap::with_capacity(16);

        let dependency_graph = Self::build_dep_graph(&mut errors, &mut unit_map, config);
        let mut groups: GroupHashMap = HashMap::new();
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
        Ok(Self {
            unit_map,
            dependency_graph,
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
        let behavior = if let Some(run) = &proc.run {
            UnitBehavior::run_many(name, Arc::new(run.commands.clone()))
        } else if let Some(modes) = &proc.modes {
            UnitBehavior::modes(
                name,
                modes.iter().map(|mode| {
                    UnitBehavior::run_many(mode.name.clone(), Arc::new(mode.run.commands.clone()))
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
    /// The one door from text to [`UnitKey`]: what comes off a command
    /// line or out of a config is a name, and everything past here is a key.
    pub fn key(&self, name: &str) -> Option<UnitKey> {
        self.unit_map.key(name)
    }

    pub fn start(&self, key: UnitKey) -> Result<(), AppError> {
        self.unit_map.start(key)?;
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

    /// Every unit's key, in the `HashMap`'s order, which is to say in none —
    /// a caller showing these to a person has to impose one.
    pub fn keys(&self) -> impl Iterator<Item = UnitKey> {
        self.unit_map.keys()
    }

    /// Hand `event` to the unit under `key` and carry out what it asks for.
    ///
    /// The behavior decides, which is why this is not two methods: an event
    /// may move a unit onto another mode before the start it also asks for,
    /// and only the behavior can do that.
    pub fn dispatch(&self, key: UnitKey, event: UnitEvent) -> anyhow::Result<()> {
        let Some(action) = self.unit_map.dispatch(key, event)? else {
            return Ok(());
        };
        match action {
            UnitAction::Start => {
                _ = self.unit_map.start(key)?;
                Ok(())
            }
            UnitAction::Stop => {
                self.unit_map.stop(key)?;
                Ok(())
            }
        }
    }

    pub async fn shutdown(&self) {
        self.unit_map.shutdown().await;
    }
}
