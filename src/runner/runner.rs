use crate::{
    base::{ExitReason, Process},
    error::RunnerError,
};

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
    ) -> impl Future<Output = Result<ExitReason, RunnerError>> + Send;
    /// Stop a run that is still in flight, as gracefully as possible.
    fn shutdown(&mut self) -> impl Future<Output = Result<ExitReason, RunnerError>> + Send;
}

/// `run` spawns the child and waits; `shutdown` is the `SIGTERM` then
/// `SIGKILL` sequence from [`Process::shutdown`].
impl Runner for Process {
    async fn run(
        &mut self,
        on_run: impl FnOnce() + Send + 'static,
    ) -> Result<ExitReason, RunnerError> {
        self.start().await?;
        on_run();
        let reason = self.wait().await?;
        Ok(reason)
    }

    async fn shutdown(&mut self) -> Result<ExitReason, RunnerError> {
        let reason = Process::shutdown(self).await?;
        Ok(reason)
    }
}

/// The no-op runner: succeeds immediately. A placeholder for behaviors with
/// nothing to execute, and the simplest thing to test the handle against.
impl Runner for () {
    async fn run(
        &mut self,
        on_run: impl FnOnce() + Send + 'static,
    ) -> Result<ExitReason, RunnerError> {
        on_run();
        Ok(ExitReason::Success)
    }

    async fn shutdown(&mut self) -> Result<ExitReason, RunnerError> {
        Ok(ExitReason::Success)
    }
}
