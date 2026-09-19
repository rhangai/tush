use std::sync::Arc;

use arc_swap::ArcSwapOption;
use parking_lot::Mutex;

use crate::error::UnitError;
use crate::unit::behavior::UnitBehaviorContext;
use crate::util::event::EventDispatcher;
use crate::util::str::SmallStr;
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
    /// The output of every run, kept across all of them.
    log: Log,
    /// For writing into the log on the unit's own behalf, not a process's.
    log_notes: LogWriterNotes,
    /// What it runs, behind a lock because a dispatch may change it.
    behavior: Mutex<UnitBehavior>,
    /// The current run, or `None` before the first.
    handle: ArcSwapOption<RunnerHandle>,
    /// Event dispatcher
    event_dispatcher: Option<EventDispatcher>,
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
            event_dispatcher: None,
        }
    }

    /// Set the event dispatcher
    pub fn set_event_dispatcher(&mut self, event_dispatcher: EventDispatcher) {
        self.event_dispatcher = Some(event_dispatcher);
    }

    /// Dump the log to stdout, for working on the log itself.
    pub fn debug(&self) {
        self.log.debug();
    }

    /// What it is called on screen.
    ///
    /// Owned because the behavior is behind a lock and nothing may borrow out
    /// of it — cheaply, a name being short enough to sit inside the
    /// [`SmallStr`] and so to be copied rather than allocated.
    pub fn name(&self) -> SmallStr {
        self.behavior.lock().name()
    }

    /// The shorter name for it, or `None` where the config declared none.
    pub fn name_short(&self) -> Option<SmallStr> {
        self.behavior.lock().name_short()
    }

    /// Which of its modes is current, or `None` if it has none. Read every
    /// time a view refreshes, since a dispatch may have moved it.
    pub fn mode(&self) -> Option<SmallStr> {
        self.behavior.lock().mode()
    }

    /// That mode's short name, read every sync for the same reason.
    pub fn mode_short(&self) -> Option<SmallStr> {
        self.behavior.lock().mode_short()
    }

    /// Hand an event to the behavior, and take the action it asks for.
    pub fn dispatch(&self, event: UnitEvent) -> Option<UnitAction> {
        self.behavior.lock().dispatch(event, self.state())
    }

    /// Everything that can be asked of this unit right now.
    ///
    /// The state is read here rather than passed in, so the list and the
    /// state it was built from are the same moment.
    pub fn choices(&self, out: &mut Vec<UnitChoice>) {
        let state = self.state();
        self.behavior.lock().choices(state, out);
    }

    /// Run it, in whatever mode its behavior is on.
    pub fn start(&self) -> Result<Arc<RunnerHandle>, UnitError> {
        let mut ctx = UnitBehaviorContext::new().with_writer(self.log.writer());
        if let Some(event_dispatcher) = self.event_dispatcher.as_ref() {
            ctx = ctx.with_event_dispatcher(event_dispatcher.clone());
        }
        let handle = self.behavior.lock().spawn(ctx)?;
        self.set_handle(handle)
    }

    /// The handle for the current run, if there is one.
    pub fn clone_handle(&self) -> Option<Arc<RunnerHandle>> {
        self.handle.load_full()
    }

    /// The restart handshake.
    ///
    /// The new handle is published immediately, so callers see the incoming
    /// run at once, but it is created paused and only released once the
    /// outgoing one is really gone:
    ///
    /// - nothing was running, or the previous run already finished: start now;
    /// - a run is still alive: abort it and start the new one from a detached
    ///   task, after the old one has been waited on.
    ///
    /// Serializing it this way keeps two runs of the same unit from ever
    /// overlapping — no two dev servers fighting over the same port.
    fn set_handle(&self, handle: RunnerHandle) -> Result<Arc<RunnerHandle>, UnitError> {
        let handle = Arc::new(handle);
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

    /// A new reader over this unit's log.
    pub fn log_reader(&self) -> LogReader {
        self.log.reader()
    }
}

/// Shut the current run down before the unit's log goes with it.
///
/// Without this a dropped unit released its [`Log`] while its process was
/// still writing, and the process died only by accident: the reader task gave
/// up, the pipe closed, and the next write earned a `SIGPIPE`. The exit then
/// came back as a plain error, indistinguishable from the process failing on
/// its own — and one that ignores `SIGPIPE` got an `EPIPE` to make its own
/// mind up about.
///
/// Aborting here routes the teardown through the path that already exists:
/// `SIGTERM`, a grace period, then `SIGKILL`, reported as
/// [`Killed`](RunnerState::Killed).
///
/// It starts the shutdown and cannot wait for it — `drop` is not async — so
/// the log is released while the process may still be on its way out, and the
/// `SIGPIPE` path stays as a backstop. For a teardown you can observe, call
/// [`stop`](Unit::stop) and wait on the handle before dropping the unit.
impl Drop for Unit {
    fn drop(&mut self) {
        self.stop();
    }
}
