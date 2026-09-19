use smallvec::SmallVec;

use crate::util::{str::SmallStr, types::SmallVecStr};

#[derive(thiserror::Error, Debug)]
pub enum ProcessError {
    #[error("empty process")]
    Empty,
    #[error("error spawning process: {0}")]
    SpawnError(std::io::Error),
    #[error("invalid stdout")]
    InvalidStdout,
    #[error("invalid stderr")]
    InvalidStderr,
    #[error("process was not running")]
    NotRunning,
    #[error("error killing process: {0}")]
    KillError(std::io::Error),
}

#[derive(thiserror::Error, Debug)]
pub enum RunnerError {
    #[error("Error {0}")]
    Process(#[from] ProcessError),
}

/// UnitMap Error
#[derive(Debug, thiserror::Error)]
pub enum UnitError {
    #[error("unit not found")]
    Invalid,
    #[error("unit not found")]
    Runner(RunnerError),
}

/// UnitMap Error
#[derive(Debug, thiserror::Error)]
pub enum UnitMapError {
    #[error("unit not found")]
    NotFound,
    #[error("unit not found")]
    UnitStart(UnitError),
    #[error("unit not found")]
    Unknown,
}

/// UnitMap Error
#[derive(Debug, thiserror::Error)]
pub enum UiError {
    #[error("unit not found")]
    DrawError(std::io::Error),
    #[error("unit not found")]
    EventError(std::io::Error),
    #[error("unit not found")]
    Unknown,
}

/// One thing wrong with a config.
#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("{0}")]
    Unknown(&'static str),
    /// A proc declared `run` and `modes` both.
    #[error("`{0}` declares both `run` and `modes`")]
    RunAndModes(SmallStr),
    /// A proc or a group is named with a character a command line needs for
    /// itself.
    #[error(
        "`{name}` contains `{character}`, which a command line needs to tell a group from a proc"
    )]
    ReservedCharacter { name: SmallStr, character: char },
    /// A proc depends on a name that no proc is declared under.
    #[error("`{proc}` depends on `{depends}`, which is not a proc")]
    UnknownDependency { proc: SmallStr, depends: SmallStr },
    /// Procs that wait on each other in a circle, so none of them can be
    /// first. A cycle of one is a proc that depends on itself.
    #[error("{} depend on each other in a circle", HelperQuoted(.0))]
    Cycle(SmallVecStr),
}

/// One thing wrong with a config.
#[derive(thiserror::Error, Debug)]
pub enum AppErrors {
    #[error("{}", HelperLines(.0, "there were problems with the configuration:"))]
    Errors(SmallVec<[AppError; 8]>),
}

struct HelperQuoted<'a>(&'a SmallVecStr);
impl<'a> std::fmt::Display for HelperQuoted<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, item) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "`{item}`")?;
        }
        Ok(())
    }
}

struct HelperLines<'a, T>(&'a T, &'a str);
impl<'a, T> std::fmt::Display for HelperLines<'a, T>
where
    &'a T: IntoIterator,
    <&'a T as IntoIterator>::Item: std::fmt::Display,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.1)?;
        for error in self.0 {
            write!(f, "\n  - {error}")?;
        }
        Ok(())
    }
}
