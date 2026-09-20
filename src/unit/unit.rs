use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

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
/// The current handle shares one lock with the flag saying whether a run has
/// ever finished, because a restart changes both — see [`UnitCurrentHandle`].
/// Nothing is awaited while it is held, so [`state`](Unit::state) and
/// [`stop`](Unit::stop) can be called from any task, a restart in flight
/// included.
pub struct Unit {
    /// The output of every run, kept across all of them.
    log: Log,
    /// For writing into the log on the unit's own behalf, not a process's.
    log_notes: LogWriterNotes,
    /// What it runs, behind a lock because a dispatch may change it.
    behavior: Mutex<UnitBehavior>,
    /// Whom to wake when a run resolves. `None` for a unit built on its own
    /// rather than through a [`UnitMap`](crate::unit::UnitMap), which is the
    /// only thing that has a dispatcher to give.
    event_dispatcher: Option<EventDispatcher>,
    /// The current run and whether one has ever finished — see
    /// [`UnitCurrentHandle`]. Behind an [`Arc`] because the task that waits
    /// on a run holds a [`Weak`] to it and must not keep the unit alive.
    handle_manager: Arc<Mutex<UnitHandleManager>>,
}

impl Unit {
    /// Create a stopped unit with an empty log.
    pub fn new(behavior: UnitBehavior) -> Self {
        let log = Log::new(4096);
        let log_notes = log.notes();
        Self {
            log,
            log_notes,
            handle_manager: UnitHandleManager::new(),
            behavior: Mutex::new(behavior),
            event_dispatcher: None,
        }
    }

