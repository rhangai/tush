use std::num::NonZeroU8;

use crate::base::ExitReason;

/// Where a run currently is in its lifecycle.
///
/// The variants are ordered: each one is "later" than the one above it, and
/// [`RunnerStateAtomic::store_next`] relies on that to make progress
/// monotonic. Everything from [`ExitSuccess`](RunnerState::ExitSuccess) down
/// is terminal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunnerState {
    /// Nothing was ever started (the state a unit reports with no handle).
    Stopped,
    /// The handle exists but is parked at the start gate.
    Waiting,
    /// Released to run; the task may not have been scheduled yet.
    Started,
    /// The runner's `run` future is in flight.
    Running,
    /// Aborted, waiting for the shutdown to complete.
    Killing,
    /// Finished on its own, successfully.
    ExitSuccess,
    /// Finished on its own, with a failure code.
    ExitError(Option<NonZeroU8>),
    /// Terminated by us.
    Killed(Option<NonZeroU8>),
}

impl RunnerState {
    /// Whether the run reached a terminal state.
    pub fn is_started(&self) -> bool {
        matches!(
            self,
            RunnerState::Started
                | RunnerState::Running
                | RunnerState::Killing
                | RunnerState::ExitSuccess
                | RunnerState::ExitError(..)
                | RunnerState::Killed(..)
        )
    }

    /// Whether the run reached a terminal state.
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            RunnerState::ExitSuccess | RunnerState::ExitError(..) | RunnerState::Killed(..)
        )
    }

    /// Whether nothing is running — terminal, or never started at all.
    pub fn is_stopped(&self) -> bool {
        matches!(
            self,
            RunnerState::Stopped
                | RunnerState::ExitSuccess
                | RunnerState::ExitError(..)
                | RunnerState::Killed(..)
        )
    }

    /// Set started
    pub fn set_started(&mut self) {
        *self = match self {
            RunnerState::Stopped | RunnerState::Waiting => RunnerState::Started,
            _ => *self,
        };
    }

    /// Set running
    pub fn set_running(&mut self) {
        *self = match self {
            RunnerState::Stopped | RunnerState::Waiting | RunnerState::Started => {
                RunnerState::Running
            }
            _ => *self,
        };
    }

    /// Set killing
    pub fn set_killing(&mut self) {
        *self = match self {
            RunnerState::Stopped
            | RunnerState::Waiting
            | RunnerState::Started
            | RunnerState::Running => RunnerState::Killing,
            _ => *self,
        };
    }
}

impl From<ExitReason> for RunnerState {
    /// Lift a finished process into the matching terminal state.
    fn from(value: ExitReason) -> Self {
        match value {
            ExitReason::Success => RunnerState::ExitSuccess,
            ExitReason::Error(code) => RunnerState::ExitError(code),
            ExitReason::Killed(code) => RunnerState::Killed(code),
        }
    }
}

impl TryFrom<RunnerState> for ExitReason {
    /// The state itself, when it was not a terminal one.
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
