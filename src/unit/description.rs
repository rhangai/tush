use std::sync::Arc;

use anyhow::Result;
use enum_dispatch::enum_dispatch;
use tokio::process::Command;

use crate::{
    base::{LogWriterRef, Process},
    runner::RunnerHandle,
};

/// The recipe for a run: what a [`Unit`](crate::unit::Unit) spawns when started.
///
/// A description is inert and reusable — spawning it does not consume it, so
/// the same description can back any number of runs. The concrete kinds live
/// behind [`UnitDescriptionInner`]; this wrapper is what the rest of the crate
/// sees, which keeps new kinds from leaking into every signature.
///
/// This is the seam where the config file (see `tmp/example.yaml`) will be
/// parsed into: today only the hardcoded [`program`](UnitDescription::program)
/// and [`noop`](UnitDescription::noop) exist.
pub struct UnitDescription {
    inner: UnitDescriptionInner,
}

impl UnitDescription {
    /// A placeholder description running a hardcoded shell command.
    pub fn program() -> Self {
        Self {
            inner: UnitDescriptionInner::Program(DescProgram {}),
        }
    }

    /// A description that does nothing and succeeds immediately.
    pub fn noop() -> Self {
        Self {
            inner: UnitDescriptionInner::Noop(DescNoop {}),
        }
    }

    /// Build the runner and hand back a paused handle for it.
    ///
    /// The handle comes back parked at the start gate; releasing it is the
    /// caller's job — see [`Unit::start_with_description`](crate::unit::Unit).
    /// `writer` is the log the process output should be sent to, if any.
    pub fn spawn(&self, writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        self.inner.spawn(writer)
    }
}

impl AsRef<UnitDescription> for UnitDescription {
    fn as_ref(&self) -> &UnitDescription {
        self
    }
}

/// The kinds of description that exist.
///
/// The `enum_dispatch` macro generates the delegation of
/// [`UnitDescriptionBehavior`] to each variant, so dispatch stays static — no
/// `Box<dyn ...>` on a path that is otherwise allocation free.
#[enum_dispatch]
enum UnitDescriptionInner {
    Noop(DescNoop),
    Program(DescProgram),
}

/// What every kind of description must be able to do.
#[enum_dispatch(UnitDescriptionInner)]
trait UnitDescriptionBehavior {
    /// Build the runner for one run and wrap it in a paused handle.
    fn spawn(&self, writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>>;
}

/// Runs an external command.
///
/// The command is hardcoded for now — a shell script that prints, sleeps and
/// exits non zero, which exercises log capture, the wait path and a failing
/// exit code in one go.
struct DescProgram {}
impl UnitDescriptionBehavior for DescProgram {
    fn spawn(&self, writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        let mut command = Command::new("bash");
        command.args(["-c", "echo 'oi'; sleep 1; echo 'tchau'; exit 2"]);
        let proc = Process::new(command, writer);
        Ok(RunnerHandle::new(proc))
    }
}

/// Runs nothing, succeeding immediately, via the `()` runner.
struct DescNoop {}
impl UnitDescriptionBehavior for DescNoop {
    fn spawn(&self, _writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        Ok(RunnerHandle::new(()))
    }
}
