use arcstr::ArcStr;

/// Something asked of a unit, for its behavior to answer.
///
/// One per menu entry: they all come out of
/// [`choices`](crate::unit::UnitBehavior::choices), so there is no "do the
/// default" — the user picked a named thing off a list.
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
/// Not an "action": that name is what a dispatch *returns*.
#[derive(Clone, Debug)]
pub struct UnitChoice {
    /// `Start`, `Restart` or `Stop` — a literal and not a built label, so
    /// the menu writes this and [`mode`](UnitChoice::mode) as two runs of
    /// text instead of allocating `"Restart Build"` per item.
    pub verb: &'static str,
    /// What the verb applies to, or `None` for a proc that runs one way.
    pub mode: Option<ArcStr>,
    /// What to send if it is chosen.
    pub event: UnitEvent,
    /// Whether it can be chosen. A disabled entry is drawn dim rather than
    /// left out, so the menu keeps its shape and `Stop` stays where it was.
    pub enabled: bool,
    /// The one the unit is already on: where the menu opens its cursor.
    pub current: bool,
}
