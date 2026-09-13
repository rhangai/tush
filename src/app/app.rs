use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::{Result, bail};
use arcstr::ArcStr;

use crate::{
    app::{
        error::{AppError, AppErrors},
        target::{TARGET_SEPARATOR, Target},
    },
    config::{Config, ConfigProc},
    runner::RunnerHandle,
    unit::{UnitAction, UnitBehavior, UnitEvent, UnitMap},
    util::{graph::DependencyGraph, types::SmallVecArcStr},
};

/// A session that has been checked and is ready to be run.
///
/// The type is the proof: an `App` is a config that survived
/// [`new`](App::new), so anything holding one can stop asking whether the
/// procs it names exist or whether their dependencies can be satisfied.
///
/// The start order is not here yet.
pub struct App {
    /// Every proc as a [`Unit`](crate::unit::Unit), by its key.
    units: Arc<UnitMap>,
    /// Group name to the keys declared under it — inverted from how the
    /// config writes it, a config being written per proc and used per group.
    groups: HashMap<ArcStr, SmallVecArcStr>,
}

impl App {
    /// Check a config, and build the session from it.
    ///
    /// Checked: that no proc declares both `run` and `modes`, that every
    /// `depends` names a proc that exists, and that nothing depends on itself
    /// directly or through others.
    ///
    /// None of it stops at the first failure — see [`AppErrors`]. Which is
    /// also why a missing dependency does not prevent the cycle check: the
    /// edge is simply not added, so both kinds of problem come back together.
    pub fn new(config: &Config) -> Result<Self> {
        if let Some(error) = Self::validate_config(config) {
            return Err(error.into());
        }

        let mut behaviors: HashMap<ArcStr, UnitBehavior> =
            HashMap::with_capacity(config.procs.len());
        let mut groups: HashMap<ArcStr, SmallVecArcStr> = HashMap::new();

        for proc in &config.procs {
            let behavior = Self::get_behavior(proc);
            for group in &proc.groups {
                groups
                    .entry(group.clone())
                    .or_default()
                    .push(proc.key.clone());
            }
            // The keys come from a mapping, so they are unique and the
            // `None` that a taken name would give back cannot happen here.
            behaviors.insert(proc.key.clone(), behavior);
        }

        Ok(Self {
            units: UnitMap::new(behaviors),
            groups,
        })
    }

    /// Get the behavior from the config
    fn get_behavior(proc: &ConfigProc) -> UnitBehavior {
        let name = proc.display_name();
        let short = proc.name_short.clone();

        let behavior = {
            if let Some(run) = &proc.run {
                UnitBehavior::run_many(name, run.commands.clone())
            } else if let Some(modes) = &proc.modes {
                return UnitBehavior::modes(
                    name,
                    modes.iter().map(|mode| {
                        UnitBehavior::run_many(mode.name.clone(), mode.run.commands.clone())
                            .with_short(mode.name_short.clone())
                    }),
                );
            } else {
                UnitBehavior::noop(name)
            }
        };
        behavior.with_short(short)
    }

    /// Hand `event` to the unit under `name` and carry out what it asks for.
    ///
    /// The behavior decides, which is why this is not two methods: an event
    /// may move a unit onto another mode before the start it also asks for,
    /// and only the behavior can do that.
    pub fn dispatch(&self, name: &str, event: UnitEvent) -> Result<Option<Arc<RunnerHandle>>> {
        let Some(action) = self.units().dispatch(name, event)? else {
            return Ok(None);
        };
        match action {
            UnitAction::Start => {
                let handle = self.units().start(name)?;
                Ok(Some(handle))
            }
            UnitAction::Stop => {
                self.units().stop(name)?;
                Ok(None)
            }
        }
    }

    /// Every proc as a [`Unit`](crate::unit::Unit), by its key.
    pub fn units(&self) -> &Arc<UnitMap> {
        &self.units
    }

    /// The keys belonging to each group.
    pub fn groups(&self) -> &HashMap<ArcStr, SmallVecArcStr> {
        &self.groups
    }

    /// The units `targets` name, in the order they were named, without
    /// repeats.
    ///
    /// A group expands to what was declared under it, in config order. The
    /// order overall is the command line's, that being the one the person who
    /// typed it has in mind.
    ///
    /// A unit named twice is kept once: `group:web server-main` with
    /// `server-main` in `web` would otherwise start it twice, and the second
    /// start kills the first.
    ///
    /// A name that is not there is an error, and all of them at once — the
    /// same reason [`new`](App::new) reports every problem with a config.
    pub fn resolve(&self, targets: &[Target]) -> Result<SmallVecArcStr> {
        let mut keys: SmallVecArcStr = SmallVecArcStr::new();
        let mut unknown: Vec<String> = Vec::new();

        for target in targets {
            match target {
                Target::Unit(key) => match self.units.contains(key) {
                    true => push_once(&mut keys, key),
                    false => unknown.push(format!("`{key}` is not a proc")),
                },
                Target::Group(group) => match self.groups.get(group) {
                    Some(members) => {
                        for key in members {
                            push_once(&mut keys, key);
                        }
                    }
                    None => unknown.push(format!("`{group}` is not a group")),
                },
            }
        }

        if !unknown.is_empty() {
            bail!("{}", unknown.join("\n"));
        }
        Ok(keys)
    }

    /// Everything wrong with `config`, in the order it was found.
    ///
    /// Apart from [`new`](App::new) because checking borrows every proc's
    /// name to build the graph while building takes them apart. Finishing the
    /// first means the second never has to wonder.
    fn validate_config(config: &Config) -> Option<AppErrors> {
        let mut errors = Vec::new();
        let declared: HashSet<&ArcStr> = config.procs.iter().map(|proc| &proc.key).collect();

        let mut graph: DependencyGraph<&ArcStr> = DependencyGraph::new();
        for proc in &config.procs {
            // Even a proc nothing mentions has to be in the graph, or it
            // would not be in the order that comes out of it.
            graph.insert(&proc.key);

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
                errors.push(AppError::RunAndModes {
                    proc: proc.key.clone(),
                });
            }

            for depends in &proc.depends {
                if !declared.contains(&depends) {
                    errors.push(AppError::UnknownDependency {
                        proc: proc.key.clone(),
                        depends: depends.clone(),
                    });
                    // Adding the edge anyway would invent the node it points
                    // at, and the phantom would then turn up in the start
                    // order as a proc nobody declared.
                    continue;
                }
                graph.add_dependency(&proc.key, depends);
            }
        }

        // `resolve` never fails; the cycles are what it had to break to
        // produce an order. The order itself is thrown away here, and will be
        // what `App` keeps once there is something to start.
        let resolved = graph.resolve();
        errors.extend(resolved.cycles().map(|cycle| AppError::Cycle {
            procs: cycle.iter().map(|i| (*i).clone()).collect(),
        }));

        if errors.is_empty() {
            None
        } else {
            Some(AppErrors::new(errors))
        }
    }
}

/// Add `key` unless it is already there. Linear: these lists are a command
/// line long.
fn push_once(keys: &mut SmallVecArcStr, key: &ArcStr) {
    if !keys.contains(key) {
        keys.push(key.clone());
    }
}
