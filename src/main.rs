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
//! - [`mod@cli`] — the command line, as `clap` reads it.
//! - [`mod@config`] — the config file parsed into the [`config::Config`]
//!   the units are declared from.
//! - [`mod@error`] — every error type in the crate.
//! - [`mod@log`] — capture of a process's output into the bounded
//!   [`Log`](log::Log) history.
//! - [`runner`] — the async supervision layer: the [`Runner`](runner::Runner)
//!   trait, its [`RunnerHandle`](runner::RunnerHandle) and the
//!   [`RunnerState`](runner::RunnerState) machine.
//! - [`mod@ui`] — the terminal screen, and the [`UiClient`](ui::UiClient) it
//!   reads a session through.
//! - [`mod@unit`] — the user facing concept: a [`Unit`](unit::Unit) is a
//!   named, restartable entry whose [`UnitBehavior`](unit::UnitBehavior)
//!   decides what it does.
//! - [`util`] — shared data structures with nothing to do with processes: the
//!   [`SmallStr`](util::str::SmallStr) every name is held as, the recycling
//!   ring the logs keep their chunks in, the dependency graph the config is
//!   checked with.
//!
//! # Layering
//!
//! ```text
//! Unit  ──owns──>  RunnerHandle  ──drives──>  Runner (Process, ...)
//!   │                                              │
//!   └──owns──>  Log  <───────writes lines──────────┘
//! ```

#![allow(dead_code)]
// A hard error and not a warning: the point of `SmallStr` is that nothing
// else names what is inside it, and a warning is something you walk past.
#![deny(clippy::disallowed_types)]

mod app;
mod base;
mod cli;
mod config;
mod error;
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
    ui::{Ui, UiApp, UiTheme},
};

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Run(args) => run(args).await,
        Command::Serve(_) => bail!("`tush serve` is not built yet"),
        Command::Attach(_) => bail!("`tush attach` is not built yet"),
    }
}

/// Build a session and show it.
///
/// This is the mode that owns what it runs, so quitting the screen has to
/// take the processes with it. The UI returning is only the screen being
/// given back; [`shutdown`](crate::unit::UnitMap::shutdown) is what makes the
/// children actually gone, and it is deliberately awaited rather than left to
/// `Drop`, which cannot.
///
/// Nothing is started here: the session comes up with every proc stopped,
/// and the screen is what runs them.
async fn run(args: RunArgs) -> Result<()> {
    if args.no_tui {
        bail!("`--no-tui` is not built yet");
    }
    let refresh = args.screen.refresh()?;

    let config = Config::from_path(&args.session.config)?;
    let app = Arc::new(App::new(&config)?);

    let result = Ui::run(UiApp::new(app.clone()), refresh, UiTheme::default()).await;
    app.unit_map().shutdown().await;
    Ok(result?)
}
