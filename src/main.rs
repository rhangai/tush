//! `tush` — a process manager built around log tailing.
//!
//! The goal is to supervise a set of long lived commands (dev servers, build
//! watchers, setup scripts) while keeping the recent output of each one
//! available for display, like a `tail -f` that also knows how to start, stop
//! and restart what it is tailing.
//!
//! # Module layout
//!
//! - [`mod@app`] — a [`Config`](config::Config) that has been checked, and
//!   the session built from it.
//! - [`base`] — the low level pieces: child [`Process`](base::Process)
//!   handling and the [`ExitReason`](base::ExitReason) of a finished process.
//! - [`mod@config`] — the config file parsed into the [`Config`](config::Config)
//!   the units are declared from.
//! - [`mod@log`] — capture of a process's output into the bounded
//!   [`Log`](log::Log) history.
//! - [`runner`] — the async supervision layer: the [`Runner`](runner::Runner)
//!   trait, its [`RunnerHandle`](runner::RunnerHandle) and the
//!   [`RunnerState`](runner::RunnerState) machine.
//! - [`mod@unit`] — the user facing concept: a [`Unit`] is a named,
//!   restartable entry whose [`UnitBehavior`] decides what it does.
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

mod app;
mod base;
mod config;
mod log;
mod runner;
mod unit;
mod util;

use anyhow::anyhow;

use crate::{
    app::App,
    config::Config,
    unit::{Unit, UnitBehavior, UnitMap},
};

/// Temporary entrypoint used to exercise the runtime while the CLI does not
/// exist yet.
///
/// It starts a unit, replaces its running behavior twice and prints the
/// observed states so the restart handshake can be inspected by hand.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_path("tmp/example.yaml")?;
    let app = App::new(config)?;
    let mut log = app
        .units()
        .log_reader("server-setup")
        .ok_or(anyhow!("Invalid"))?;
    let h = app.units().start("server-setup")?;
    h.wait().await;
    for line in log.iter_sync() {
        line.print();
    }
    println!("{:?}", app.units().state("server-setup"));
    // let map = UnitMap::new();
    // let mut log = map.add("key", UnitBehavior::program()).unwrap();
    // let h1 = map.start("key").unwrap();
    // // println!("{:?}", h1.state());
    // h1.wait().await;
    // for line in log.iter_sync() {
    //     line.print();
    // }
    Ok(())
}
