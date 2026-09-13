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
    util::graph::DependencyGraph,
};

/// A session that has been checked and is ready to be run.
///
/// The type is the proof. A `Config` is whatever the file said; an `App` is a
/// config that has survived [`new`](App::new), so anything holding one can
/// stop asking whether the procs it names exist or whether their dependencies
/// can be satisfied — the questions were answered once, at the door.
///
/// What it holds is the config turned into the things a run actually uses:
/// the [`units`](App::units), and the [`groups`](App::groups) that address
/// them in bulk. The start order is not here yet.
pub struct App {
    /// Every proc as a [`Unit`](crate::unit::Unit), by its key.
    units: Arc<UnitMap>,
    /// Group name to the keys declared under it.
    ///
    /// Built while the units are, so a group only ever names units that were
    /// added — and inverted from how the config writes it, because a config
    /// is written per proc and a group is used per group.
    groups: HashMap<ArcStr, Vec<ArcStr>>,
}

impl App {
    /// Check a config, and build the session from it.
    ///
    /// # What is checked
    ///
    /// - no proc declares both `run` and `modes`, which would leave it
    ///   ambiguous what a plain start means;
    /// - every `depends` names a proc that exists;
    /// - nothing depends on itself, directly or through others.
    ///
    /// # Every problem, not the first one
    ///
    /// The checks do not stop at the first failure. A config with three
    /// mistakes in it is a config somebody is about to fix, and telling them
    /// about one mistake per run is three runs of the same discovery. The
    /// error returned holds all of them — see [`AppErrors`].
    ///
    /// Which is also why a missing dependency does not prevent the cycle
    /// check: the edge is simply not added, the graph stays honest about what
    /// it knows, and both kinds of problem come back together.
    pub fn new(config: &Config) -> Result<Self> {
        if let Some(error) = Self::validate_config(config) {
            return Err(error.into());
        }

        let mut behaviors: HashMap<ArcStr, UnitBehavior> = HashMap::new();
        let mut groups: HashMap<ArcStr, Vec<ArcStr>> = HashMap::new();

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
        if let Some(run) = &proc.run {
            return UnitBehavior::run_many(name, run.0.clone());
        }
        if let Some(modes) = &proc.modes {
            return UnitBehavior::modes(
                name,
                modes
                    .iter()
                    .map(|mode| UnitBehavior::run_many(mode.name.as_str(), mode.run.0.clone()))
                    .collect(),
            );
        }
        UnitBehavior::noop(name)
    }

    /// Every proc as a [`Unit`](crate::unit::Unit), by its key.
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
    pub fn groups(&self) -> &HashMap<ArcStr, Vec<ArcStr>> {
        &self.groups
    }

    /// The units `targets` name, in the order they were named, without
    /// repeats.
    ///
    /// A group expands to what was declared under it. Order is the command
    /// line's, because that is the one the person who typed it has in mind;
    /// within a group it is the order the config declared them, which is as
    /// good an answer as any until there is a start order to ask instead.
    ///
    /// # Named twice
    ///
    /// Kept once. `group:web server-main` where `server-main` is in `web` is
    /// somebody naming a group and then a member of it, and starting it twice
    /// would restart it — the second start would kill the first, which is the
    /// opposite of what they asked for.
    ///
    /// # Named and not there
    ///
    /// An error, and all of them at once. A command line with two typos in it
    /// is about to be retyped, and telling somebody about one typo per run is
    /// two runs of the same discovery — the same reason
    /// [`new`](App::new) reports every problem with a config.
    pub fn resolve(&self, targets: &[Target]) -> Result<Vec<ArcStr>> {
        let mut keys: Vec<ArcStr> = Vec::new();
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
    /// Separate from [`new`](App::new) because the two halves want the config
    /// differently: checking reads every proc and borrows their names to
    /// build the graph, while building takes them apart. Doing the first to
    /// completion means the second never has to wonder.
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

/// Add `key` unless it is already there.
///
/// Linear, because the lists this is building are a command line long.
fn push_once(keys: &mut Vec<ArcStr>, key: &ArcStr) {
    if !keys.contains(key) {
        keys.push(key.clone());
    }
}
