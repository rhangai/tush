use crate::unit::{base::UnitRunner, state::UnitState};

pub struct UnitHandle {
    abort_sender: Option<tokio::sync::oneshot::Sender<()>>,
    state_sender: tokio::sync::watch::Sender<UnitState>,
    state_receiver: tokio::sync::watch::Receiver<UnitState>,
}

impl UnitHandle {
    pub fn new(runner: impl UnitRunner) -> Self {
        let (abort_sender, abort_receiver) = tokio::sync::oneshot::channel::<()>();
        let (state_sender, state_receiver) = tokio::sync::watch::channel(UnitState::Started);

        let handle = UnitHandle {
            abort_sender: Some(abort_sender),
            state_sender: state_sender.clone(),
            state_receiver,
        };
        tokio::spawn(async move {
            let mut runner = runner;
            _ = state_sender.send(UnitState::Running);
            let runner_fut = runner.run();
            tokio::select! {
                wait_result = runner_fut => {
                    _ = state_sender.send(wait_result.map_or(UnitState::ExitError(None), |i| i.into()));
                }
                _ = abort_receiver => {
                    _ = state_sender.send(UnitState::Killing);
                    let shutdown_state = runner.shutdown().await;
                    _ = state_sender.send(shutdown_state.map_or(UnitState::Killed(None), |i| i.into()));
                }
            }
        });
        handle
    }

    pub fn state(&self) -> UnitState {
        *self.state_receiver.borrow()
    }

    pub fn abort(&mut self) {
        if let Some(abort_sender) = self.abort_sender.take() {
            _ = abort_sender.send(());
            _ = self.state_sender.send(UnitState::Killing);
        }
    }
}
