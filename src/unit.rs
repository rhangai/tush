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
//! The collection above is [`AppUnitMap`](crate::app::AppUnitMap), which is
//! in [`app`](crate::app) rather than here: it holds every unit of a session
//! together with the interner that keys them and the graph that orders their
//! starts, and all three are answers to a config.

mod behavior;
mod dispatch;
mod unit;

#[allow(unused_imports)]
pub use behavior::{UnitBehavior, UnitType};

#[allow(unused_imports)]
pub use dispatch::{UnitAction, UnitChoice, UnitChoices, UnitEvent};

#[allow(unused_imports)]
pub use unit::{Unit, UnitHandle, UnitStart};
