use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use crate::error::UnitError;
use crate::unit::behavior::UnitBehaviorContext;
use crate::util::event::EventDispatcher;
use crate::util::str::SmallStr;
use crate::{
    log::{Log, LogReader},
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
    /// Create a stopped unit with an empty log of `log_size` bytes.
    pub fn new(behavior: UnitBehavior, log_size: usize) -> Self {
        let log = Log::with_bytes(log_size);
        Self {
            log,
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
    ///
    /// The state is read before the lock rather than inside the call, because
    /// `ensure_handle` takes `behavior` while holding `handle_manager`: holding
    /// `behavior` while `state` waits for `handle_manager` is the other half of
    /// a deadlock between the screen and the schedule.
    pub fn dispatch(&self, event: UnitEvent) -> Option<UnitAction> {
        let state = self.state();
        self.behavior.lock().dispatch(event, state)
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
        self.note_start(handle.start());
        Ok(handle)
    }

    /// Run it, in whatever mode its behavior is on.
    pub fn start_or_resume(&self) -> Result<Arc<UnitHandle>, UnitError> {
        let handle = {
            let mut manager = self.handle_manager.lock();
            manager.ensure_handle(|h| !h.is_started(), || self.spawn())?
        };
        self.note_start(handle.start());
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
        self.note_start(handle.start());
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

    /// Say a run is beginning, above the `starting <command>` the process
    /// itself writes.
    ///
    /// Worth both lines because they answer different questions: a unit run
    /// can be several commands in a row, and only the unit knows whether this
    /// is a first start or one that displaced a run already going.
    fn note_start(&self, start: UnitStart) {
        match start {
            UnitStart::Already => {}
            UnitStart::Started => self.note("starting"),
            UnitStart::Restarted => self.note("restarting"),
        }
    }

    /// One line into the log on the unit's own behalf, not a process's.
    ///
    /// Built per line rather than kept in a field: a notes writer is a `Weak`
    /// and an id, which costs less to make than the chunk the line goes into,
    /// and a field would have to be behind a lock to be written through
    /// `&self`.
    fn note(&self, text: &str) {
        self.log.notes().write_line(text);
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

    /// Stop the current run, and the one it is still replacing.
    ///
    /// Both, because a stop during a restart otherwise leaves the outgoing
    /// process running with nothing left to end it — the incoming handle was
    /// what would have. Returns without waiting for either; the current handle
    /// stays in place so its terminal state remains observable.
    ///
    /// Which is why a run that has already finished is left alone and says
    /// nothing: the handle outliving it means its being there is no evidence
    /// there was anything to stop, and a note for a stop that moved nothing is
    /// a log saying something happened when nothing did.
    ///
    /// [`is_stopped`](RunnerState::is_stopped) and not
    /// [`is_pending`](RunnerState::is_pending), because a handle still parked
    /// at the start gate has never run and aborting it is exactly what stops
    /// it running later.
    pub fn stop(&self) {
        let stopping = {
            let manager = self.handle_manager.lock();
            let mut stopping = false;
            if let Some(handle) = manager.unit_handle.as_ref()
                && !handle.state().is_stopped()
            {
                handle.abort();
                stopping = true;
            }
            if let Some(outgoing) = manager.outgoing.as_ref()
                && !outgoing.state().is_stopped()
            {
                outgoing.abort();
                stopping = true;
            }
            stopping
        };
        // Outside the lock: writing a line takes the log's, and the manager's
        // is read by the screen on every frame.
        if stopping {
            self.note("stopping");
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

    /// Put `reader` on this unit's log, in place of building one.
    pub fn log_reader_into(&self, reader: &mut LogReader) {
        self.log.reader_into(reader);
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

/// What a [`start`](UnitHandle::start) turned out to be.
///
/// Three outcomes that leave the handle looking the same afterwards, told
/// apart only while it happens — which is why it is reported rather than
/// asked for later.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnitStart {
    /// Nothing: the run was already released, or had been replaced before it
    /// ever ran.
    Already,
    /// Released, with no run before it to see out.
    Started,
    /// Released once the run it displaced has been seen out.
    Restarted,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct UnitHandleId(NonZeroU32);

/// One run of a unit, as the thing that started it holds it.
///
/// The run it replaced is not here: it lives in the manager's slot, which is
/// the only place both runs are visible at once — see
/// [`begin`](UnitHandleManager::begin).
pub struct UnitHandle {
    manager_weak: Weak<Mutex<UnitHandleManager>>,
    handle_id: UnitHandleId,
    runner_handle: RunnerHandle,
    /// The start election. Whoever flips it is the one caller that goes on to
    /// take the outgoing run, so it is taken exactly once.
    started: AtomicBool,
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
        })
    }

    pub fn state(&self) -> RunnerState {
        self.runner_handle.state()
    }

    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    /// Release this run, seeing out the one it replaced first.
    ///
    /// Both answers come out of one pass over the manager — see
    /// [`begin`](UnitHandleManager::begin) — so the run to wait on and whether
    /// anybody still needs this unit to resolve are read at the same moment.
    ///
    /// A handle that has since been replaced does not start at all: nothing
    /// holds it any more, so a run it began would be one nobody could stop.
    ///
    /// What it answers is for the line the unit writes about it: the three
    /// cases read the same from outside — the handle is started — and only
    /// here is it still known which of them happened.
    pub fn start(self: &Arc<Self>) -> UnitStart {
        if self
            .started
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return UnitStart::Already;
        }
        let Some(manager) = self.manager_weak.upgrade() else {
            return UnitStart::Already;
        };
        let Some((outgoing, needs_resolve)) = manager.lock().begin(self.handle_id) else {
            return UnitStart::Already;
        };
        // Only wait if the outgoing run has not finished on its own.
        if let Some(outgoing) = outgoing
            && outgoing.state().is_pending()
        {
            let handle = self.clone();
            _ = tokio::spawn(async move {
                outgoing.abort_and_wait().await;
                handle.runner_handle.start();
                if needs_resolve {
                    handle.resolve_task().await;
                }
            });
            return UnitStart::Restarted;
        }
        self.runner_handle.start();
        if needs_resolve {
            tokio::spawn(self.clone().resolve_task());
        }
        UnitStart::Started
    }

    pub fn abort(&self) {
        self.runner_handle.abort();
    }

    pub async fn abort_and_wait(&self) {
        self.runner_handle.abort();
        self.runner_handle.wait().await;
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
    outgoing: Option<Arc<UnitHandle>>,
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
                outgoing: None,
                event_dispatcher: None,
                manager_weak: manager_weak.clone(),
            })
        })
    }

    /// Make a handle for `runner_handle` the current one, and put the run it
    /// replaces where the next start will find it.
    ///
    /// A departing handle that was never started has nothing to see out, and
    /// dropping it aborts the task it parked. One that was started and is
    /// displaced *again* before anybody waited on it is aborted here: the
    /// handle that would have seen it out was itself replaced, so nothing is
    /// left that could.
    fn set_handle(&mut self, runner_handle: RunnerHandle) -> Arc<UnitHandle> {
        self.handle_id.0 = if let Some(id) = self.handle_id.0.checked_add(1) {
            id
        } else {
            NonZeroU32::new(1).expect("should never happen")
        };

        // Get the current handle
        if let Some(departing) = self.unit_handle.take()
            && departing.is_started()
        {
            // Replace with the latest
            if let Some(stale) = self.outgoing.replace(departing) {
                stale.abort();
            }
        }
        let unit_handle = UnitHandle::new(self.manager_weak.clone(), self.handle_id, runner_handle);
        self.unit_handle = Some(unit_handle.clone());
        unit_handle
    }

    /// What a handle needs to start: the run it must see out, and whether
    /// anything is still waiting for this unit to resolve.
    ///
    /// `None` when `handle_id` is not the current run any more — a handle
    /// replaced between being built and being started has been dropped by
    /// everything that could stop it, so it must not begin.
    fn begin(&mut self, handle_id: UnitHandleId) -> Option<(Option<Arc<UnitHandle>>, bool)> {
        if self.handle_id != handle_id {
            return None;
        }
        Some((self.outgoing.take(), !self.resolved))
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

#[cfg(all(test, unix))]
mod test {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::util::types::SmallVecStr;

    /// A unit running `script` under `bash`.
    ///
    /// Every script here outlives the start that replaces it, which is the
    /// only way to have an outgoing run to be wrong about.
    fn unit(script: &str) -> Unit {
        let mut argv = SmallVecStr::new();
        argv.push(SmallStr::new("bash"));
        argv.push(SmallStr::new("-c"));
        argv.push(SmallStr::new(script));
        // A size, not the size: nothing here reads the log back, so the
        // smallest one that holds a line will do.
        Unit::new(UnitBehavior::run(SmallStr::new("test"), argv), 4096)
    }

    /// The window the manager's id check closes.
    ///
    /// [`Unit::start`] releases the manager lock between building a handle and
    /// releasing it, so a second start can replace the first in between. The
    /// replaced one must not run: nothing holds it any more, so a process it
    /// spawned would have nobody left to stop it.
    #[tokio::test]
    async fn a_handle_replaced_before_it_started_does_not_run() {
        let unit = unit("sleep 30");
        let superseded = unit.handle_manager.lock().set_handle(unit.spawn().unwrap());
        let current = unit.handle_manager.lock().set_handle(unit.spawn().unwrap());

        superseded.start();
        current.start();

        // Only the current one may be waited on: `wait_for` opens the start
        // gate, so asking the superseded handle anything would start it.
        assert!(current.runner_handle.wait_for(|s| s.is_started()).await);
        assert_eq!(superseded.state(), RunnerState::Waiting);

        current.abort_and_wait().await;
    }

    /// Three runs, each asked for while the last was still going: none of them
    /// overlaps, and none of them is left behind.
    ///
    /// The script records its own pid, so the assertions are about processes
    /// and not about what the handles report — a state machine agreeing with
    /// itself is not the thing at risk here.
    #[tokio::test]
    async fn overlapping_restarts_leave_no_process_behind() {
        let path = std::env::temp_dir().join(format!("tush-restart-{}.pids", std::process::id()));
        _ = std::fs::remove_file(&path);
        let unit = unit(&format!("echo $$ >> {}; sleep 30", path.display()));

        let mut handles = Vec::new();
        for run in 1..=3 {
            handles.push(unit.start().unwrap());
            let pids = wait_for_pids(&path, run).await;
            // The incoming run cannot exist until the outgoing one is gone.
            for pid in &pids[..pids.len() - 1] {
                assert!(!alive(*pid), "pid {pid} was still up when run {run} began");
            }
        }

        unit.stop();
        for handle in &handles {
            handle.abort_and_wait().await;
        }

        let pids = read_pids(&path);
        assert_eq!(pids.len(), 3, "each start should have reached the script");
        for pid in pids {
            assert!(!alive(pid), "pid {pid} survived the unit");
        }
        _ = std::fs::remove_file(&path);
    }

    /// The pids recorded so far, once there are `count` of them.
    ///
    /// Polled, because the run writes its pid on its own schedule; the
    /// deadline is what turns a run that never starts into a failure rather
    /// than a hang.
    async fn wait_for_pids(path: &Path, count: usize) -> Vec<u32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let pids = read_pids(path);
            if pids.len() >= count {
                return pids;
            }
            assert!(
                Instant::now() < deadline,
                "only {} of {count} runs reached the script",
                pids.len()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn read_pids(path: &Path) -> Vec<u32> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    }

    /// `kill(pid, 0)` fails with `ESRCH` only when the process is gone, which
    /// makes it the liveness test. `EPERM` is one we may not signal, and that
    /// still counts as alive.
    fn alive(pid: u32) -> bool {
        if unsafe { libc::kill(pid as i32, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}
