//! Low level building blocks shared by the rest of the crate.
//!
//! Nothing in here knows about units or supervision policy; these are the raw
//! primitives that the upper layers compose:
//!
//! - [`Process`] spawns an OS child in its own process group and pumps its
//!   stdout, line by line, into a log.
//! - [`Log`] is the multi-reader ring buffer holding the most recent output
//!   lines, handed out to writers as a [`LogWriterRef`] and to readers as a
//!   [`LogBuffer`] snapshot.
//! - [`ExitReason`] describes how a process finished.

mod exit_reason;
mod log;
mod process;

#[allow(unused_imports)]
pub use log::{Log, LogBuffer, LogWriterRef};

#[allow(unused_imports)]
pub use process::Process;

#[allow(unused_imports)]
pub use exit_reason::ExitReason;
