//! Low level building blocks shared by the rest of the crate.
//!
//! Nothing in here knows about units or supervision policy; these are the raw
//! primitives that the upper layers compose:
//!
//! - [`Process`] spawns an OS child in its own process group and pumps its
//!   stdout, line by line, into a log.
//! - [`ExitReason`] describes how a process finished.
//!
//! The output history itself lives in [`log`](crate::log): a `Process` is
//! given a [`LogWriterRef`](crate::log::LogWriterRef) and pumps its stdout
//! into whichever [`Log`](crate::log::Log) that handle points at.

mod exit_reason;
mod process;

#[allow(unused_imports)]
pub use process::Process;

#[allow(unused_imports)]
pub use exit_reason::ExitReason;
