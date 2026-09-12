use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::Result;

use crate::{
    app::error::{AppError, AppErrors},
    config::Config,
    unit::{UnitBehavior, UnitMap},
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
    groups: HashMap<String, Vec<String>>,
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
    pub fn new(config: Config) -> Result<Self> {
        let errors = Self::check(&config);
        if !errors.is_empty() {
            return Err(AppErrors::new(errors).into());
        }

        let mut behaviors: HashMap<String, UnitBehavior> = HashMap::new();
        let mut groups: HashMap<String, Vec<String>> = HashMap::new();

        for proc in config.procs {
            // Borrowed before the fields start moving out from under it.
            let name = proc.display_name().to_owned();

            let behavior = match (proc.run, proc.modes) {
                // `run` and `modes` together was refused above, so the `_`
                // here is only ever `None`; spelling it out as a catch-all
                // keeps the match total without an `unreachable!` that would
                // turn a checking mistake into a panic.
                (Some(run), _) => UnitBehavior::run_many(name, run.0),
                (None, Some(modes)) => UnitBehavior::modes(
                    name,
                    modes
                        .into_iter()
                        .map(|mode| UnitBehavior::run_many(mode.name, mode.run.0))
                        .collect(),
                ),
                // A proc that says neither is not an error, only a proc with
                // nothing to start — the config layer leaves that open on
                // purpose, and the noop runner is what it means.
                (None, None) => UnitBehavior::noop(name),
            };

            for group in proc.groups {
                groups.entry(group).or_default().push(proc.key.clone());
            }
            // The keys come from a mapping, so they are unique and the
            // `None` that a taken name would give back cannot happen here.
            behaviors.insert(proc.key, behavior);
        }

        Ok(Self {
            units: UnitMap::new(behaviors),
            groups,
        })
    }

    /// Every proc as a [`Unit`](crate::unit::Unit), by its key.
    pub fn units(&self) -> &Arc<UnitMap> {
        &self.units
    }

    /// The keys belonging to each group.
    pub fn groups(&self) -> &HashMap<String, Vec<String>> {
        &self.groups
    }

    /// Everything wrong with `config`, in the order it was found.
    ///
    /// Separate from [`new`](App::new) because the two halves want the config
    /// differently: checking reads every proc and borrows their names to
    /// build the graph, while building takes them apart. Doing the first to
    /// completion means the second never has to wonder.
    fn check(config: &Config) -> Vec<AppError> {
        let mut errors = Vec::new();
        let declared: HashSet<&str> = config.procs.iter().map(|proc| proc.key.as_str()).collect();

        let mut graph = DependencyGraph::new();
        for proc in &config.procs {
            // Even a proc nothing mentions has to be in the graph, or it
            // would not be in the order that comes out of it.
            graph.insert(proc.key.as_str());

            if proc.run.is_some() && proc.modes.is_some() {
                errors.push(AppError::RunAndModes {
                    proc: proc.key.clone(),
                });
            }

            for depends in &proc.depends {
                if !declared.contains(depends.as_str()) {
                    errors.push(AppError::UnknownDependency {
                        proc: proc.key.clone(),
                        depends: depends.clone(),
                    });
                    // Adding the edge anyway would invent the node it points
                    // at, and the phantom would then turn up in the start
                    // order as a proc nobody declared.
                    continue;
                }
                graph.add_dependency(proc.key.as_str(), depends.as_str());
            }
        }

        // `resolve` never fails; the cycles are what it had to break to
        // produce an order. The order itself is thrown away here, and will be
        // what `App` keeps once there is something to start.
        let resolved = graph.resolve();
        errors.extend(resolved.cycles().map(|cycle| AppError::Cycle {
            procs: cycle.iter().map(|key| (*key).to_owned()).collect(),
        }));

        errors
    }
}
