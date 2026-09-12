use std::collections::HashSet;

use anyhow::Result;

use crate::{
    app::error::{AppError, AppErrors},
    config::Config,
    util::graph::DependencyGraph,
};

/// A session that has been checked and is ready to be run.
///
/// Empty for now: it exists to be the thing a [`Config`] becomes once it has
/// been found sound. Everything a run needs — the units, the start order, the
/// groups — lands here as it is built.
///
/// The type is the proof. A `Config` is whatever the file said; an `App` is a
/// config that has survived [`new`](App::new), so anything holding one can
/// stop asking whether the procs it names exist or whether their dependencies
/// can be satisfied.
pub struct App {}

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

        if errors.is_empty() {
            Ok(Self {})
        } else {
            Err(AppErrors::new(errors).into())
        }
    }
}
