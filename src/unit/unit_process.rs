use anyhow::anyhow;
use tokio::process::Command;

use crate::{
    base::Process,
    unit::{
        base::{UnitDescription, UnitRunner},
        state::UnitExitReason,
    },
};

pub struct UnitProcess {
    command: Vec<String>,
}

impl UnitProcess {
    pub fn new(command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let command = command.into_iter().map(Into::into).collect();
        Self { command }
    }
}

impl UnitDescription for UnitProcess {
    fn exec(&self, writer: Option<crate::base::LogWriterRef>) -> anyhow::Result<impl UnitRunner> {
        let Some((command, args)) = self.command.split_first() else {
            return Err(anyhow!("No commands"));
        };
        let mut command = Command::new(command);
        command.args(args);
        let process = Process::new(command, writer);
        Ok(UnitProcessRunner { process })
    }
}

struct UnitProcessRunner {
    process: Process,
}

impl UnitRunner for UnitProcessRunner {
    async fn run(&mut self) -> anyhow::Result<UnitExitReason> {
        self.process.start().await?;
        let exit = self.process.wait().await?;
        Ok(exit.into())
    }

    async fn shutdown(&mut self) -> anyhow::Result<UnitExitReason> {
        let exit = self.process.shutdown().await?;
        Ok(exit.into())
    }
}
