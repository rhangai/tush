//! `tush` — a process manager built around log tailing.
//!
//! The goal is to supervise a set of long lived commands (dev servers, build
//! watchers, setup scripts) while keeping the recent output of each one
//! available for display, like a `tail -f` that also knows how to start, stop
//! and restart what it is tailing.
//!
//! # Module layout
//!
//! - [`mod@app`] — a [`config::Config`] that has been checked, and
//!   the session built from it.
//! - [`base`] — the low level pieces: child [`Process`](base::Process)
//!   handling and the [`ExitReason`](base::ExitReason) of a finished process.
//! - [`mod@config`] — the config file parsed into the [`config::Config`]
//!   the units are declared from.
//! - [`mod@log`] — capture of a process's output into the bounded
//!   [`Log`](log::Log) history.
//! - [`runner`] — the async supervision layer: the [`Runner`](runner::Runner)
//!   trait, its [`RunnerHandle`](runner::RunnerHandle) and the
//!   [`RunnerState`](runner::RunnerState) machine.
//! - [`mod@unit`] — the user facing concept: a [`Unit`](unit::Unit) is a
//!   named, restartable entry whose [`UnitBehavior`](unit::UnitBehavior)
//!   decides what it does.
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
mod cli;
mod config;
mod log;
mod runner;
mod ui;
mod unit;
mod util;

use std::sync::Arc;

use anyhow::{Result, bail};
use clap::Parser;

use crate::{
    app::App,
    cli::{Cli, Command, RunArgs},
    config::Config,
    ui::{Ui, UiApp},
};

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Run(args) => run(args).await,
        Command::Serve(_) => bail!("`tush serve` is not built yet"),
        Command::Attach(_) => bail!("`tush attach` is not built yet"),
    }
}

/// Build a session, start what was asked for, and show it.
///
/// This is the mode that owns what it runs, so quitting the screen has to
/// take the processes with it. The UI returning is only the screen being
/// given back; [`shutdown`](crate::unit::UnitMap::shutdown) is what makes the
/// children actually gone, and it is deliberately awaited rather than left to
/// `Drop`, which cannot.
///
/// The starting happens before the screen does, so that a target that names
/// nothing is a message on a terminal that still works rather than one behind
/// a screen that is being torn down.
async fn run(args: RunArgs) -> Result<()> {
    if args.no_tui {
        bail!("`--no-tui` is not built yet");
    }
    let refresh = args.screen.refresh()?;

    let config = Config::from_path(&args.session.config)?;
    let app = Arc::new(App::new(&config)?);
    let targets = app.resolve(&args.session.targets)?;

    for key in &targets {
        app.units().start(key)?;
    }

    let result = Ui::run(UiApp::new(app.clone()), refresh).await;
    app.units().shutdown().await;
    result
}
