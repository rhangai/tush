//! Units: the named, restartable things a user manages.
//!
//! Where a [`RunnerHandle`](crate::runner::RunnerHandle) supervises one run, a
//! [`Unit`] is the identity that survives across runs. It owns the
//! [`Log`](crate::log::Log) — so the output history is not lost on restart —
//! and it holds whichever handle is current.
//!
//! A [`UnitBehavior`] is what the unit does: what to spawn, and how it
//! answers the events that reach it. Keeping it separate from the unit is
//! what lets a unit be restarted under a different behavior — the `modes` in
//! the config: build vs. watch.
//!
//! [`UnitMap`] is the layer above: every unit of a session under the key its
//! caller addresses it by, shared rather than owned, so anything holding the
//! map can start, stop and ask after any of them.

mod behavior;
mod dispatch;
mod map;
mod unit;

#[allow(unused_imports)]
pub use behavior::UnitBehavior;

#[allow(unused_imports)]
pub use dispatch::{UnitAction, UnitChoice, UnitEvent};

#[allow(unused_imports)]
pub use map::{UnitKey, UnitMap};

#[allow(unused_imports)]
pub use unit::{Unit, UnitHandle};
