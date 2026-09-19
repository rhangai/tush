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
    Process(ProcessError),
}

impl From<ProcessError> for RunnerError {
    fn from(value: ProcessError) -> Self {
        RunnerError::Process(value)
    }
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
