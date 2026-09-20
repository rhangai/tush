use std::sync::Arc;

use enum_dispatch::enum_dispatch;
use tokio::process::Command;

use crate::error::UnitError;
use crate::util::event::EventDispatcher;
use crate::util::str::SmallStr;
use crate::{
    base::Process,
    log::LogWriterRef,
    runner::{RunnerHandle, RunnerSerial, RunnerState},
    unit::dispatch::{UnitAction, UnitChoice, UnitEvent},
    util::types::{SmallMultiVecStr, SmallVecStr},
};

/// What a behavior is handed to spawn with: the unit's surroundings, not the
/// behavior's own configuration.
///
/// One parameter rather than one per thing. It replaced a bare
/// `Option<LogWriterRef>` as soon as there was a second such thing to pass,
/// and that swap rewrote every [`spawn`](UnitBehaviorKind::spawn) in the
/// module; the next thing to add should not.
///
/// Both fields are optional: a unit built without a log, or with nobody
/// listening, still runs.
pub struct UnitBehaviorContext {
    writer: Option<LogWriterRef>,
    spawn_paused: bool,
    event_dispatcher: Option<EventDispatcher>,
}

impl UnitBehaviorContext {
    pub fn new() -> Self {
        Self {
            writer: None,
            spawn_paused: false,
            event_dispatcher: None,
        }
    }
    pub fn spawn_paused(self) -> Self {
        Self {
            spawn_paused: true,
            ..self
        }
    }
    pub fn with_writer(self, writer: LogWriterRef) -> Self {
        Self {
            writer: Some(writer),
            ..self
        }
    }
    pub fn with_event_dispatcher(self, event_dispatcher: EventDispatcher) -> Self {
        Self {
            event_dispatcher: Some(event_dispatcher),
            ..self
        }
    }
}

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
    /// An [`SmallStr`] rather than a [`String`] because of where it is asked
    /// for: a view refreshing several times a second reads the current mode's
    /// name out of here on every frame, and the behavior is behind a lock, so
    /// nothing may borrow out of it. With a `String` that is an allocation
    /// and a copy per unit per frame, for text that never changes; a name fits
    /// inside a [`SmallStr`], so here it is the copy alone.
    name: SmallStr,
    /// A shorter name for it, when the config declared one.
    ///
    /// Kept as an `Option` and never folded into [`name`](Self::name): only a
    /// view knows whether it has the room for the long one, and a fallback
    /// taken here would reach it as a short name somebody chose.
    short: Option<SmallStr>,
    inner: UnitBehaviorInner,
}

impl UnitBehavior {
    /// One command, as its argv: the program, then its arguments.
    pub fn run(name: SmallStr, command: SmallVecStr) -> Self {
        let mut commands = SmallMultiVecStr::new();
        commands.push(command);
        Self::run_many(name, Arc::new(commands))
    }

