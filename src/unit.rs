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
//! The `pool` and `context` modules are placeholders for the next layer —
//! many units, their groups and their dependencies — and are not wired up yet.

mod context;
mod description;
mod pool;
mod unit;

#[allow(unused_imports)]
pub use unit::Unit;

#[allow(unused_imports)]
pub use description::UnitDescription;
