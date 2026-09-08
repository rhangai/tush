use crate::base::ProcessExit;

#[derive(Clone, Copy, Debug)]
pub enum UnitState {
    Stopped,
    Started,
    Running,
    ExitSuccess,
    ExitError(Option<i32>),
    Killing,
    Killed(Option<i32>),
}

impl UnitState {
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            UnitState::ExitSuccess | UnitState::ExitError(..) | UnitState::Killed(..)
        )
    }

    pub fn is_stopped(&self) -> bool {
        matches!(
            self,
            UnitState::Stopped
                | UnitState::ExitSuccess
                | UnitState::ExitError(..)
                | UnitState::Killed(..)
        )
    }
}

impl From<UnitExitReason> for UnitState {
    fn from(value: UnitExitReason) -> Self {
        match value {
            UnitExitReason::Success => UnitState::ExitSuccess,
            UnitExitReason::Error(code) => UnitState::ExitError(code),
            UnitExitReason::Killed(code) => UnitState::Killed(code),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum UnitExitReason {
    Success,
    Error(Option<i32>),
    Killed(Option<i32>),
}

impl From<ProcessExit> for UnitExitReason {
    fn from(value: ProcessExit) -> Self {
        match value {
            ProcessExit::Success => UnitExitReason::Success,
            ProcessExit::Error(code) => UnitExitReason::Error(code),
            ProcessExit::Killed(code) => UnitExitReason::Killed(code),
        }
    }
}
