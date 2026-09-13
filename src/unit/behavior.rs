use std::sync::Arc;

use anyhow::{Result, bail};
use arcstr::ArcStr;
use enum_dispatch::enum_dispatch;
use tokio::process::Command;

use crate::{
    base::Process,
    log::LogWriterRef,
    runner::{RunnerHandle, RunnerSerial, RunnerState},
    unit::dispatch::{UnitAction, UnitChoice, UnitEvent},
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
    pub fn run(name: impl Into<ArcStr>, command: Vec<ArcStr>) -> Self {
        Self::run_many(name, vec![command])
    }

    /// Several commands, run one after the other, stopping at the first that
    /// fails.
    ///
    /// One unit still, with one log and one state — the sequence is a
    /// [`RunnerSerial`], which is itself a single runner.
    pub fn run_many(name: impl Into<ArcStr>, commands: Vec<Vec<ArcStr>>) -> Self {
        Self::wrap(name, UnitBehaviorInner::Run(BehaviorRun { commands }))
    }

    /// Several named ways to run, one of which is current.
    ///
    /// Each mode is a whole [`UnitBehavior`], which is what lets a mode be
    /// anything a proc can be — one command, or a sequence of them — and what
    /// gives it its name.
    ///
    /// It starts on the first, which is what the config wrote first, and
    /// moves only when a [`StartMode`](UnitEvent::StartMode) picks another —
    /// which is what the `&mut self` on [`dispatch`](UnitBehavior::dispatch)
    /// is for.
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
    ///
    /// [`Stop`](UnitEvent::Stop) is answered here rather than passed down,
    /// for the same reason it is offered here: ending a run is the same work
    /// whatever the unit runs, so no kind has to know about it and none can
    /// accidentally refuse it.
    pub fn dispatch(&mut self, event: UnitEvent, state: RunnerState) -> Option<UnitAction> {
        if let UnitEvent::Stop = event {
            return (!state.is_stopped()).then_some(UnitAction::Stop);
        }
        self.inner.dispatch(event, state)
    }

    /// Everything that can be asked of this behavior right now, written into
    /// `out`.
    ///
    /// Filled into a `Vec` the caller owns rather than returned, because the
    /// caller is a menu that opens and closes over and over while keeping one
    /// buffer: refilling it costs the [`ArcStr`] refcounts and nothing else.
    /// Cleared here rather than by the caller, so that what comes back is the
    /// list and not the list appended to whatever was there.
    ///
    /// [`Stop`](UnitEvent::Stop) is appended here and by no kind, because
    /// ending a run is not a property of the recipe. Doing it once, here, is
    /// also what makes it the last entry of every menu — in the same place
    /// every time, for every unit.
    pub fn choices(&self, state: RunnerState, out: &mut Vec<UnitChoice>) {
        out.clear();
        self.inner.choices(state, out);
        out.push(UnitChoice {
            verb: STOP,
            mode: None,
            event: UnitEvent::Stop,
            enabled: !state.is_stopped(),
            current: false,
        });
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

    /// The ways this behavior offers to be run, in the order a menu should
    /// list them.
    ///
    /// Nothing by default, for the same reason: a proc with no way to run has
    /// nothing to offer. Its menu is the dimmed [`Stop`](UnitEvent::Stop) the
    /// wrapper appends, which is the honest picture of a proc that declared
    /// no way to run.
    fn choices(&self, _state: RunnerState, _out: &mut Vec<UnitChoice>) {}

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
    commands: Vec<Vec<ArcStr>>,
}

impl UnitBehaviorKind for BehaviorRun {
    /// One way to run, so one entry — and it is always available, because a
    /// run that is up restarts.
    ///
    /// That is a change from when this was <kbd>Enter</kbd>, which refused to
    /// touch a live run on the grounds that the restart was what the user did
    /// *not* ask for. Off a menu it is exactly what they asked for: the entry
    /// says `Restart`, and they put the cursor on it and pressed.
    fn choices(&self, state: RunnerState, out: &mut Vec<UnitChoice>) {
        out.push(UnitChoice {
            verb: verb(state),
            mode: None,
            event: UnitEvent::Start,
            enabled: true,
            current: true,
        });
    }

    /// Start it, whatever it was doing.
    ///
    /// Nothing sends this blind any more — it is one entry of a list this
    /// behavior wrote and the user read — so there is nothing left here to
    /// protect a running unit from.
    ///
    /// [`StartMode`](UnitEvent::StartMode) is refused because there are no
    /// modes to move between: inventing an index would run the one command
    /// under a name it does not have. [`Stop`](UnitEvent::Stop) never arrives,
    /// having been answered by the wrapper.
    fn dispatch(&mut self, event: UnitEvent, _state: RunnerState) -> Option<UnitAction> {
        match event {
            UnitEvent::Start => Some(UnitAction::Start),
            UnitEvent::StartMode(_) | UnitEvent::Stop => None,
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

/// The three words a menu entry can begin with.
const START: &str = "start";
const RESTART: &str = "restart";
const STOP: &str = "stop";

/// What starting is called from `state`.
///
/// `Restart` when there is a run to replace, which is the difference between
/// an entry that warns you what it is about to do and one that does it.
fn verb(state: RunnerState) -> &'static str {
    if state.is_stopped() { START } else { RESTART }
}

/// Builds the child from an argv.
///
/// The first word is the program and the rest are its arguments, handed to
/// the OS as they are: no shell, so nothing re-splits them and no quoting
/// rule applies.
fn command(argv: &[ArcStr]) -> Result<Command> {
    let Some((program, args)) = argv.split_first() else {
        bail!("a command with no program to run");
    };
    let mut command = Command::new(program.as_str());
    command.args(args.iter().map(|i| i.as_str()));
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

    /// One entry per mode, in the order the config wrote them, each naming
    /// the mode it would run.
    ///
    /// The mode it is on reads `Restart` while a run is up and `Start`
    /// otherwise. The others always read `Start`, even though picking one
    /// does take the current run down: what the entry names is the run it is
    /// about to make, and that one is starting. The run it replaces is named
    /// by the entry above it, which says `Restart` and is where the cursor
    /// already was.
    fn choices(&self, state: RunnerState, out: &mut Vec<UnitChoice>) {
        for (index, mode) in self.modes.iter().enumerate() {
            let current = index == self.index;
            out.push(UnitChoice {
                verb: if current { verb(state) } else { START },
                mode: Some(mode.name()),
                event: UnitEvent::StartMode(index),
                enabled: true,
                current,
            });
        }
    }

    /// Move onto the mode that was picked, and run it.
    ///
    /// Moving the index is the whole reason this is an event and not a
    /// command: [`spawn`](UnitBehaviorKind::spawn) reads it, so picking a
    /// mode and running it are one step and there is no window in which the
    /// unit is on a mode it is not running.
    ///
    /// An index that is not a mode is refused rather than clamped. It cannot
    /// have come from a list this behavior wrote, so it is a caller that made
    /// one up — and running some other mode than the one asked for is a worse
    /// answer than running none.
    fn dispatch(&mut self, event: UnitEvent, _state: RunnerState) -> Option<UnitAction> {
        match event {
            UnitEvent::Start => (!self.modes.is_empty()).then_some(UnitAction::Start),
            UnitEvent::StartMode(index) => {
                if index >= self.modes.len() {
                    return None;
                }
                self.index = index;
                Some(UnitAction::Start)
            }
            UnitEvent::Stop => None,
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
