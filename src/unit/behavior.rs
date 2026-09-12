use std::sync::Arc;

use anyhow::{Result, bail};
use arcstr::ArcStr;
use enum_dispatch::enum_dispatch;
use tokio::process::Command;

use crate::{
    base::Process,
    log::LogWriterRef,
    runner::{RunnerHandle, RunnerSerial, RunnerState},
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
    /// What it is called.
    ///
    /// An [`ArcStr`] rather than a [`String`] because of where it is asked
    /// for: a view refreshing several times a second reads the current mode's
    /// name out of here on every frame, and the behavior is behind a lock, so
    /// nothing may borrow out of it. With a `String` that is an allocation
    /// and a copy per unit per frame, for text that never changes; with this
    /// it is a refcount.
    name: ArcStr,
    inner: UnitBehaviorInner,
}

impl UnitBehavior {
    /// One command, as its argv: the program, then its arguments.
    pub fn run(name: impl Into<ArcStr>, command: Vec<String>) -> Self {
        Self::run_many(name, vec![command])
    }

    /// Several commands, run one after the other, stopping at the first that
    /// fails.
    ///
    /// One unit still, with one log and one state — the sequence is a
    /// [`RunnerSerial`], which is itself a single runner.
    pub fn run_many(name: impl Into<ArcStr>, commands: Vec<Vec<String>>) -> Self {
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
    pub fn modes(name: impl Into<ArcStr>, modes: Vec<UnitBehavior>) -> Self {
        Self::wrap(
            name,
            UnitBehaviorInner::Modes(BehaviorModes { index: 0, modes }),
        )
    }

    /// A behavior that does nothing and succeeds immediately.
    pub fn noop(name: impl Into<ArcStr>) -> Self {
        Self::wrap(name, UnitBehaviorInner::Noop(BehaviorNoop {}))
    }

    /// What this behavior is called: the proc, or the mode.
    ///
    /// By value, because it is cheap to hand over and the callers that want
    /// it want it out from under the lock the behavior sits behind.
    pub fn name(&self) -> ArcStr {
        self.name.clone()
    }

    /// Put a kind behind the wrapper the rest of the crate sees.
    fn wrap(name: impl Into<ArcStr>, inner: UnitBehaviorInner) -> Self {
        Self {
            name: name.into(),
            inner,
        }
    }

    /// Which of its modes is current, for a behavior that has any.
    ///
    /// The mode's own [`name`](UnitBehavior::name) — `Build`, `Watch` — and
    /// `None` for a behavior that runs only one way, which is most of them.
    /// The distinction is the point: there is nothing to show for a proc that
    /// has no modes, and a made up label for it would be noise on every row.
    pub fn mode(&self) -> Option<ArcStr> {
        self.inner.mode()
    }

    /// Hand an event to the behavior, and take the action it asks for.
    pub fn dispatch(&mut self, event: UnitEvent, state: RunnerState) -> Option<UnitAction> {
        self.inner.dispatch(event, state)
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
    /// What an event asks of this behavior, given where its run is.
    ///
    /// Nothing, by default. A kind with no answer to an event should not
    /// invent one — and [`BehaviorNoop`] never will have one, because a proc
    /// that declared no way to run has nothing an event could ask of it.
    fn dispatch(&mut self, _event: UnitEvent, _state: RunnerState) -> Option<UnitAction> {
        None
    }
    /// Which of its modes is current, for the kinds that have any.
    fn mode(&self) -> Option<ArcStr> {
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
    /// Start it if nothing is running, and leave a running one alone.
    ///
    /// Pressing the key again on something already up should not take it down
    /// and bring it back: the restart is what the user did *not* ask for.
    ///
    /// Every way of not running is a way of being startable, the three
    /// terminal states included — a run that finished, failed or was killed
    /// is a run you can have again.
    fn dispatch(&mut self, event: UnitEvent, state: RunnerState) -> Option<UnitAction> {
        match event {
            UnitEvent::Default => state.is_stopped().then_some(UnitAction::Start),
        }
    }

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
    index: usize,
    modes: Vec<UnitBehavior>,
}

impl UnitBehaviorKind for BehaviorModes {
    fn mode(&self) -> Option<ArcStr> {
        Some(self.modes.get(self.index)?.name())
    }

    /// Move to the next mode and run it — except the very first time.
    ///
    /// A unit with modes is already *on* one before it has ever run, and that
    /// mode is the first one, which is what the config wrote first and what
    /// the row on screen has been saying all along. Stepping past it would
    /// make the first press start something other than what it offered, and
    /// there would be no way to run the first mode at all without cycling the
    /// whole way round.
    ///
    /// [`Stopped`](RunnerState::Stopped) is exactly that case and nothing
    /// else: a unit reports it only while it has no run behind it, since a
    /// handle is never in that state — one that finished says so, and says
    /// how.
    fn dispatch(&mut self, event: UnitEvent, state: RunnerState) -> Option<UnitAction> {
        if self.modes.is_empty() {
            return None;
        }
        match event {
            UnitEvent::Default => {
                if !matches!(state, RunnerState::Stopped) {
                    self.index = (self.index + 1) % self.modes.len();
                }
                Some(UnitAction::Start)
            }
        }
    }

    fn spawn(&self, writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        if self.modes.is_empty() {
            return Ok(RunnerHandle::new(()));
        };
        let mode = &self.modes[self.index];
        mode.spawn(writer)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Ask a behavior what it would do from `state`.
    fn dispatch(behavior: &mut UnitBehavior, state: RunnerState) -> Option<UnitAction> {
        behavior.dispatch(UnitEvent::Default, state)
    }

    fn run() -> UnitBehavior {
        UnitBehavior::run("proc", vec!["true".into()])
    }

    fn modes() -> UnitBehavior {
        UnitBehavior::modes(
            "proc",
            vec![
                UnitBehavior::run("Build", vec!["true".into()]),
                UnitBehavior::run("Watch", vec!["true".into()]),
            ],
        )
    }

    /// Pressing the key on something already up should not take it down and
    /// bring it back: the restart is what was not asked for.
    #[test]
    fn a_plain_run_is_left_alone_while_it_is_running() {
        let mut behavior = run();
        for state in [
            RunnerState::Waiting,
            RunnerState::Started,
            RunnerState::Running,
            RunnerState::Killing,
        ] {
            assert!(dispatch(&mut behavior, state).is_none(), "{state:?}");
        }
    }

    /// Every way of not running is a way of being startable, including the
    /// three that mean it ran and stopped.
    #[test]
    fn a_plain_run_starts_again_from_any_state_that_is_not_running() {
        let mut behavior = run();
        for state in [
            RunnerState::Stopped,
            RunnerState::ExitSuccess,
            RunnerState::ExitError(None),
            RunnerState::Killed(None),
        ] {
            assert!(
                matches!(dispatch(&mut behavior, state), Some(UnitAction::Start)),
                "{state:?}"
            );
        }
    }

    /// The first press runs the mode the unit was already showing. Stepping
    /// past it would start something other than what the row offered, and
    /// leave no way to run the first mode without cycling all the way round.
    #[test]
    fn the_first_press_runs_the_mode_it_was_already_on() {
        let mut behavior = modes();
        assert_eq!(behavior.mode().as_deref(), Some("Build"));

        assert!(matches!(
            dispatch(&mut behavior, RunnerState::Stopped),
            Some(UnitAction::Start)
        ));
        assert_eq!(
            behavior.mode().as_deref(),
            Some("Build"),
            "it stepped past the first"
        );
    }

    /// After that every press moves on, whether the run is still going or
    /// already over — a unit with modes is a unit you cycle.
    #[test]
    fn every_press_after_the_first_moves_to_the_next_mode() {
        let mut behavior = modes();
        dispatch(&mut behavior, RunnerState::Stopped);

        dispatch(&mut behavior, RunnerState::Running);
        assert_eq!(behavior.mode().as_deref(), Some("Watch"));
        dispatch(&mut behavior, RunnerState::ExitSuccess);
        assert_eq!(
            behavior.mode().as_deref(),
            Some("Build"),
            "it should wrap round"
        );
    }

    /// Only `Stopped` means never run, so a unit that ran and finished cycles
    /// like any other — the state a unit reports with no handle is the one
    /// case, and a finished handle says how it finished instead.
    #[test]
    fn a_finished_run_is_not_a_first_press() {
        let mut behavior = modes();
        dispatch(&mut behavior, RunnerState::Killed(None));
        assert_eq!(behavior.mode().as_deref(), Some("Watch"));
    }

    /// A proc that declared no way to run has nothing an event could ask of
    /// it, in any state.
    #[test]
    fn a_noop_answers_nothing() {
        let mut behavior = UnitBehavior::noop("proc");
        for state in [
            RunnerState::Stopped,
            RunnerState::Running,
            RunnerState::ExitSuccess,
        ] {
            assert!(dispatch(&mut behavior, state).is_none(), "{state:?}");
        }
    }
}
