use crate::base::{ExitReason, LogWriterRef};

///
pub trait Runner: Send + 'static {
    fn run(&mut self) -> impl Future<Output = anyhow::Result<ExitReason>> + Send;
    fn shutdown(&mut self) -> impl Future<Output = anyhow::Result<ExitReason>> + Send;
}

/// Description
pub trait RunnerDescription {
    fn exec(&self, writer: Option<LogWriterRef>) -> anyhow::Result<impl Runner>;
}

#[derive(Clone, Copy, Debug)]
pub enum RunnerState {
    Stopped,
    Started,
    Running,
    ExitSuccess,
    ExitError(Option<i32>),
    Killing,
    Killed(Option<i32>),
}

impl RunnerState {
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            RunnerState::ExitSuccess | RunnerState::ExitError(..) | RunnerState::Killed(..)
        )
    }

    pub fn is_stopped(&self) -> bool {
        matches!(
            self,
            RunnerState::Stopped
                | RunnerState::ExitSuccess
                | RunnerState::ExitError(..)
                | RunnerState::Killed(..)
        )
    }
}

impl From<ExitReason> for RunnerState {
    fn from(value: ExitReason) -> Self {
        match value {
            ExitReason::Success => RunnerState::ExitSuccess,
            ExitReason::Error(code) => RunnerState::ExitError(code),
            ExitReason::Killed(code) => RunnerState::Killed(code),
        }
    }
}

impl TryFrom<RunnerState> for ExitReason {
    type Error = RunnerState;
    fn try_from(value: RunnerState) -> Result<Self, RunnerState> {
        match value {
            RunnerState::ExitSuccess => Ok(ExitReason::Success),
            RunnerState::ExitError(code) => Ok(ExitReason::Error(code)),
            RunnerState::Killed(code) => Ok(ExitReason::Killed(code)),
            reason => Err(reason),
        }
    }
}
