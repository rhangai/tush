use anyhow::anyhow;
use tokio::{process::Command, task::JoinSet};

use crate::{
    base::Log,
    process::{child::ProcessChild, handle::ProcessHandle, state::ProcessState},
};

pub struct Process {
    command: Vec<String>,
    log: Log,
    handle: Option<ProcessHandle>,
}

impl Process {
    pub fn new(capacity: usize, command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let log = Log::new(capacity);
        let command = command.into_iter().map(Into::into).collect();
        Self {
            command,
            log,
            handle: None,
        }
    }

    pub fn log(&self) -> &Log {
        &self.log
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
        if let Some(mut handle) = self.handle.take() {
            handle.kill();
        }
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        let mut join_set = JoinSet::new();
        self.start_inner(Some(&mut join_set))?;
        join_set.join_all().await;
        Ok(())
    }

    fn start_inner(&mut self, join_set: Option<&mut JoinSet<()>>) -> anyhow::Result<()> {
        let writer = self.log.writer();
        let Some((command, args)) = self.command.split_first() else {
            return Err(anyhow!("No commands"));
        };
        let mut command = Command::new(command);
        command.args(args);
        let child = ProcessChild::new(command, writer);
        let handle = if let Some(join_set) = join_set {
            ProcessHandle::new_in_join_set(child, join_set)
        } else {
            ProcessHandle::new(child)
        };
        self.handle = Some(handle);
        Ok(())
    }
}
