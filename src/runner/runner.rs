use crate::base::{ExitReason, Process};

/// A runner
pub trait Runner: Send + 'static {
    fn run(&mut self) -> impl Future<Output = anyhow::Result<ExitReason>> + Send;
    fn shutdown(&mut self) -> impl Future<Output = anyhow::Result<ExitReason>> + Send;
}

/// A process is a runner
impl Runner for Process {
    async fn run(&mut self) -> anyhow::Result<ExitReason> {
        self.start().await?;
        self.wait().await
    }

    async fn shutdown(&mut self) -> anyhow::Result<ExitReason> {
        Process::shutdown(self).await
    }
}

/// A process is a runner
impl Runner for () {
    async fn run(&mut self) -> anyhow::Result<ExitReason> {
        Ok(ExitReason::Success)
    }

    async fn shutdown(&mut self) -> anyhow::Result<ExitReason> {
        Ok(ExitReason::Success)
    }
}
