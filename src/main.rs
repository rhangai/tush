//! `tush` — a process manager built around log tailing.
//!
//! The goal is to supervise a set of long lived commands (dev servers, build
//! watchers, setup scripts) while keeping the recent output of each one
//! available for display, like a `tail -f` that also knows how to start, stop
//! and restart what it is tailing.
//!
//! # Module layout
//!
//! - [`base`] — the low level pieces: child [`Process`](base::Process)
//!   handling and the [`ExitReason`](base::ExitReason) of a finished process.
//! - [`mod@log`] — capture of a process's output into the bounded
//!   [`Log`](log::Log) history.
//! - [`runner`] — the async supervision layer: the [`Runner`](runner::Runner)
//!   trait, its [`RunnerHandle`](runner::RunnerHandle) and the
//!   [`RunnerState`](runner::RunnerState) machine.
//! - [`mod@unit`] — the user facing concept: a [`Unit`] is a named,
//!   restartable entry described by a [`UnitDescription`].
//! - [`util`] — shared data structures, currently the recycling ring buffer
//!   the logs keep their chunks in.
//!
//! # Layering
//!
//! ```text
//! Unit  ──owns──>  RunnerHandle  ──drives──>  Runner (Process, ...)
//!   │                                              │
//!   └──owns──>  Log  <───────writes lines──────────┘
//! ```

#![allow(dead_code)]

mod base;
mod log;
mod runner;
mod unit;
mod util;

use crate::unit::{Unit, UnitDescription};

/// Temporary entrypoint used to exercise the runtime while the CLI does not
/// exist yet.
///
/// It starts a unit, replaces its running description twice and prints the
/// observed states so the restart handshake can be inspected by hand.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let unit = Unit::new(UnitDescription::program());
    let mut log = unit.log_reader();
    let h1 = unit.start()?;
    // println!("{:?}", h1.state());
    h1.wait().await;
    log.sync();
    for line in log.iter() {
        line.print();
    }
    Ok(())
}
