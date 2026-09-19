use std::{collections::HashMap, sync::Arc};

use smallvec::SmallVec;
use string_interner::{DefaultStringInterner, DefaultSymbol};

use crate::{
    app::TARGET_SEPARATOR,
    config::{Config, ConfigProc},
    error::{AppConfigError, AppError},
    log::LogReader,
    runner::RunnerState,
    unit::{UnitAction, UnitBehavior, UnitChoice, UnitEvent, UnitMap},
    util::{graph::DependencyGraph, str::SmallStr},
};

/// Every proc of a checked config, as a unit, under the key it was interned
/// as.
///
/// The layer over [`UnitMap`] that owns the interner, and so the only place a
/// name becomes an [`AppUnitKey`]: everything above it — the screen, the
/// commands it sends — addresses a unit by key and never by text.
pub struct AppUnitMap {
    interner: DefaultStringInterner,
    unit_map: Arc<UnitMap<DefaultSymbol>>,
    dependency_graph: DependencyGraph<DefaultSymbol>,
    groups: GroupHashMap,
}

/// How a unit is addressed once the config has been checked.
///
/// The interned key rather than the name it was interned from: a key is read
/// out of a row, copied into a command and compared every frame, and a name
/// there is a string to clone and hash where this is an integer.
///
/// The symbol is private, so every key that exists came from
/// [`key`](AppUnitMap::key) or [`keys`](AppUnitMap::keys) — which is to say
/// from the one interner it means anything against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppUnitKey {
    value: DefaultSymbol,
}

type GroupHashMap = HashMap<DefaultSymbol, SmallVec<[DefaultSymbol; 16]>>;

impl AppUnitMap {
    /// Check `config`, intern every key, and build a unit per proc.
    ///
    /// Every problem is collected before any of them is reported — see
    /// [`AppConfigError::Errors`].
    pub fn new(config: &Config) -> Result<Self, AppConfigError> {
        let mut errors: Vec<AppConfigError> = Vec::new();
        let mut interner: DefaultStringInterner = DefaultStringInterner::new();

        let dependency_graph = Self::build_dep_graph(&mut errors, &mut interner, config);
        let mut behaviors: HashMap<DefaultSymbol, UnitBehavior> =
            HashMap::with_capacity(config.procs.len());
        let mut groups: GroupHashMap = HashMap::new();
        for proc in &config.procs {
            let Some(key) = interner.get(proc.key.as_ref()) else {
                errors.push(AppConfigError::LogicErrorKey(proc.key.clone()));
                continue;
            };
            let behavior = Self::build_behavior(proc);
            for group in &proc.groups {
                let group_key = interner.get_or_intern(group.as_ref());
                groups.entry(group_key).or_default().push(key);
            }
            behaviors.insert(key, behavior);
        }
        if !errors.is_empty() {
            return Err(AppConfigError::Errors(errors));
        }
        Ok(Self {
            interner,
            unit_map: UnitMap::new(behaviors),
            dependency_graph,
            groups,
        })
    }

    /// Build the dependency graph, while validating
    fn build_dep_graph(
        errors: &mut Vec<AppConfigError>,
        units_interner: &mut DefaultStringInterner,
        config: &Config,
    ) -> DependencyGraph<DefaultSymbol> {
        let mut graph: DependencyGraph<DefaultSymbol> = DependencyGraph::new();
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
            let key = units_interner.get_or_intern(proc.key.as_ref());
            graph.insert(key);
        }
        for proc in &config.procs {
            let Some(key) = units_interner.get(proc.key.as_ref()) else {
                errors.push(AppConfigError::Unknown("key should exist in interner"));
                continue;
            };
            for depends in &proc.depends {
                let Some(depends_key) = units_interner.get(depends) else {
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
                if let Some(key_str) = units_interner.resolve(*key) {
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
    /// The one door from text to [`AppUnitKey`]: what comes off a command
    /// line or out of a config is a name, and everything past here is a key.
    pub fn key(&self, name: &str) -> Result<AppUnitKey, AppError> {
        self.interner
            .get(name)
            .map(|value| AppUnitKey { value })
            .ok_or_else(|| AppError::InvalidKey(name.into()))
    }

    pub fn start(&self, key: AppUnitKey) -> Result<(), AppError> {
        self.unit_map.start(&key.value)?;
        Ok(())
    }

    pub fn stop(&self, key: AppUnitKey) -> Result<(), AppError> {
        Ok(self.unit_map.stop(&key.value)?)
    }

    pub fn state(&self, key: AppUnitKey) -> Result<RunnerState, AppError> {
        Ok(self.unit_map.state(&key.value)?)
    }

    pub fn mode(&self, key: AppUnitKey) -> Result<Option<SmallStr>, AppError> {
        Ok(self.unit_map.mode(&key.value)?)
    }

    pub fn mode_short(&self, key: AppUnitKey) -> Result<Option<SmallStr>, AppError> {
        Ok(self.unit_map.mode_short(&key.value)?)
    }

    pub fn name(&self, key: AppUnitKey) -> Result<SmallStr, AppError> {
        Ok(self.unit_map.name(&key.value)?)
    }

    pub fn name_short(&self, key: AppUnitKey) -> Result<Option<SmallStr>, AppError> {
        Ok(self.unit_map.name_short(&key.value)?)
    }

    pub fn choices(&self, key: AppUnitKey, out: &mut Vec<UnitChoice>) -> Result<(), AppError> {
        self.unit_map.choices(&key.value, out)?;
        Ok(())
    }

    pub fn log_reader(&self, key: AppUnitKey) -> Option<LogReader> {
        self.unit_map.log_reader(&key.value)
    }

    /// Every unit's key, in the `HashMap`'s order, which is to say in none —
    /// a caller showing these to a person has to impose one.
    pub fn keys(&self) -> impl Iterator<Item = AppUnitKey> {
        self.unit_map.keys().map(|k| AppUnitKey { value: *k })
    }

    /// Hand `event` to the unit under `key` and carry out what it asks for.
    ///
    /// The behavior decides, which is why this is not two methods: an event
    /// may move a unit onto another mode before the start it also asks for,
    /// and only the behavior can do that.
    pub fn dispatch(&self, key: AppUnitKey, event: UnitEvent) -> anyhow::Result<()> {
        let Some(action) = self.unit_map.dispatch(&key.value, event)? else {
            return Ok(());
        };
        match action {
            UnitAction::Start => {
                _ = self.unit_map.start(&key.value)?;
                Ok(())
            }
            UnitAction::Stop => {
                self.unit_map.stop(&key.value)?;
                Ok(())
            }
        }
    }

    pub async fn shutdown(&self) {
        self.unit_map.shutdown().await;
    }
}
