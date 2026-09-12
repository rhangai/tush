use std::sync::Arc;

use anyhow::{Result, bail};
use enum_dispatch::enum_dispatch;
use tokio::process::Command;

use crate::{
    base::Process,
    log::LogWriterRef,
    runner::{RunnerHandle, RunnerSerial},
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
/// This is the seam the config file (see `tmp/example.yaml`) is parsed into:
/// a proc's `run` becomes [`run`](UnitBehavior::run) or
/// [`run_many`](UnitBehavior::run_many), and its `modes` become
/// [`modes`](UnitBehavior::modes) over one behavior per mode.
///
/// # The name
///
/// Every behavior carries one, and it means different things at different
/// depths: for the behavior a unit holds it is the proc, and for the
/// behaviors inside a [`modes`](UnitBehavior::modes) it is the mode — `Build`,
/// `Watch`. Which is the point of keeping it on the wrapper rather than on
/// the unit: a mode is a behavior, so a mode has a name the same way.
pub struct UnitBehavior {
    name: String,
    inner: UnitBehaviorInner,
}

impl UnitBehavior {
    /// One command, as its argv: the program, then its arguments.
    pub fn run(name: String, command: Vec<String>) -> Self {
        Self::run_many(name, vec![command])
    }

    /// Several commands, run one after the other, stopping at the first that
    /// fails.
    ///
    /// One unit still, with one log and one state — the sequence is a
    /// [`RunnerSerial`], which is itself a single runner.
    pub fn run_many(name: String, commands: Vec<Vec<String>>) -> Self {
        Self::wrap(name, UnitBehaviorInner::Run(BehaviorRun { commands }))
    }

    /// Several named ways to run, one of which is current.
    ///
    /// Each mode is a whole [`UnitBehavior`], which is what lets a mode be
    /// anything a proc can be — one command, or a sequence of them — and what
    /// gives it its name.
    ///
    /// Today it always runs the first. Choosing between them is a
    /// [`dispatch`](UnitBehavior::dispatch) away, and that is what the `&mut
    /// self` there is for.
    pub fn modes(name: String, modes: Vec<UnitBehavior>) -> Self {
        Self::wrap(name, UnitBehaviorInner::Modes(BehaviorModes { modes }))
    }

    /// A behavior that does nothing and succeeds immediately.
    pub fn noop(name: String) -> Self {
        Self::wrap(name, UnitBehaviorInner::Noop(BehaviorNoop {}))
    }

    /// What this behavior is called: the proc, or the mode.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Put a kind behind the wrapper the rest of the crate sees.
    fn wrap(name: impl Into<String>, inner: UnitBehaviorInner) -> Self {
        Self {
            name: name.into(),
            inner,
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

/// Runs nothing, succeeding immediately, via the `()` runner.
struct BehaviorNoop {}
impl UnitBehaviorKind for BehaviorNoop {
    fn spawn(&self, _writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        Ok(RunnerHandle::new(()))
    }
}

/// Runs external commands, in order.
///
/// Always a [`RunnerSerial`], even for one command, so that the one and the
/// many are the same shape — there is no second path through here for the
/// common case to drift away from.
struct BehaviorRun {
    /// Each command as its argv, in the order they were written.
    commands: Vec<Vec<String>>,
}

impl UnitBehaviorKind for BehaviorRun {
    fn spawn(&self, writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        if self.commands.is_empty() {
            return Ok(RunnerHandle::new(()));
        }
        if self.commands.len() == 1 {
            let process = Process::new(command(&self.commands[0])?, writer);
            return Ok(RunnerHandle::new(process));
        }
        let mut serial = RunnerSerial::new();
        for argv in &self.commands {
            let writer = writer.as_ref().map(LogWriterRef::share);
            serial.add(Process::new(command(argv)?, writer));
        }
        Ok(RunnerHandle::new(serial))
    }
}

/// Builds the child from an argv.
///
/// The first word is the program and the rest are its arguments, handed to
/// the OS as they are: no shell, so nothing re-splits them and no quoting
/// rule applies.
fn command(argv: &[String]) -> Result<Command> {
    let Some((program, args)) = argv.split_first() else {
        bail!("a command with no program to run");
    };
    let mut command = Command::new(program);
    command.args(args);
    Ok(command)
}

/// Several behaviors, one of which is the one that runs.
///
/// The first of them, for now. What makes this a kind of its own rather than
/// a `Vec` on the unit is that choosing is going to be its own behavior: the
/// `dispatch` that switches modes belongs here, next to the list it switches
/// within.
struct BehaviorModes {
    modes: Vec<UnitBehavior>,
}

impl UnitBehaviorKind for BehaviorModes {
    fn spawn(&self, writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        match self.modes.first() {
            Some(mode) => mode.spawn(writer),
            // A proc whose `modes` list is empty has nothing to start, which
            // is what the noop runner is.
            None => Ok(RunnerHandle::new(())),
        }
    }
}
