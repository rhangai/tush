use std::fmt;

use arcstr::ArcStr;

use crate::util::types::SmallVecArcStr;

/// Everything wrong with a config, reported at once.
///
/// One error so it can travel as one, and a list rather than a string so that
/// whatever reports it decides how each one is shown. Only
/// [`App::new`](crate::app::App::new) builds one, so this is always the full
/// account and never the problem that happened to be noticed first.
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
    /// A proc or a group is named with a character a command line needs for
    /// itself.
    ReservedCharacter { name: ArcStr, character: char },
    /// A proc depends on a name that no proc is declared under.
    UnknownDependency { proc: ArcStr, depends: ArcStr },
    /// Procs that wait on each other in a circle, so none of them can be
    /// first. A cycle of one is a proc that depends on itself.
    Cycle { procs: SmallVecArcStr },
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunAndModes { proc } => {
                write!(f, "`{proc}` declares both `run` and `modes`")
            }
            Self::ReservedCharacter { name, character } => {
                write!(
                    f,
                    "`{name}` contains `{character}`, which a command line needs to tell a group from a proc"
                )
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
