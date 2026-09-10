use std::sync::Arc;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::runner::{
    Runner,
    state::{RunnerState, RunnerStateAtomic},
};

/// A handle for the runner
pub struct RunnerHandle {
    start_notify: Option<Notify>,
    abort_token: CancellationToken,
    state: RunnerStateAtomic,
    exit_state_receiver: tokio::sync::watch::Receiver<Option<RunnerState>>,
}

impl RunnerHandle {
    /// Create the handle from the runner
    pub fn new(runner: impl Runner) -> Arc<Self> {
        Self::new_inner(runner, true)
    }

    /// Create the handle from the runner, already running
    pub fn new_running(runner: impl Runner) -> Arc<Self> {
        Self::new_inner(runner, false)
    }

    /// Create the handle from the runner
    fn new_inner(runner: impl Runner, paused: bool) -> Arc<Self> {
        let (exit_state_sender, exit_state_receiver) =
            tokio::sync::watch::channel::<Option<RunnerState>>(None);
        let handle = Arc::new(RunnerHandle {
            start_notify: if paused { Some(Notify::new()) } else { None },
            abort_token: CancellationToken::new(),
            state: RunnerStateAtomic::new(if paused {
                RunnerState::Waiting
            } else {
                RunnerState::Started
            }),
            exit_state_receiver,
        });

        // Block to spawn the worker task
        {
            let handle = handle.clone();
            tokio::spawn(async move {
                if let Some(start_notify) = &handle.start_notify {
                    start_notify.notified().await;
                }
                let mut runner = runner;
                // Eager check for cancelation
                if handle.abort_token.is_cancelled() {
                    let exit_state = RunnerState::Killed(None);
                    handle.state.store(exit_state);
                    _ = exit_state_sender.send(Some(exit_state));
                    return;
                }
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

    /// Start the handle
    pub fn start(&self) {
        self.state.store_next(RunnerState::Started);
        self.notify_start();
    }

    /// Abort the runner
    pub fn abort(&self) {
        self.state.store_next(RunnerState::Killing);
        self.abort_token.cancel();
        self.notify_start();
    }

    /// Wait for the runner
    pub async fn wait(&self) -> Option<RunnerState> {
        self.notify_start();
        let mut receiver = self.exit_state_receiver.clone();
        receiver
            .wait_for(|s| s.is_some())
            .await
            .map_or(None, |v| v.clone())
    }

    /// Notify the start handle
    fn notify_start(&self) {
        if let Some(start_notify) = &self.start_notify {
            start_notify.notify_one();
        }
    }
}
