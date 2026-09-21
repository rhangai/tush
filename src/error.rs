//! Every error type in the crate, gathered rather than kept beside the code
//! that returns them.

use crate::util::str::SmallStr;

/// What starting or killing a child process can fail with.
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

/// What supervising one run can fail with — so far, only the process itself.
#[derive(thiserror::Error, Debug)]
pub enum RunnerError {
    #[error("Error {0}")]
    Process(#[from] ProcessError),
}

/// What starting a unit can fail with: a command with nothing in it, or
/// the run underneath.
#[derive(Debug, thiserror::Error)]
pub enum UnitError {
    #[error("unit not found")]
    Invalid,
    #[error("unit already started")]
    AlreadyStarted,
    #[error("unit not found")]
    Runner(RunnerError),
}

/// What addressing a unit through a [`UnitMap`](crate::unit::UnitMap) can
/// fail with.
#[derive(Debug, thiserror::Error)]
pub enum UnitMapError {
    #[error("unit not found")]
    NotFound,
    #[error("unit not found")]
    UnitStart(UnitError),
    #[error("unit not found")]
    Unknown,
}

/// What takes the screen down: the terminal, never the session behind it.
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
pub enum AppConfigError {
    /// An invariant of the checking itself that did not hold, named at the
    /// place it broke. Not a mistake in the config.
    #[error("{0}")]
    Unknown(&'static str),
    /// A key that was interned and did not come back — the same kind of
    /// thing, said about one key.
    #[error("key `{0}` should exist")]
    LogicErrorKey(SmallStr),
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
    Cycle(Vec<SmallStr>),
    /// Every problem one pass over the config found, because a config with
    /// three mistakes in it is about to be fixed and one mistake per run is
    /// three runs of the same discovery.
    #[error("{}", HelperLines(.0, "there were problems with the configuration:"))]
    Errors(Vec<AppConfigError>),
}

/// What addressing a unit in a session that is already built can fail with.
///
/// Apart from [`AppConfigError`], which is everything the checking found: by
/// the time there is a session to address, those questions are answered.
#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("key `{0}` does not exist")]
    InvalidKey(SmallStr),
    #[error("{0}")]
    MapError(#[from] UnitMapError),
}

struct HelperQuoted<'a, T>(&'a T);
impl<'a, T> std::fmt::Display for HelperQuoted<'a, T>
where
    &'a T: IntoIterator,
    <&'a T as IntoIterator>::Item: std::fmt::Display,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, item) in self.0.into_iter().enumerate() {
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

/// What stops a server from listening.
///
/// Never a client: one connection going wrong is that connection's problem,
/// and a session with no screen carries on without it.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("could not prepare {0}: {1}")]
    Directory(std::path::PathBuf, std::io::Error),
    #[error("a server is already listening on {0}")]
    AlreadyRunning(std::path::PathBuf),
    #[error("could not listen on {0}: {1}")]
    Bind(std::path::PathBuf, std::io::Error),
    #[error("stopped accepting connections: {0}")]
    Accept(std::io::Error),
}

/// What stops a screen from following a session over a socket.
///
/// Only [`connect`](crate::view::ViewSocket::connect) hands one to a caller;
/// after that they are the task's, and what a screen does about one is show
/// the frame it already had — see [`ViewClient`](crate::view::ViewClient),
/// which neither waits nor fails.
#[derive(Debug, thiserror::Error)]
pub enum ViewSocketError {
    #[error("could not reach the session: {0}")]
    Connect(std::io::Error),
    #[error("the session answered badly: {0}")]
    Http(hyper::Error),
    #[error("could not build the request: {0}")]
    Request(hyper::http::Error),
    #[error("the session answered {0}")]
    Status(u16),
    #[error("could not read what the session said: {0}")]
    Body(#[from] serde_json::Error),
}