    /// Several commands, run one after the other, stopping at the first that
    /// fails.
    ///
    /// One unit still, with one log and one state — the sequence is a
    /// [`RunnerSerial`], which is itself a single runner.
    ///
    /// Shared and not owned: the commands are read at spawn and never written,
    /// and an `Arc` is what keeps them out of the enum every behavior is — 32
    /// bytes carried by each no-op and each mode, rather than 208.
    pub fn run_many(name: SmallStr, commands: Arc<SmallMultiVecStr>) -> Self {
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
    pub fn modes(name: SmallStr, modes: impl Iterator<Item = UnitBehavior>) -> Self {
        Self::wrap(
            name,
            UnitBehaviorInner::Modes(BehaviorModes {
                index: 0,
                modes: modes.collect(),
            }),
        )
    }

    /// A behavior that does nothing and succeeds immediately.
    pub fn noop(name: SmallStr) -> Self {
        Self::wrap(name, UnitBehaviorInner::Noop(BehaviorNoop {}))
    }

    /// Give it the short name the config declared, if it declared one.
    ///
    /// Takes the `Option` rather than the name so that a caller holding the
    /// config's field passes it straight through — `None` is a behavior with
    /// no short name, which is what it already was.
    pub fn with_short(mut self, short: Option<SmallStr>) -> Self {
        self.short = short;
        self
    }

    /// What this behavior is called: the proc, or the mode.
    ///
    /// By value, because it is cheap to hand over and the callers that want
    /// it want it out from under the lock the behavior sits behind.
    pub fn name(&self) -> SmallStr {
        self.name.clone()
    }

    /// The shorter name for it, or `None` where none was declared.
    pub fn name_short(&self) -> Option<SmallStr> {
        self.short.clone()
    }

    /// Put a kind behind the wrapper the rest of the crate sees.
    fn wrap(name: SmallStr, inner: UnitBehaviorInner) -> Self {
        Self {
            name,
            short: None,
            inner,
        }
    }

    /// Which of its modes is current, for a behavior that has any.
    ///
    /// The mode's own [`name`](UnitBehavior::name) — `Build`, `Watch` — and
    /// `None` for a behavior that runs only one way, which is most of them.
    /// The distinction is the point: there is nothing to show for a proc that
    /// has no modes, and a made up label for it would be noise on every row.
    pub fn mode(&self) -> Option<SmallStr> {
        self.inner.mode()
    }

    /// The current mode's short name, which is `None` both for a behavior
    /// with no modes and for a mode that declared none.
    pub fn mode_short(&self) -> Option<SmallStr> {
        self.inner.mode_short()
    }

    /// Hand an event to the behavior, and take the action it asks for.
    ///
    /// [`Stop`](UnitEvent::Stop) is answered here rather than passed down:
    /// ending a run is the same work whatever the unit runs, so no kind has
    /// to know about it and none can refuse it by mistake.
    pub fn dispatch(&mut self, event: UnitEvent, state: RunnerState) -> Option<UnitAction> {
        if let UnitEvent::Stop = event {
            return (!state.is_stopped()).then_some(UnitAction::Stop);
        }
        self.inner.dispatch(event, state)
    }

    /// Everything that can be asked of this behavior right now.
    ///
    /// Into a `Vec` the caller owns and reuses, so a menu that opens over and
    /// over refills that capacity rather than asking for it again.
    ///
    /// [`Stop`](UnitEvent::Stop) is appended here and by no kind, which is
    /// also what makes it the last entry of every menu.
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
    pub fn spawn(&self, ctx: UnitBehaviorContext) -> Result<RunnerHandle, UnitError> {
        self.inner.spawn(ctx)
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

    /// The ways this behavior offers to be run, in the order to list them.
    ///
    /// Nothing by default: a proc with no way to run has nothing to offer,
    /// and a menu of one dim `Stop` is the honest picture of it.
    fn choices(&self, _state: RunnerState, _out: &mut Vec<UnitChoice>) {}

    /// Which of its modes is current, for the kinds that have any.
    fn mode(&self) -> Option<SmallStr> {
        None
    }

    /// That mode's short name, on the same terms.
    fn mode_short(&self) -> Option<SmallStr> {
        None
    }

    /// Build the runner for one run and wrap it in a paused handle.
    fn spawn(&self, ctx: UnitBehaviorContext) -> Result<RunnerHandle, UnitError>;
}

/// Runs nothing, succeeding immediately, via the `()` runner.
struct BehaviorNoop {}
impl UnitBehaviorKind for BehaviorNoop {
    fn spawn(&self, _ctx: UnitBehaviorContext) -> Result<RunnerHandle, UnitError> {
        Ok(RunnerHandle::new(()))
    }
}

/// Runs external commands, in order.
///
/// Always a [`RunnerSerial`], even for one command, so that the one and the
/// many are the same shape — there is no second path through here for the
/// common case to drift away from.
struct BehaviorRun {
    /// Each command as its argv, in the order they were written, shared with
    /// whatever else holds the same proc's run.
    commands: Arc<SmallMultiVecStr>,
}

impl UnitBehaviorKind for BehaviorRun {
    /// One way to run, so one entry, always available: a run that is up
    /// restarts. Blind <kbd>Enter</kbd> used to refuse that; off a menu the
    /// entry says `Restart` and the cursor was put on it.
    fn choices(&self, state: RunnerState, out: &mut Vec<UnitChoice>) {
        out.push(UnitChoice {
            verb: verb(state),
            mode: None,
            event: UnitEvent::Start,
            enabled: true,
            current: true,
        });
    }

    /// Start it, whatever it was doing: nothing sends this blind any more.
    ///
    /// [`StartMode`](UnitEvent::StartMode) is refused for want of modes to
    /// move between; [`Stop`](UnitEvent::Stop) never arrives, the wrapper
    /// having answered it.
    fn dispatch(&mut self, event: UnitEvent, _state: RunnerState) -> Option<UnitAction> {
        match event {
            UnitEvent::Start => Some(UnitAction::Start),
            UnitEvent::StartMode(_) | UnitEvent::Stop => None,
        }
    }

    fn spawn(&self, ctx: UnitBehaviorContext) -> Result<RunnerHandle, UnitError> {
        if self.commands.is_empty() {
            return Ok(RunnerHandle::new(()));
        }
        if self.commands.len() == 1 {
            let row = self.commands.get_row(0).unwrap();
            let process = Process::new(command(row)?, ctx.writer);
            return Ok(RunnerHandle::new(process));
        }
        let mut serial = RunnerSerial::new();
        for argv in self.commands.iter() {
            let writer = ctx.writer.as_ref().map(LogWriterRef::share);
            serial.add(Process::new(command(argv)?, writer));
        }
        Ok(RunnerHandle::new(serial))
    }
}

/// The three words a menu entry can begin with.
const START: &str = "start";
const RESTART: &str = "restart";
const STOP: &str = "stop";

/// `Restart` when there is a run to replace, so the entry warns you.
fn verb(state: RunnerState) -> &'static str {
    if state.is_stopped() { START } else { RESTART }
}

/// Builds the child from an argv.
///
/// The first word is the program and the rest are its arguments, handed to
/// the OS as they are: no shell, so nothing re-splits them and no quoting
/// rule applies.
fn command(argv: &[SmallStr]) -> Result<Command, UnitError> {
    let Some((program, args)) = argv.split_first() else {
        return Err(UnitError::Invalid);
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
    fn mode(&self) -> Option<SmallStr> {
        Some(self.modes.get(self.index)?.name())
    }

    fn mode_short(&self) -> Option<SmallStr> {
        self.modes.get(self.index)?.name_short()
    }

    /// One entry per mode, in the order the config wrote them.
    ///
    /// The mode it is on reads `Restart` while a run is up. The others read
    /// `Start` even though picking one takes that run down — the entry names
    /// the run it is about to make, and that one is starting.
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
    /// Moving the index is why this is an event and not a command:
    /// [`spawn`](UnitBehaviorKind::spawn) reads it, so there is no window in
    /// which the unit is on a mode it is not running.
    ///
    /// An index that is not a mode is refused rather than clamped — it cannot
    /// have come from a list this behavior wrote.
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

    fn spawn(&self, ctx: UnitBehaviorContext) -> Result<RunnerHandle, UnitError> {
        if self.modes.is_empty() {
            return Ok(RunnerHandle::new(()));
        };
        let mode = &self.modes[self.index];
        mode.spawn(ctx)
    }
}
