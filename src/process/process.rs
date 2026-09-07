use anyhow::anyhow;
use parking_lot::Mutex;
use tokio::process::Command;

use crate::{
    base::LogWriterRef,
    process::{child::ProcessChild, state::ProcessState},
};

pub struct Process {
    command: Vec<String>,
    handle: Mutex<Option<ProcessHandle>>,
}

impl Process {
    pub fn new(command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let command = command.into_iter().map(Into::into).collect();
        Self {
            command,
            handle: Mutex::new(None),
        }
    }

    pub fn start(&self) -> anyhow::Result<()> {
        let mut handle = self.handle.lock();
        let is_stopped = handle.as_ref().is_none_or(|h| h.state().is_finished());
        if is_stopped {
            *handle = Some(self.create_handle(None)?);
        }
        Ok(())
    }

    pub fn restart(&self) -> anyhow::Result<()> {
        *self.handle.lock() = Some(self.create_handle(None)?);
        Ok(())
    }

    pub async fn wait(&self) -> anyhow::Result<()> {
        let mut receiver = {
            let mut handle = self.handle.lock();
            let Some(handle) = handle.as_mut() else {
                return Ok(());
            };
            handle.state_receiver.clone()
        };
        _ = receiver.wait_for(|s| s.is_finished()).await;
        Ok(())
    }

    pub fn state(&self) -> ProcessState {
        self.handle
            .lock()
            .as_ref()
            .map(|h| h.state())
            .unwrap_or(ProcessState::Stopped)
    }

    pub fn stop(&self) -> anyhow::Result<()> {
        let mut handle = self.handle.lock();
        if let Some(handle) = handle.as_mut() {
            handle.abort();
        }
        Ok(())
    }

    fn create_handle(&self, writer: Option<LogWriterRef>) -> anyhow::Result<ProcessHandle> {
        let Some((command, args)) = self.command.split_first() else {
            return Err(anyhow!("No commands"));
        };
        let mut command = Command::new(command);
        command.args(args);
        let child = ProcessChild::new(command, writer);
        let handle = ProcessHandle::new(child);
        Ok(handle)
    }
}

struct ProcessHandle {
    abort_sender: Option<tokio::sync::oneshot::Sender<()>>,
    state_sender: tokio::sync::watch::Sender<ProcessState>,
    state_receiver: tokio::sync::watch::Receiver<ProcessState>,
}

impl ProcessHandle {
    fn new(child: ProcessChild) -> Self {
        let (abort_sender, abort_receiver) = tokio::sync::oneshot::channel::<()>();
        let (state_sender, state_receiver) = tokio::sync::watch::channel(ProcessState::Started);

        let handle = ProcessHandle {
            abort_sender: Some(abort_sender),
            state_sender: state_sender.clone(),
            state_receiver,
        };
        tokio::spawn(async move {
            let mut child = child;
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
        });
        handle
    }

    fn state(&self) -> ProcessState {
        *self.state_receiver.borrow()
    }

    fn abort(&mut self) {
        if let Some(abort_sender) = self.abort_sender.take() {
            _ = abort_sender.send(());
            _ = self.state_sender.send(ProcessState::Killing);
        }
    }
}
