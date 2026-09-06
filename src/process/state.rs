#[derive(Clone, Copy)]
pub enum ProcessState {
    Started,
    Running,
    ExitSuccess,
    ExitError,
    Killing,
    Killed,
}

impl ProcessState {
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            ProcessState::ExitSuccess | ProcessState::ExitError | ProcessState::Killed
        )
    }
}
