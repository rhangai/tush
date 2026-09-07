use parking_lot::Mutex;
use tokio::task::JoinSet;

use crate::process::{child::ProcessChild, state::ProcessState};

pub struct ProcessHandle {
    abort_sender: Option<tokio::sync::oneshot::Sender<()>>,
    state_receiver: tokio::sync::watch::Receiver<ProcessState>,
}

impl ProcessHandle {
    pub fn new(child: ProcessChild) -> Self {
        let (handle, runner) = Self::new_runner(child);
        tokio::spawn(runner.run());
        handle
    }

    pub fn new_in_join_set(child: ProcessChild, join_set: &mut JoinSet<()>) -> Self {
        let (handle, runner) = Self::new_runner(child);
        join_set.spawn(runner.run());
        handle
    }

    fn new_runner(child: ProcessChild) -> (Self, ProcessRunner) {
        let (abort_sender, abort_receiver) = tokio::sync::oneshot::channel::<()>();
        let (state_sender, state_receiver) = tokio::sync::watch::channel(ProcessState::Started);
        let runner = ProcessRunner {
            child,
            state_sender,
            abort_receiver,
        };
        let handle = ProcessHandle {
            abort_sender: Some(abort_sender),
            state_receiver,
        };
        (handle, runner)
    }

    pub fn state(&self) -> ProcessState {
        *self.state_receiver.borrow()
    }

    pub async fn wait(&mut self) {
        _ = self.state_receiver.wait_for(|s| s.is_finished()).await;
    }

    pub fn abort(&mut self) {
        if let Some(abort_sender) = self.abort_sender.take() {
            _ = abort_sender.send(());
        }
    }
}

struct ProcessRunner {
    child: ProcessChild,
    abort_receiver: tokio::sync::oneshot::Receiver<()>,
    state_sender: tokio::sync::watch::Sender<ProcessState>,
}

impl ProcessRunner {
    async fn run(self) {
        let ProcessRunner {
            mut child,
            abort_receiver,
            state_sender,
        } = self;
        _ = child.start().await;
        _ = state_sender.send(ProcessState::Running);
        tokio::select! {
            wait_result = child.wait() => {
                _ = state_sender.send(wait_result.unwrap_or(ProcessState::ExitError(None)));
            }
            _ = abort_receiver => {
                _ = state_sender.send(ProcessState::Killing);
                let shutdown_state = child.shutdown().await;
                _ = state_sender.send(shutdown_state.unwrap_or(ProcessState::Killed(None)));
            }
        }
    }
}
