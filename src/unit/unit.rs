use std::sync::Arc;

use arc_swap::ArcSwapOption;
use arcstr::ArcStr;
use parking_lot::Mutex;

use crate::{
    log::{Log, LogReader, LogWriterNotes},
    runner::{RunnerHandle, RunnerState},
    unit::{UnitAction, UnitChoice, UnitEvent, behavior::UnitBehavior},
};

/// A named, restartable entry: one log, one behavior, one current run.
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
    log_notes: LogWriterNotes,
    behavior: Mutex<UnitBehavior>,
    handle: ArcSwapOption<RunnerHandle>,
}

impl Unit {
    /// Create a stopped unit with an empty log.
    pub fn new(behavior: UnitBehavior) -> Self {
        let log = Log::new(4096);
        let log_notes = log.notes();
        Self {
            log,
            log_notes,
            behavior: Mutex::new(behavior),
            handle: ArcSwapOption::const_empty(),
        }
    }

    /// Start running the process
    ///
    /// Uses the unit's own behavior.
    pub fn debug(&self) {
        self.log.debug();
    }

    /// What it is called on screen.
    ///
    /// Owned, because the behavior is behind a lock and nothing may borrow
    /// out of it — but owned cheaply: an [`ArcStr`] hands over a refcount
    /// rather than a copy, which is what lets these two be read as often as a
    /// view likes.
    pub fn name(&self) -> ArcStr {
        self.behavior.lock().name()
    }

    /// Which of its modes is current, or `None` if it has none.
    ///
    /// Read every time a view refreshes: this is the one that changes, each
    /// time a [`dispatch`](Unit::dispatch) moves the behavior on to the next
    /// mode.
    pub fn mode(&self) -> Option<ArcStr> {
        self.behavior.lock().mode()
    }

    /// Hand an event to the behavior, and take the action it asks for.
    pub fn dispatch(&self, event: UnitEvent) -> Option<UnitAction> {
        self.behavior.lock().dispatch(event, self.state())
    }

    /// Everything that can be asked of this unit right now, written into
    /// `out`.
    ///
    /// The state is read here rather than taken, so that the list and the
    /// state it was built from are the same moment — a menu that offered a
    /// `Restart` because the caller looked a frame ago is a menu that lies
    /// about the cheapest thing it could have checked.
    pub fn choices(&self, out: &mut Vec<UnitChoice>) {
        let state = self.state();
        self.behavior.lock().choices(state, out);
    }

    /// Start running the process
    ///
    /// Uses the unit's own behavior.
    pub fn start(&self) -> anyhow::Result<Arc<RunnerHandle>> {
        let handle = self.behavior.lock().spawn(Some(self.log.writer()))?;
        self.set_handle(handle)
    }

    /// Clone the handle
    pub fn clone_handle(&self) -> Option<Arc<RunnerHandle>> {
        self.handle.load_full()
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
