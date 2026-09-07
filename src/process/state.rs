#[derive(Clone, Copy, Debug)]
pub enum ProcessState {
    Stopped,
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
