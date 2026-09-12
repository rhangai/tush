//! Supervision of a single runnable thing.
//!
//! A [`Runner`] is anything that can be run to completion and asked to shut
//! down — today a [`Process`](crate::base::Process), tomorrow whatever else
//! needs the same lifecycle. A [`RunnerHandle`] wraps one runner in a Tokio
//! task and exposes the only three verbs the rest of the crate needs: start,
//! abort, wait. [`RunnerState`] is the state machine both sides agree on, kept
//! in a lock free [`RunnerStateAtomic`] so it can be polled from anywhere.
//!
//! A handle supervises exactly one run. Restarting is not a operation on a
//! handle — it is a new handle, which is what [`Unit`](crate::unit::Unit) does.
//!
//! Runners compose: [`RunnerSerial`] is itself a `Runner` made of several,
//! run one after the other, so a unit whose config lists more than one
//! command is still one handle, one log and one state.

mod handle;
mod runner;
mod serial;
mod state;

#[allow(unused_imports)]
pub use runner::Runner;

#[allow(unused_imports)]
pub use handle::RunnerHandle;

#[allow(unused_imports)]
pub use serial::RunnerSerial;

#[allow(unused_imports)]
pub use state::{RunnerState, RunnerStateAtomic};
