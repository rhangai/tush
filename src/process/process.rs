use anyhow::anyhow;
use tokio::process::Command;

use crate::{
    base::LogWriterRef,
    process::{child::ProcessChild, handle::ProcessHandle, state::ProcessState},
};

pub struct Process {
    command: Vec<String>,
    handle: Option<ProcessHandle>,
}

impl Process {
    pub fn new(command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let command = command.into_iter().map(Into::into).collect();
        Self {
            command,
            handle: None,
        }
    }

    pub fn start(&mut self) {
        if let Some(handle) = &self.handle {
            if handle.state().is_finished() {
                self.start_inner(None);
            }
            return;
        }
        self.start_inner(None);
    }

    pub fn restart(&mut self) {
        self.start_inner(None);
    }

    pub fn state(&self) -> ProcessState {
        self.handle
            .as_ref()
            .map(|h| h.state())
            .unwrap_or(ProcessState::Stopped)
    }

    pub async fn wait(&mut self) {
        if let Some(handle) = &mut self.handle {
            handle.wait().await;
        }
    }

    pub fn stop(&mut self) {
        if let Some(handle) = &mut self.handle {
            handle.abort();
        }
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        self.start_inner(None)?;
        self.wait().await;
        Ok(())
    }

    fn start_inner(&mut self, writer: Option<LogWriterRef>) -> anyhow::Result<()> {
        let Some((command, args)) = self.command.split_first() else {
            return Err(anyhow!("No commands"));
        };
        let mut command = Command::new(command);
        command.args(args);
        let child = ProcessChild::new(command, writer);
        let handle = ProcessHandle::new(child);
        self.handle = Some(handle);
        Ok(())
    }
}
