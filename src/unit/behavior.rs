use std::sync::Arc;

use anyhow::Result;
use enum_dispatch::enum_dispatch;
use tokio::process::Command;

use crate::{
    base::Process,
    log::LogWriterRef,
    runner::RunnerHandle,
    unit::dispatch::{UnitAction, UnitEvent},
};

/// What a [`Unit`](crate::unit::Unit) does: what it spawns when started, and
/// how it answers the events that reach it.
///
/// Behavior rather than description, because it is not inert. A description
/// would be read; this is *asked* — [`dispatch`](UnitBehavior::dispatch) takes
/// `&mut self` and may answer with a [`UnitAction`], which is how a behavior
/// like `modes` gets to remember which mode it is in. That is also why the
/// unit keeps it behind a lock.
///
/// The concrete kinds live behind [`UnitBehaviorInner`]; this wrapper is what
/// the rest of the crate sees, which keeps new kinds from leaking into every
/// signature.
///
/// This is the seam where the config file (see `tmp/example.yaml`) will be
/// parsed into: today only the hardcoded [`run`](UnitBehavior::run),
/// [`noop`](UnitBehavior::noop) and `modes` exist.
pub struct UnitBehavior {
    inner: UnitBehaviorInner,
}

impl UnitBehavior {
    /// A placeholder behavior running a hardcoded shell command.
    pub fn run() -> Self {
        Self {
            inner: UnitBehaviorInner::Run(BehaviorRun {}),
        }
    }

    /// A behavior that does nothing and succeeds immediately.
    pub fn noop() -> Self {
        Self {
            inner: UnitBehaviorInner::Noop(BehaviorNoop {}),
        }
    }

    /// Hand an event to the behavior, and take the action it asks for.
    pub fn dispatch(&mut self, event: UnitEvent) -> Option<UnitAction> {
        self.inner.dispatch(event)
    }

    /// Build the runner and hand back a paused handle for it.
    ///
    /// The handle comes back parked at the start gate; releasing it is the
    /// caller's job — see [`Unit::start`](crate::unit::Unit::start).
    /// `writer` is the log the process output should be sent to, if any.
    pub fn spawn(&self, writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        self.inner.spawn(writer)
    }
}

impl AsRef<UnitBehavior> for UnitBehavior {
    fn as_ref(&self) -> &UnitBehavior {
        self
    }
}

/// The kinds of behavior that exist.
///
/// The `enum_dispatch` macro generates the delegation of
/// [`UnitBehaviorKind`] to each variant, so dispatch stays static — no
/// `Box<dyn ...>` on a path that is otherwise allocation free.
#[enum_dispatch]
enum UnitBehaviorInner {
    Noop(BehaviorNoop),
    Run(BehaviorRun),
    Modes(BehaviorModes),
}

/// What every kind of behavior must be able to do.
#[enum_dispatch(UnitBehaviorInner)]
trait UnitBehaviorKind {
    /// Dispatch a event that may trigger an action
    fn dispatch(&mut self, _event: UnitEvent) -> Option<UnitAction> {
        None
    }
    /// Build the runner for one run and wrap it in a paused handle.
    fn spawn(&self, writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>>;
}

/// Runs an external command.
///
/// The command is hardcoded for now — a shell script that prints, sleeps and
/// exits non zero, which exercises log capture, the wait path and a failing
/// exit code in one go.
struct BehaviorRun {}
impl UnitBehaviorKind for BehaviorRun {
    fn spawn(&self, writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        let mut command = Command::new("find");
        command.args([".", "-type", "f"]);
        let proc = Process::new(command, writer);
        Ok(RunnerHandle::new(proc))
    }
}

/// Runs nothing, succeeding immediately, via the `()` runner.
struct BehaviorNoop {}
impl UnitBehaviorKind for BehaviorNoop {
    fn spawn(&self, _writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        Ok(RunnerHandle::new(()))
    }
}

/// Runs nothing, succeeding immediately, via the `()` runner.
struct BehaviorModes {}
impl UnitBehaviorKind for BehaviorModes {
    fn spawn(&self, _writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        Ok(RunnerHandle::new(()))
    }
}
