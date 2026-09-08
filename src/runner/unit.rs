use parking_lot::Mutex;

use crate::{
    base::{ExitReason, LogWriterRef},
    runner::{Runner, RunnerDescription, RunnerState},
};

pub struct RunnerUnit<D: RunnerDescription> {
    description: D,
    handle: Mutex<Option<RunnerHandle>>,
}

impl<D: RunnerDescription> RunnerUnit<D> {
    pub fn new(description: D) -> Self {
        Self {
            description,
            handle: Mutex::new(None),
        }
    }

    /// Start the unit
    pub fn start(&self) -> anyhow::Result<()> {
        let mut handle = self.handle.lock();
        let is_stopped = handle.as_ref().is_none_or(|h| h.state().is_finished());
        if is_stopped {
            *handle = Some(self.spawn(None)?);
        }
        Ok(())
    }

    pub fn stop(&self) -> anyhow::Result<()> {
        let mut handle = self.handle.lock();
        if let Some(handle) = handle.as_mut() {
            handle.abort();
        }
        Ok(())
    }

    pub fn restart(&self) -> anyhow::Result<()> {
        *self.handle.lock() = Some(self.spawn(None)?);
        Ok(())
    }

    /// Spawn a new handle for this unit
    pub fn spawn(&self, writer: Option<LogWriterRef>) -> anyhow::Result<RunnerHandle> {
        let handle = RunnerHandle::new(self.description.exec(writer)?);
        Ok(handle)
    }

    pub fn state(&self) -> RunnerState {
        self.handle
            .lock()
            .as_ref()
            .map(|h| h.state())
            .unwrap_or(RunnerState::Stopped)
    }
}

/// A handle for the runner
pub struct RunnerHandle {
    abort_sender: Option<tokio::sync::oneshot::Sender<()>>,
    state_sender: tokio::sync::watch::Sender<RunnerState>,
    state_receiver: tokio::sync::watch::Receiver<RunnerState>,
}

impl RunnerHandle {
    /// Create the handle from the runner
    fn new(runner: impl Runner) -> Self {
        let (abort_sender, abort_receiver) = tokio::sync::oneshot::channel::<()>();
        let (state_sender, state_receiver) = tokio::sync::watch::channel(RunnerState::Started);

        let handle = RunnerHandle {
            abort_sender: Some(abort_sender),
            state_sender: state_sender.clone(),
            state_receiver,
        };
        tokio::spawn(async move {
            let mut runner = runner;
            _ = state_sender.send(RunnerState::Running);
            let runner_fut = runner.run();
            tokio::select! {
                wait_result = runner_fut => {
                    _ = state_sender.send(wait_result.map_or(RunnerState::ExitError(None), |i| i.into()));
                }
                _ = abort_receiver => {
                    _ = state_sender.send(RunnerState::Killing);
                    let shutdown_state = runner.shutdown().await;
                    _ = state_sender.send(shutdown_state.map_or(RunnerState::Killed(None), |i| i.into()));
                }
            }
        });
        handle
    }

    /// State of the current runner
    pub fn state(&self) -> RunnerState {
        *self.state_receiver.borrow()
    }

    /// Wait for the runner to finish
    pub async fn wait(&mut self) -> ExitReason {
        let result = self.state_receiver.wait_for(|s| s.is_finished()).await;
        let Ok(status) = result else {
            return ExitReason::Error(None);
        };
        ExitReason::try_from(*status).unwrap_or(ExitReason::Error(None))
    }

    /// Abort the runner
    pub fn abort(&mut self) {
        if let Some(abort_sender) = self.abort_sender.take() {
            _ = self.state_sender.send(RunnerState::Killing);
            _ = abort_sender.send(());
        }
    }
}
