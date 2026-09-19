use std::{collections::HashMap, sync::Arc};

use smallvec::SmallVec;
use string_interner::{DefaultStringInterner, DefaultSymbol};

use crate::{
    app::TARGET_SEPARATOR,
    config::{Config, ConfigProc},
    error::AppError,
    unit::{UnitAction, UnitBehavior, UnitEvent, UnitMap},
    util::{graph::DependencyGraph, str::SmallStr},
};

pub struct AppUnitMap {
    interner: DefaultStringInterner,
    units: Arc<UnitMap<DefaultSymbol>>,
    dependency_graph: DependencyGraph<DefaultSymbol>,
    groups: GroupHashMap,
}

type GroupHashMap = HashMap<DefaultSymbol, SmallVec<[DefaultSymbol; 16]>>;

impl AppUnitMap {
    pub fn new(config: &Config) -> Result<Self, AppError> {
        let mut errors: Vec<AppError> = Vec::new();
        let mut interner: DefaultStringInterner = DefaultStringInterner::new();

        let dependency_graph = Self::build_dep_graph(&mut errors, &mut interner, config);
        let mut behaviors: HashMap<DefaultSymbol, UnitBehavior> =
            HashMap::with_capacity(config.procs.len());
        let mut groups: GroupHashMap = HashMap::new();
        for proc in &config.procs {
            let Some(key) = interner.get(proc.key.as_ref()) else {
                errors.push(AppError::UnknownKey(proc.key.clone()));
                continue;
            };
            let behavior = Self::build_behavior(proc);
            for group in &proc.groups {
                let Some(group_key) = interner.get(group.as_ref()) else {
                    errors.push(AppError::UnknownKey(group.clone()));
                    continue;
                };
                groups.entry(group_key).or_default().push(key);
            }
            behaviors.insert(key, behavior);
        }
        Ok(Self {
            interner,
            units: UnitMap::new(behaviors),
            dependency_graph,
            groups,
        })
    }

    /// Build the dependency graph, while validating
    fn build_dep_graph(
        errors: &mut Vec<AppError>,
        interner: &mut DefaultStringInterner,
        config: &Config,
    ) -> DependencyGraph<DefaultSymbol> {
        let mut graph: DependencyGraph<DefaultSymbol> = DependencyGraph::new();
        for proc in &config.procs {
            if proc.key.contains(TARGET_SEPARATOR) {
                errors.push(AppError::ReservedCharacter {
                    name: proc.key.clone(),
                    character: TARGET_SEPARATOR,
                });
            }
            for group in &proc.groups {
                if group.contains(TARGET_SEPARATOR) {
                    errors.push(AppError::ReservedCharacter {
                        name: group.clone(),
                        character: TARGET_SEPARATOR,
                    });
                }
            }
            if proc.run.is_some() && proc.modes.is_some() {
                errors.push(AppError::RunAndModes(proc.key.clone()));
            }
            let key = interner.get_or_intern(proc.key.as_ref());
            graph.insert(key);
        }
        for proc in &config.procs {
            let Some(key) = interner.get(proc.key.as_ref()) else {
                errors.push(AppError::Unknown("key should exist in interner"));
                continue;
            };
            for depends in &proc.depends {
                let Some(depends_key) = interner.get(depends) else {
                    errors.push(AppError::UnknownDependency {
                        proc: proc.key.clone(),
                        depends: depends.clone(),
                    });
                    continue;
                };
                if !graph.contains(&depends_key) {
                    errors.push(AppError::UnknownDependency {
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
                if let Some(key_str) = interner.resolve(*key) {
                    cycle_procs.push(SmallStr::new(key_str));
                };
            }
            errors.push(AppError::Cycle(cycle_procs))
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

    fn unit_key(&self, name: &str) -> Result<DefaultSymbol, AppError> {
        self.interner
            .get(name)
            .ok_or_else(|| AppError::UnknownKey(name.into()))
    }

    pub fn start(&self, name: &str) -> Result<(), AppError> {
        let key = self.unit_key(name)?;
        self.units.start(&key);
        Ok(())
    }

    /// Hand `event` to the unit under `name` and carry out what it asks for.
    ///
    /// The behavior decides, which is why this is not two methods: an event
    /// may move a unit onto another mode before the start it also asks for,
    /// and only the behavior can do that.
    pub fn dispatch(&self, name: &str, event: UnitEvent) -> anyhow::Result<()> {
        let key = self.unit_key(name)?;
        let Some(action) = self.units.dispatch(&key, event)? else {
            return Ok(());
        };
        match action {
            UnitAction::Start => {
                _ = self.units.start(&key)?;
                Ok(())
            }
            UnitAction::Stop => {
                self.units.stop(&key)?;
                Ok(())
            }
        }
    }

    pub async fn shutdown(&self) {
        self.units.shutdown().await;
    }
}
