use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::runner::{
    Runner,
    state::{RunnerState, RunnerStateAtomic},
};

/// A handle for the runner
pub struct RunnerHandle {
    abort_token: CancellationToken,
    state: RunnerStateAtomic,
    exit_state_receiver: tokio::sync::watch::Receiver<Option<RunnerState>>,
}

impl RunnerHandle {
    /// Create the handle from the runner
    pub fn new(runner: impl Runner) -> Arc<Self> {
        let (exit_state_sender, exit_state_receiver) =
            tokio::sync::watch::channel::<Option<RunnerState>>(None);
        let handle = Arc::new(RunnerHandle {
            abort_token: CancellationToken::new(),
            state: RunnerStateAtomic::new(RunnerState::Started),
            exit_state_receiver,
        });

        // Block to spawn the worker task
        {
            let handle = handle.clone();
            tokio::spawn(async move {
                let mut runner = runner;
                handle.state.store_next(RunnerState::Running);
                let runner_fut = runner.run();
                let exit_state = tokio::select! {
                    wait_result = runner_fut => {
                        wait_result.map_or(RunnerState::ExitError(None), |i| i.into())
                    }
                    _ = handle.abort_token.cancelled() => {
                        handle.state.store(RunnerState::Killing);
                        let shutdown_state = runner.shutdown().await;
                        shutdown_state.map_or(RunnerState::Killed(None), |i| i.into())
                    }
                };
                handle.state.store(exit_state);
                _ = exit_state_sender.send(Some(exit_state));
            })
        };

        handle
    }

    /// State of the current runner
    pub fn state(&self) -> RunnerState {
        self.state.load()
    }

    /// Abort the runner
    pub fn abort(&self) {
        self.state.store_next(RunnerState::Killing);
        self.abort_token.cancel();
    }

    /// Wait for the runner
    pub async fn wait(&self) -> Option<RunnerState> {
        let mut receiver = self.exit_state_receiver.clone();
        receiver
            .wait_for(|s| s.is_some())
            .await
            .map_or(None, |v| v.clone())
    }
}
