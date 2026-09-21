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
//! - [`mod@server`] — a session with no screen, listening for something to
//!   attach to it.
//! - [`runner`] — the async supervision layer: the [`Runner`](runner::Runner)
//!   trait, its [`RunnerHandle`](runner::RunnerHandle) and the
//!   [`RunnerState`](runner::RunnerState) machine.
//! - [`mod@ui`] — the terminal screen.
//! - [`mod@unit`] — the user facing concept: a [`Unit`](unit::Unit) is a
//!   named, restartable entry whose [`UnitBehavior`](unit::UnitBehavior)
//!   decides what it does.
//! - [`util`] — shared data structures with nothing to do with processes: the
//!   [`SmallStr`](util::str::SmallStr) every name is held as, the recycling
//!   ring the logs keep their chunks in, the dependency graph the config is
//!   checked with.
//! - [`mod@view`] — a session as something outside it sees and drives it: the
//!   [`ViewClient`](view::ViewClient) the screen and, later, a socket read it
//!   through.
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
mod server;
mod ui;
mod unit;
mod util;
mod view;

use std::sync::Arc;

use anyhow::{Result, bail};
use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::{
    app::App,
    cli::{Cli, Command, RunArgs, ServeArgs},
    config::Config,
    server::{Server, ServerPrinter, default_socket_path},
    ui::{Ui, UiTheme},
    view::ViewApp,
};

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Run(args) => run(args).await,
        Command::Serve(args) => serve(args).await,
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
/// What comes up started is what the command line named, and nothing else;
/// the screen is how the rest are run. Scheduling them before the UI is safe
/// because [`App::run_tasks`] has already spawned it, and a request made
/// before it is polled is one it still wakes for.
async fn run(args: RunArgs) -> Result<()> {
    if args.no_tui {
        bail!("`--no-tui` is not built yet");
    }
    let refresh = args.screen.refresh()?;

    let config = Config::from_path(&args.session.config)?;
    let app = Arc::new(App::new(&config)?);
    app.run_tasks();

    schedule_targets(&app, args.session.targets);

    let result = Ui::run(ViewApp::new(app.clone()), refresh, UiTheme::default()).await;
    app.shutdown().await;
    Ok(result?)
}

/// Build a session, print it, and let something else attach to it.
///
/// The screen's job is split three ways here: the socket is where a client
/// reads the session, stdout is where a person does, and a signal is what
/// takes it down. Nothing a client does ends the session — that is the whole
/// difference from [`run`], where quitting the screen is quitting.
///
/// The order at the end is the part that matters. The socket closes first so
/// nothing new attaches to a session that is going away; then the procs are
/// stopped and waited for, which is what writes the last note into each log;
/// and only then is the printer told to stop, so those notes are printed
/// rather than being the thing that was still in flight.
async fn serve(args: ServeArgs) -> Result<()> {
    let config = Config::from_path(&args.session.config)?;
    let app = Arc::new(App::new(&config)?);
    app.run_tasks();

    let path = match args.socket {
        Some(path) => path,
        None => default_socket_path(&args.session.config),
    };
    let server = Server::bind(app.clone(), path)?;
    println!("listening on {}", server.path().display());

    let printing = CancellationToken::new();
    let printer = tokio::spawn(ServerPrinter::new(&app).run(printing.clone()));

    schedule_targets(&app, args.session.targets);

    let result = server.run(cancel_on_interrupt()?).await;

    app.shutdown().await;
    printing.cancel();
    let _ = printer.await;
    Ok(result?)
}

/// Start what the command line named, and nothing else.
///
/// A target naming no unit is not a failure: it matched nothing, the same
/// answer an unknown group gets from
/// [`schedule_group`](App::schedule_group).
fn schedule_targets(app: &App, targets: Vec<app::Target>) {
    for target in targets {
        match target {
            app::Target::Unit(unit) => {
                if let Some(key) = app.unit_map().key(unit.as_ref()) {
                    app.schedule(key);
                };
            }
            app::Target::Group(group) => {
                app.schedule_group(group.as_ref());
            }
        }
    }
}

/// A token the signal that means "stop" cancels.
///
/// Both of them: `SIGINT` is the terminal a server was started in, `SIGTERM`
/// is everything else — a supervisor, a container stopping, `kill`. Ignoring
/// the second would leave the children for the system to kill rather than
/// shut down, which is the case the orderly path exists for.
///
/// The handlers are installed here, so failing to install one is a startup
/// failure and not a server that quietly cannot be stopped. The task that
/// waits on them is detached because there is nothing to join it to: it ends
/// at the first signal, and outlives what it cancels only when that failed
/// first — with the process already on its way out.
fn cancel_on_interrupt() -> Result<CancellationToken> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        cancel.cancel();
    });
    Ok(token)
}
