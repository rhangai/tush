#[derive(Clone, Copy, Debug)]
pub enum ProcessState {
    Stopped,
    Started,
    Running,
    ExitSuccess,
    ExitError(Option<i32>),
    Killing,
    Killed(Option<i32>),
}

impl ProcessState {
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            ProcessState::ExitSuccess | ProcessState::ExitError(..) | ProcessState::Killed(..)
        )
    }
}
