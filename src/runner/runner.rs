use crate::base::{ExitReason, Process};

/// A runner
///
/// Something that can be driven to completion by a
/// [`RunnerHandle`](crate::runner::RunnerHandle). Implementors are moved into
/// a Tokio task, hence the `Send + 'static` bound.
///
/// The two methods are always used together, and only one of them wins:
/// `run` is polled inside a `select!` against the abort signal, and if the
/// abort fires first, `run` is dropped and `shutdown` is called on the same
/// value. So `run` must be cancel safe in the weak sense that dropping it
/// mid-flight has to leave the runner in a state `shutdown` can still clean up.
pub trait Runner: Send + 'static {
    /// Run to completion, reporting how it ended.
    fn run(&mut self) -> impl Future<Output = anyhow::Result<ExitReason>> + Send;
    /// Stop a run that is still in flight, as gracefully as possible.
    fn shutdown(&mut self) -> impl Future<Output = anyhow::Result<ExitReason>> + Send;
}

/// A process is a runner
///
/// `run` spawns the child and waits for it; `shutdown` is the `SIGTERM`,
/// then `SIGKILL` sequence from [`Process::shutdown`].
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
///
/// The unit type is the no-op runner: it succeeds immediately. Useful as a
/// placeholder for descriptions that have nothing to execute, and as the
/// simplest thing to test the handle's state machine against.
impl Runner for () {
    async fn run(&mut self) -> anyhow::Result<ExitReason> {
        Ok(ExitReason::Success)
    }

    async fn shutdown(&mut self) -> anyhow::Result<ExitReason> {
        Ok(ExitReason::Success)
    }
}