    /// While the unit is still being built: `&mut self`, and
    /// [`UnitMap::insert`](crate::unit::UnitMap::insert) is where it happens.
    pub fn set_event_dispatcher(&mut self, event_dispatcher: EventDispatcher) {
        self.event_dispatcher = Some(event_dispatcher.clone());
        self.handle_manager.lock().event_dispatcher = Some(event_dispatcher);
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

    /// Ensure the handle is cretead
    pub fn create(&self) -> Result<Arc<UnitHandle>, UnitError> {
        let mut manager = self.handle_manager.lock();
        manager.ensure_handle(|_| true, || self.spawn())
    }

    /// Ensure the handle is cretead
    pub fn ensure_created(&self) -> Result<Arc<UnitHandle>, UnitError> {
        let mut manager = self.handle_manager.lock();
        manager.ensure_handle(|_| true, || self.spawn())
    }

    /// Run it, in whatever mode its behavior is on.
    pub fn start(&self) -> Result<Arc<UnitHandle>, UnitError> {
        let handle = self.spawn()?;
        let handle = {
            let mut manager = self.handle_manager.lock();
            manager.set_handle(handle)
        };
        handle.start();
        Ok(handle)
    }

    /// Run it, in whatever mode its behavior is on.
    pub fn start_or_resume(&self) -> Result<Arc<UnitHandle>, UnitError> {
        let handle = {
            let mut manager = self.handle_manager.lock();
            manager.ensure_handle(|h| !h.is_started(), || self.spawn())?
        };
        handle.start();
        Ok(handle)
    }

    /// Start it only if it has never been started.
    ///
    /// [`AlreadyStarted`](UnitError::AlreadyStarted) is how "there was
    /// nothing to do" comes back, so a caller that only wanted it running
    /// drops the error rather than reporting it.
    pub fn ensure_started(&self) -> Result<Arc<UnitHandle>, UnitError> {
        let handle = {
            let mut manager = self.handle_manager.lock();
            manager.ensure_handle(|_| true, || self.spawn())?
        };
        handle.start();
        Ok(handle)
    }

    /// The handle for the current run, if there is one.
    pub fn clone_handle(&self) -> Option<Arc<UnitHandle>> {
        self.handle_manager.lock().unit_handle.clone()
    }

    /// Whether a run of this unit has ever reached the end.
    ///
    /// What anything depending on this unit waits for. It stays true once
    /// set: a later restart does not put the dependents back on hold.
    pub fn resolved(&self) -> bool {
        self.handle_manager.lock().resolved
    }

    /// A run of the current mode, wired to this unit's log and, if it has
    /// one, its dispatcher.
    fn spawn(&self) -> Result<RunnerHandle, UnitError> {
        let mut ctx = UnitBehaviorContext::new().with_writer(self.log.writer());
        if let Some(event_dispatcher) = self.event_dispatcher.as_ref() {
            ctx = ctx.with_event_dispatcher(event_dispatcher.clone());
        }
        self.behavior.lock().spawn(ctx)
    }

    /// Stop the current run.
    ///
    /// Aborts the current run, if any, and returns without waiting for it.
    /// The handle stays in place so its terminal state remains observable.
    pub fn stop(&self) {
        if let Some(handle) = self.handle_manager.lock().unit_handle.as_ref() {
            handle.abort();
        }
    }

    /// State of the current run.
    ///
    /// [`Stopped`](RunnerState::Stopped) when the unit was never started.
    pub fn state(&self) -> RunnerState {
        self.handle_manager
            .lock()
            .unit_handle
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

#[derive(Clone, Copy, PartialEq, Eq)]
struct UnitHandleId(NonZeroU32);

pub struct UnitHandle {
    manager_weak: Weak<Mutex<UnitHandleManager>>,
    handle_id: UnitHandleId,
    runner_handle: RunnerHandle,
    started: AtomicBool,
    parent_runner_handle: Option<Arc<UnitHandle>>,
}

impl UnitHandle {
    fn new(
        manager_weak: Weak<Mutex<UnitHandleManager>>,
        handle_id: UnitHandleId,
        runner_handle: RunnerHandle,
    ) -> Arc<Self> {
        Arc::new(Self {
            manager_weak,
            handle_id,
            runner_handle,
            started: AtomicBool::new(false),
            parent_runner_handle: None,
        })
    }

    fn child(
        self: Arc<Self>,
        manager_weak: Weak<Mutex<UnitHandleManager>>,
        handle_id: UnitHandleId,
        runner_handle: RunnerHandle,
    ) -> Arc<Self> {
        Arc::new(Self {
            manager_weak,
            handle_id,
            runner_handle,
            started: AtomicBool::new(false),
            parent_runner_handle: Some(self),
        })
    }

    pub fn state(&self) -> RunnerState {
        self.runner_handle.state()
    }

    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    pub fn start(self: &Arc<Self>) {
        // Start and set the bool as started
        if self
            .started
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let needs_resolve = self.needs_resolve();
        if let Some(parent) = self.parent_runner_handle.as_ref() {
            // Only wait if parent is pending
            if parent.state().is_pending() {
                let parent = parent.clone();
                let handle = self.clone();
                _ = tokio::spawn(async move {
                    parent.abort_and_wait().await;
                    handle.runner_handle.start();
                    if needs_resolve {
                        handle.resolve_task().await;
                    }
                });
                return;
            };
        };
        self.runner_handle.start();
        if needs_resolve {
            tokio::spawn(self.clone().resolve_task());
        }
    }

    pub fn abort(&self) {
        self.runner_handle.abort();
    }

    pub async fn abort_and_wait(&self) {
        self.runner_handle.abort();
        self.runner_handle.wait().await;
    }

    fn needs_resolve(&self) -> bool {
        let Some(manager) = self.manager_weak.upgrade() else {
            return false;
        };
        return !manager.lock().resolved;
    }

    async fn resolve_task(self: Arc<Self>) {
        if !self.runner_handle.wait_for(|s| s.is_finished()).await {
            return;
        }
        let Some(manager) = self.manager_weak.upgrade() else {
            return;
        };
        manager.lock().set_resolved(self.handle_id);
    }
}

struct UnitHandleManager {
    handle_id: UnitHandleId,
    resolved: bool,
    unit_handle: Option<Arc<UnitHandle>>,
    event_dispatcher: Option<EventDispatcher>,
    manager_weak: Weak<Mutex<Self>>,
}

impl UnitHandleManager {
    fn new() -> Arc<Mutex<Self>> {
        Arc::<Mutex<Self>>::new_cyclic(|manager_weak| {
            Mutex::new(Self {
                handle_id: UnitHandleId(NonZeroU32::new(1).expect("should never happen")),
                resolved: false,
                unit_handle: None,
                event_dispatcher: None,
                manager_weak: manager_weak.clone(),
            })
        })
    }

    fn set_handle(&mut self, runner_handle: RunnerHandle) -> Arc<UnitHandle> {
        self.handle_id.0 = if let Some(id) = self.handle_id.0.checked_add(1) {
            id
        } else {
            NonZeroU32::new(1).expect("should never happen")
        };
        let unit_handle = if let Some(handle) = self.unit_handle.take()
            && handle.is_started()
        {
            handle.child(self.manager_weak.clone(), self.handle_id, runner_handle)
        } else {
            UnitHandle::new(self.manager_weak.clone(), self.handle_id, runner_handle)
        };
        self.unit_handle = Some(unit_handle.clone());
        unit_handle
    }

    fn ensure_handle(
        &mut self,
        test: impl FnOnce(&UnitHandle) -> bool,
        f: impl FnOnce() -> Result<RunnerHandle, UnitError>,
    ) -> Result<Arc<UnitHandle>, UnitError> {
        if let Some(handle) = self.unit_handle.as_ref() {
            // Check
            if test(handle) {
                return Ok(handle.clone());
            }
        };
        Ok(self.set_handle(f()?))
    }

    fn set_resolved(&mut self, handle_id: UnitHandleId) {
        if self.handle_id == handle_id {
            self.resolved = true;
            if let Some(event_dispatcher) = self.event_dispatcher.as_ref() {
                event_dispatcher.trigger();
            }
        }
    }
}
