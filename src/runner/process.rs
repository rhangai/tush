use anyhow::anyhow;
use tokio::process::Command;

use crate::{
    base::{ExitReason, LogWriterRef, Process},
    runner::base::{Runner, RunnerDescription},
};

pub struct RunnerProcessDescription {
    command: Vec<String>,
}

impl RunnerProcessDescription {
    pub fn new(command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let command = command.into_iter().map(Into::into).collect();
        Self { command }
    }
}

impl RunnerDescription for RunnerProcessDescription {
    fn exec(&self, writer: Option<LogWriterRef>) -> anyhow::Result<impl Runner> {
        let Some((command, args)) = self.command.split_first() else {
            return Err(anyhow!("No commands"));
        };
        let mut command = Command::new(command);
        command.args(args);
        let process = Process::new(command, writer);
        Ok(RunnerProcess { process })
    }
}

struct RunnerProcess {
    process: Process,
}

impl Runner for RunnerProcess {
    async fn run(&mut self) -> anyhow::Result<ExitReason> {
        self.process.start().await?;
        self.process.wait().await
    }

    async fn shutdown(&mut self) -> anyhow::Result<ExitReason> {
        self.process.shutdown().await
    }
}
