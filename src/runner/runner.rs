use crate::base::{ExitReason, Process};

/// Something a [`RunnerHandle`](crate::runner::RunnerHandle) can drive to
/// completion. Moved into a Tokio task, hence `Send + 'static`.
///
/// The two methods are used together and only one wins: `run` is polled in a
/// `select!` against the abort signal, and if the abort fires first, `run` is
/// dropped and `shutdown` is called on the same value. So dropping `run`
/// mid-flight must leave something `shutdown` can still clean up.
pub trait Runner: Send + 'static {
    /// Run to completion, reporting how it ended.
    fn run(
        &mut self,
        on_run: impl FnOnce() + Send + 'static,
    ) -> impl Future<Output = anyhow::Result<ExitReason>> + Send;
    /// Stop a run that is still in flight, as gracefully as possible.
    fn shutdown(&mut self) -> impl Future<Output = anyhow::Result<ExitReason>> + Send;
}

/// `run` spawns the child and waits; `shutdown` is the `SIGTERM` then
/// `SIGKILL` sequence from [`Process::shutdown`].
impl Runner for Process {
    async fn run(&mut self, on_run: impl FnOnce() + Send + 'static) -> anyhow::Result<ExitReason> {
        self.start().await?;
        on_run();
        self.wait().await
    }

    async fn shutdown(&mut self) -> anyhow::Result<ExitReason> {
        Process::shutdown(self).await
    }
}

/// The no-op runner: succeeds immediately. A placeholder for behaviors with
/// nothing to execute, and the simplest thing to test the handle against.
impl Runner for () {
    async fn run(&mut self, on_run: impl FnOnce() + Send + 'static) -> anyhow::Result<ExitReason> {
        on_run();
        Ok(ExitReason::Success)
    }

    async fn shutdown(&mut self) -> anyhow::Result<ExitReason> {
        Ok(ExitReason::Success)
    }
}
