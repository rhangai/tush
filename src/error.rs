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
    #[error("{0}")]
    Process(#[from] ProcessError),
}

/// What starting a unit can fail with: a command with nothing in it, or
/// the run underneath.
#[derive(Debug, thiserror::Error)]
pub enum UnitError {
    #[error("a command with no program in it")]
    Invalid,
    #[error("unit already started")]
    AlreadyStarted,
    #[error("{0}")]
    Runner(RunnerError),
}

/// What takes the screen down: the terminal, never the session behind it.
#[derive(Debug, thiserror::Error)]
pub enum UiError {
    #[error("could not draw to the terminal: {0}")]
    DrawError(std::io::Error),
    #[error("could not read from the terminal: {0}")]
    EventError(std::io::Error),
    #[error("the screen stopped for a reason it did not name")]
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
    #[error("`{name}` contains reserved `{character}`")]
    ReservedCharacter { name: SmallStr, character: char },
    /// A proc depends on a name that no proc is declared under.
    #[error("`{proc}` depends on `{depends}`, which is not a proc")]
    UnknownDependency { proc: SmallStr, depends: SmallStr },
    /// Procs that wait on each other in a circle, so none of them can be
    /// first. A cycle of one is a proc that depends on itself.
    #[error("{} depend on each other in a circle", helper::Quoted(.0))]
    Cycle(Vec<SmallStr>),
    /// Every problem one pass over the config found, because a config with
    /// three mistakes in it is about to be fixed and one mistake per run is
    /// three runs of the same discovery.
    #[error("{}", helper::Lines(.0, "there were problems with the configuration:"))]
    Errors(Vec<AppConfigError>),
}

/// What addressing a unit in a session that is already built can fail with.
///
/// Apart from [`AppConfigError`], which is everything the checking found: by
/// the time there is a session to address, those questions are answered.
#[derive(thiserror::Error, Debug)]
pub enum AppError {
    /// A key no unit was declared under — a mistake in a config or a command
    /// and not a state a unit can be in.
    #[error("unit not found")]
    NotFound,
    /// The unit was found and would not start.
    #[error("could not start the unit: {0}")]
    UnitStart(UnitError),
    /// The unit was found and would not start.
    #[error("error with the unit: {0}")]
    UnitError(#[from] UnitError),
}

/// What stops a server from listening.
///
/// Never a client: one connection going wrong is that connection's problem,
/// and a session with no screen carries on without it.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
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

/// What stops one `tush dispatch` from being delivered.
///
/// Mostly not the socket. A key or a mode that does not exist is a typo, and
/// what answers a typo is the list the person could have typed instead — which
/// is why the modes are carried here rather than left as a status code.
#[derive(Debug, thiserror::Error)]
pub enum ViewDispatchError {
    /// Transparent, and not `"{0}"`: `#[from]` makes the inner error the
    /// source as well, and `anyhow` prints the chain — so a wrapper with a
    /// message of its own says the same sentence twice.
    #[error(transparent)]
    Socket(#[from] ViewSocketError),
    #[error("no proc is declared as `{0}`")]
    UnknownUnit(String),
    #[error("`{0}` runs one way and has no mode to pick")]
    NoModes(String),
    #[error("`{unit}` has no mode `{mode}`; it has {}", helper::Quoted(.modes))]
    UnknownMode {
        unit: String,
        mode: String,
        modes: Vec<SmallStr>,
    },
}

/// What an `#[error]` needs and `std::fmt` does not give it.
///
/// A module so the names can be what they are — `helper::Quoted` reads at the
/// attribute that uses it, where a `HelperQuoted` only said twice where it
/// came from.
mod helper {
    /// A list, each item in backticks: ``a`, `b``.
    pub struct Quoted<'a, T>(pub &'a T);

    impl<'a, T> std::fmt::Display for Quoted<'a, T>
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

    /// A heading, then one indented line per item.
    pub struct Lines<'a, T>(pub &'a T, pub &'a str);

    impl<'a, T> std::fmt::Display for Lines<'a, T>
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
}
