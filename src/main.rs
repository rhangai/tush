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
mod ui;
mod unit;
mod util;

use std::sync::Arc;

use crate::{
    app::App,
    config::Config,
    ui::{Ui, UiApp},
};

/// Temporary entrypoint: the `tush ui` mode, with the config path still
/// hardcoded because there is no CLI to read one from yet.
///
/// This is the mode that owns what it runs — the session is built here, so
/// quitting the screen has to take the processes with it. The UI returning is
/// only the screen being given back; [`shutdown`](crate::unit::UnitMap::shutdown)
/// is what makes the children actually gone, and it is deliberately awaited
/// rather than left to `Drop`, which cannot.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_path("tmp/example.yaml")?;
    let app = Arc::new(App::new(&config)?);

    let result = Ui::run(UiApp::new(app.clone())).await;
    app.units().shutdown().await;
    result
}
