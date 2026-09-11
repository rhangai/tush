use std::sync::Arc;

use arc_swap::ArcSwapOption;

use crate::{
    log::{Log, LogReader},
    runner::{RunnerHandle, RunnerState},
    unit::description::UnitDescription,
};

/// A named, restartable entry: one log, one description, one current run.
///
/// The unit outlives its runs. Handles come and go — each
/// [`start`](Unit::start) creates a new one — but the [`Log`] belongs to the
/// unit, so restarting a process keeps its scrollback intact and readers do not
/// have to re-subscribe.
///
/// The current handle lives in an [`ArcSwapOption`] so that
/// [`state`](Unit::state) and [`stop`](Unit::stop) can be called from any task
/// without locking, including while a restart is in flight.
pub struct Unit {
    log: Log,
    description: UnitDescription,
    handle: ArcSwapOption<RunnerHandle>,
}

impl Unit {
    /// Create a stopped unit with an empty log.
    pub fn new(description: UnitDescription) -> Self {
        Self {
            log: Log::new(4096),
            description,
            handle: ArcSwapOption::const_empty(),
        }
    }

    /// Start running the process
    ///
    /// Uses the unit's own description.
    pub fn debug(&self) {
        self.log.debug();
    }

    /// Start running the process
    ///
    /// Uses the unit's own description.
    pub fn start(&self) -> anyhow::Result<Arc<RunnerHandle>> {
        self.start_with_description(&self.description)
    }

    /// Start running the process
    ///
    /// Runs `desc` instead of the unit's own — the mechanism behind switching
    /// a unit between modes. The unit's stored description is left untouched,
    /// so a later [`start`](Unit::start) goes back to it.
    pub fn start_with_description(
        &self,
        desc: impl AsRef<UnitDescription>,
    ) -> anyhow::Result<Arc<RunnerHandle>> {
        let handle = desc.as_ref().spawn(Some(self.log.writer()))?;
        self.set_handle(handle)
    }

    /// Set the handle internally
    ///
    /// The restart handshake. The new handle is published immediately — so
    /// callers see the incoming run right away — but it is created paused, and
    /// only released once the outgoing one is really gone:
    ///
    /// - nothing was running, or the previous run already finished: start now;
    /// - a run is still alive: abort it and start the new one from a detached
    ///   task, after the old one has been waited on.
    ///
    /// Serializing it this way keeps two runs of the same unit from ever
    /// overlapping — no two dev servers fighting over the same port.
    fn set_handle(&self, handle: Arc<RunnerHandle>) -> anyhow::Result<Arc<RunnerHandle>> {
        let old_handle = self.handle.swap(Some(handle.clone()));
        match old_handle {
            None => {
                handle.start();
            }
            Some(old_handle) if old_handle.state().is_finished() => {
                handle.start();
            }
            Some(old_handle) => {
                old_handle.abort();
                let handle = handle.clone();
                tokio::spawn(async move {
                    old_handle.wait().await;
                    handle.start();
                });
            }
        }
        Ok(handle)
    }

    /// Stop the current run.
    ///
    /// Aborts the current run, if any, and returns without waiting for it.
    /// The handle stays in place so its terminal state remains observable.
    pub fn stop(&self) {
        if let Some(handle) = self.handle.load().as_ref() {
            handle.abort();
        }
    }

    /// State of the current run.
    ///
    /// [`Stopped`](RunnerState::Stopped) when the unit was never started.
    pub fn state(&self) -> RunnerState {
        self.handle
            .load()
            .as_ref()
            .map_or(RunnerState::Stopped, |s| s.state())
    }

    /// Create a new log reader to be used
    pub fn log_reader(&self) -> LogReader {
        self.log.reader()
    }
}

/// Shut the current run down before the unit's log goes with it.
///
/// Without this a dropped unit released its [`Log`] while its process was
/// still writing, and the process died only as a side effect: the reader task
/// gave up, its end of the pipe closed, and the next write earned a
/// `SIGPIPE`. That reaped the child, but by accident — the exit came back as
/// a plain error, indistinguishable from the process having failed on its
/// own, and a process that ignores `SIGPIPE` got an `EPIPE` to make its own
/// mind up about.
///
/// Aborting here routes the same teardown through the path that already
/// exists, so it is a `SIGTERM` with a grace period before the `SIGKILL`, and
/// the run is reported as [`Killed`](RunnerState::Killed).
///
/// # What this does not do
///
/// It starts the shutdown; it cannot wait for it. `drop` is not async, and
/// the supervising task owns the runner and outlives the unit, so the log is
/// released while the process may still be on its way out — the `SIGPIPE`
/// path above stays as a backstop, and so does the `SIGKILL` in
/// [`Process`](crate::base::Process)'s own `Drop` if the runtime goes away
/// before the task can run. For a teardown you can observe, call
/// [`stop`](Unit::stop) and then wait on the handle before dropping the unit.
impl Drop for Unit {
    fn drop(&mut self) {
        self.stop();
    }
}
