use std::fmt;

use arcstr::ArcStr;

/// Everything wrong with a config, reported at once.
///
/// One error rather than many so it can travel as an error, and a list rather
/// than a string so that whatever reports it — a line on stderr now, a panel
/// in the interface later — can decide how each one is shown.
///
/// Only [`App::new`](crate::app::App::new) builds one, so an `AppErrors` in
/// hand is always the full account of a config that was rejected, never a
/// single problem that happened to be noticed first.
#[derive(Debug)]
pub struct AppErrors {
    errors: Vec<AppError>,
}

impl AppErrors {
    /// Collect the problems found while checking one config.
    pub(super) fn new(errors: Vec<AppError>) -> Self {
        Self { errors }
    }

    /// The problems, in the order they were found: per proc as the config
    /// declares them, and then the cycles.
    pub fn errors(&self) -> &[AppError] {
        &self.errors
    }
}

impl fmt::Display for AppErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.errors.len();
        let plural = if count == 1 { "" } else { "s" };
        write!(f, "{count} problem{plural} with the configuration:")?;
        for error in &self.errors {
            write!(f, "\n  - {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for AppErrors {}

/// One thing wrong with a config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppError {
    /// A proc declared `run` and `modes` both.
    RunAndModes { proc: ArcStr },
    /// A proc depends on a name that no proc is declared under.
    UnknownDependency { proc: ArcStr, depends: ArcStr },
    /// Procs that wait on each other in a circle, so none of them can be
    /// first. A cycle of one is a proc that depends on itself.
    Cycle { procs: Vec<ArcStr> },
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunAndModes { proc } => {
                write!(f, "`{proc}` declares both `run` and `modes`")
            }
            Self::UnknownDependency { proc, depends } => {
                write!(f, "`{proc}` depends on `{depends}`, which is not a proc")
            }
            Self::Cycle { procs } if procs.len() == 1 => {
                write!(f, "`{}` depends on itself", procs[0])
            }
            Self::Cycle { procs } => {
                let procs: Vec<_> = procs.iter().map(|proc| format!("`{proc}`")).collect();
                write!(f, "{} depend on each other in a circle", procs.join(", "))
            }
        }
    }
}

impl std::error::Error for AppError {}
