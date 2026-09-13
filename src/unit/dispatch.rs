use arcstr::ArcStr;

/// Something asked of a unit, for its behavior to answer.
///
/// These are the entries of a menu, which is why they are this specific: the
/// user does not press a key and hope, they pick a named thing off a list
/// that the behavior itself wrote. So there is no "do the default" event any
/// more — [`choices`](crate::unit::UnitBehavior::choices) is what says what
/// there is, and every one of these came from it.
///
/// Still events rather than commands, because what one costs is the
/// behavior's to decide: [`StartMode`](UnitEvent::StartMode) also moves the
/// unit onto that mode, which is a change to the behavior and not something
/// a caller could do from outside.
#[derive(Clone, Copy, Debug)]
pub enum UnitEvent {
    /// Run it in the mode it is already on, restarting it if it is up.
    Start,
    /// Move to the mode at this index, and run that.
    StartMode(usize),
    /// End the current run.
    Stop,
}

#[derive(Clone, Copy, Debug)]
pub enum UnitAction {
    Start,
    Stop,
}

/// One entry in the list of things a unit can be asked right now.
///
/// # Why it is not called an action
///
/// That name is taken, by what a [`dispatch`](crate::unit::UnitBehavior::dispatch)
/// *returns* — the effect the session then has to carry out. This is the
/// other end of the same exchange: what the user may ask for, before anybody
/// has worked out what it costs.
#[derive(Clone, Debug)]
pub struct UnitChoice {
    /// The verb, as a literal: `Start`, `Restart`, `Stop`.
    ///
    /// A `&'static str` and not a built label, because the menu writes this
    /// and [`mode`](UnitChoice::mode) into the buffer as two runs of text at
    /// known positions. Composing `"Restart Build"` into a `String` would be
    /// an allocation per item to say what two writes already say.
    pub verb: &'static str,
    /// The mode the verb applies to, for a unit that has modes.
    ///
    /// `None` is a proc that runs one way, and the entry is the bare verb.
    pub mode: Option<ArcStr>,
    /// What to send if it is chosen.
    pub event: UnitEvent,
    /// Whether it can be chosen at all.
    ///
    /// Dimmed and present rather than absent, so that the menu is the same
    /// shape every time it opens for the same unit. An entry that comes and
    /// goes with the state moves the entries under it, and a list whose rows
    /// move is a list you have to read before you can press anything.
    pub enabled: bool,
    /// Whether this is the one the unit is already on.
    ///
    /// What the menu opens with the cursor on, so that <kbd>Enter</kbd> twice
    /// runs what the row was already offering — which is what one press used
    /// to do, and is worth keeping under the hand that learnt it.
    pub current: bool,
}
