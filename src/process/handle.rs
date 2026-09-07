use tokio::task::JoinSet;

use crate::process::{child::ProcessChild, state::ProcessState};

pub struct ProcessHandle {
    kill_sender: Option<tokio::sync::oneshot::Sender<()>>,
    state_receiver: tokio::sync::watch::Receiver<ProcessState>,
}

impl ProcessHandle {
    pub fn new(child: ProcessChild) -> Self {
        let (handle, future) = Self::new_inner(child);
        tokio::spawn(future);
        handle
    }

    pub fn new_in_join_set(child: ProcessChild, join_set: &mut JoinSet<()>) -> Self {
        let (handle, future) = Self::new_inner(child);
        join_set.spawn(future);
        handle
    }

    fn new_inner(child: ProcessChild) -> (Self, impl Future<Output = ()>) {
        let (kill_sender, kill_receiver) = tokio::sync::oneshot::channel::<()>();
        let (state_sender, state_receiver) = tokio::sync::watch::channel(ProcessState::Started);

        let future = async move {
            let mut runner = child;
            runner.start().await;
            _ = state_sender.send(ProcessState::Running);

            tokio::select! {
                ok = runner.wait() => {
                    _ = state_sender.send(ProcessState::ExitSuccess);
                }
                result = kill_receiver => {
                    match result {
                        Ok(_) => {
                            _ = state_sender.send(ProcessState::Killing);
                            runner.kill().await;
                            _ = state_sender.send(ProcessState::Killed);
                        }
                        Err(_) => {
                            let ok = runner.wait().await;
                            _ = state_sender.send(ProcessState::ExitSuccess);
                        }
                    }
                }
            }
        };
        let handle = ProcessHandle {
            kill_sender: Some(kill_sender),
            state_receiver,
        };
        (handle, future)
    }

    pub fn state(&self) -> ProcessState {
        *self.state_receiver.borrow()
    }

    pub async fn wait(&mut self) {
        _ = self.state_receiver.wait_for(|s| s.is_finished()).await;
    }

    pub fn kill(&mut self) {
        if let Some(kill_sender) = self.kill_sender.take() {
            _ = kill_sender.send(());
        }
    }
}
