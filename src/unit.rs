//! Units: the named, restartable things a user manages.
//!
//! Where a [`RunnerHandle`](crate::runner::RunnerHandle) supervises one run, a
//! [`Unit`] is the identity that survives across runs. It owns the
//! [`Log`](crate::log::Log) — so the output history is not lost on restart —
//! and it holds whichever handle is current.
//!
//! A [`UnitDescription`] is the recipe: what to spawn, and how. Keeping it
//! separate from the unit is what allows a unit to be restarted under a
//! different description (the `modes` in the config sketch: build vs. watch).
//!
//! [`UnitMap`] is the layer above: every unit of a session, by name, shared
//! rather than owned, so that anything holding the map can start, stop and
//! ask after any of them. The `context` module — how a unit reaches back to
//! the map it belongs to, for the groups and `pre-condition` dependencies in
//! the config sketch — is still a placeholder.

mod description;
mod dispatch;
mod map;
mod unit;

#[allow(unused_imports)]
pub use description::UnitDescription;

#[allow(unused_imports)]
pub use dispatch::{UnitAction, UnitEvent};

#[allow(unused_imports)]
pub use map::UnitMap;

#[allow(unused_imports)]
pub use unit::Unit;
