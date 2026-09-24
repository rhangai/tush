use smallvec::{IntoIter, SmallVec};

use crate::util::str::SmallStr;

/// Something asked of a unit, for its behavior to answer.
///
/// One per menu entry: they all come out of
/// [`choices`](crate::unit::UnitBehavior::choices), so there is no "do the
/// default" — the user picked a named thing off a list.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum UnitEvent {
    /// Run it in the mode it is already on, restarting it if it is up.
    Start,
    /// Move to the mode at this index, and run that.
    StartMode(usize),
    /// End the current run.
    Stop,
}

/// What a dispatch decided the unit should do.
///
/// Smaller than the [`UnitEvent`] that prompted it, and deliberately: a
/// [`StartMode`](UnitEvent::StartMode) has already moved the behavior onto
/// that mode by the time this comes back, so all that is left to do is run it
/// or stop it. `None` is the third answer and a common one — a behavior with
/// nothing to say to the event it was handed.
#[derive(Clone, Copy, Debug)]
pub enum UnitAction {
    /// Start the unit, on whatever mode its behavior is now on.
    Start,
    /// Stop the current run.
    Stop,
}

/// One entry in the list of things a unit can be asked right now.
///
/// Not an "action": that name is what a dispatch *returns*.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct UnitChoice {
    /// `Start`, `Restart` or `Stop` — a word of its own and not a built
    /// label, so the menu writes this and [`mode`](UnitChoice::mode) as two
    /// runs of text instead of allocating `"Restart Build"` per item.
    pub verb: SmallStr,
    /// What the verb applies to, or `None` for a proc that runs one way.
    pub mode: Option<SmallStr>,
    /// That mode's short name, so a caller matching text a person typed takes
    /// what the screen shows them — `W` as readily as `Watch`. `None` where
    /// the config declared no short name.
    pub mode_short: Option<SmallStr>,
    /// What to send if it is chosen.
    pub event: UnitEvent,
    /// Whether it can be chosen. A disabled entry is drawn dim rather than
    /// left out, so the menu keeps its shape and `Stop` stays where it was.
    pub enabled: bool,
    /// The one the unit is already on: where the menu opens its cursor.
    pub current: bool,
}

/// The entries of one menu, as one list handed about by value.
///
/// A [`SmallVec`] and not a `Vec`: the list is lent to a client and taken
/// back on every open and every poll, and a proc's modes plus the `Stop`
/// after them fit inside it without an allocation.
///
/// Transparent on the wire, so what crosses is the bare array of entries and
/// the type costs the protocol nothing.
#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct UnitChoices {
    choices: SmallVec<[UnitChoice; 4]>,
}

impl UnitChoices {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.choices.clear();
    }

    pub fn push(&mut self, choice: UnitChoice) {
        self.choices.push(choice);
    }

    pub fn iter(&self) -> impl Iterator<Item = &UnitChoice> {
        self.choices.iter()
    }

    pub fn len(&self) -> usize {
        self.choices.len()
    }

    pub fn at(&self, index: usize) -> &UnitChoice {
        &self.choices[index]
    }

    pub fn get(&self, index: usize) -> Option<&UnitChoice> {
        self.choices.get(index)
    }
}

impl IntoIterator for UnitChoices {
    type Item = UnitChoice;
    type IntoIter = IntoIter<[UnitChoice; 4]>;
    fn into_iter(self) -> Self::IntoIter {
        self.choices.into_iter()
    }
}

impl Extend<UnitChoice> for UnitChoices {
    fn extend<T: IntoIterator<Item = UnitChoice>>(&mut self, iter: T) {
        self.choices.extend(iter)
    }
}
