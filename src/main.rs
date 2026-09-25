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
//!   [`ViewClient`] the screen, a socket and a one-shot command read it
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

use anyhow::Result;
use clap::Parser;
use tokio::{
    signal::unix::{SignalKind, signal},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    app::App,
    cli::{
        AttachArgs, Cli, Command, DispatchArgs, DispatchCommand, RunArgs, ServeArgs, StatusArgs,
    },
    config::Config,
    server::{Server, default_socket_path},
    ui::{Ui, UiTheme},
    view::{ViewApp, ViewClient, ViewDispatch, ViewPrinter, ViewSocket, ViewStatus},
};

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Run(args) => run(args).await,
        Command::Serve(args) => serve(args).await,
        Command::Attach(args) => attach(args).await,
        Command::Dispatch(args) => dispatch(args).await,
        Command::Status(args) => status(args).await,
    }
}

/// Build a session and show it.
///
/// This is the mode that owns what it runs, so what gives the session back
/// has to take the processes with it — quitting the screen, or the signal,
/// which ends the screen by the same door rather than killing us through it.
/// [`shutdown`](crate::app::AppUnitMap::shutdown) is what makes the children
/// actually gone, and it is deliberately awaited rather than left to `Drop`,
/// which cannot.
///
/// What comes up started is what the command line named, and nothing else;
/// the screen is how the rest are run. Scheduling them before the UI is safe
/// because [`App::run_tasks`] has already spawned it, and a request made
/// before it is polled is one it still wakes for.
///
/// `--fps` is read only when there is a screen to use it, and before
/// anything is built, so a number the screen cannot take is a failure while
/// there are still no procs to stop again.
async fn run(args: RunArgs) -> Result<()> {
    let config = Config::from_path(&args.session.config)?;
    let app = Arc::new(App::new(&config)?);
    app.run_tasks();

    schedule_targets(&app, args.session.targets);

    if args.no_tui {
        let printing = CancellationToken::new();
        let printer = tokio::spawn(ViewPrinter::new(&app).run(printing.clone()));
        cancel_on_interrupt()?.cancelled().await;
        shutdown_printing(&app, printing, printer).await;
        return Ok(());
    }

    let refresh = args.screen.refresh()?;
    let client = ViewApp::new(app.clone());
    let theme = theme_for(&client);
    let result = Ui::run(client, refresh, theme, cancel_on_interrupt()?).await;
    app.shutdown().await;
    Ok(result?)
}

/// Stop the session, then stop printing it.
///
/// That order is the whole of it: [`shutdown`](App::shutdown) is what writes
/// the last note into each log — the line saying the process exited — so a
/// printer stopped before it prints everything except the lines a person is
/// waiting for. Awaited rather than dropped, because the drain that puts
/// those lines out is the one [`ViewPrinter::run`] does after it is
/// cancelled.
async fn shutdown_printing(app: &App, printing: CancellationToken, printer: JoinHandle<()>) {
    app.shutdown().await;
    printing.cancel();
    let _ = printer.await;
}

/// Show a session that is running somewhere else.
///
/// The screen is the whole of this process: there are no procs to stop, so
/// quitting takes nothing down and the session carries on without it — the
/// opposite of [`run`], and the reason the two are separate commands.
///
/// A signal is still worth catching with nothing to stop: it is the terminal
/// that has to come back, and only the screen's own exit gives it back.
///
/// The poll rate is the refresh rate. They are free to differ — the task
/// fetches on its own clock and the screen draws on its — and one number is
/// what a person asked for until there is a reason for two.
async fn attach(args: AttachArgs) -> Result<()> {
    let refresh = args.screen.refresh()?;
    let socket = args.socket.unwrap_or_else(default_socket_path);
    let client = ViewSocket::connect(socket, refresh).await?;
    let theme = theme_for(&client);
    Ok(Ui::run(client, refresh, theme, cancel_on_interrupt()?).await?)
}

/// Say one thing to a session that is already running, and stop.
///
/// The only mode with neither procs nor a screen, so there is nothing to take
/// down and nothing to restore — which is why it is the one that waits on the
/// round trip and reports what came back. A command nobody could tell had
/// failed is worse than a slow one.
async fn dispatch(args: DispatchArgs) -> Result<()> {
    let socket = args.socket.unwrap_or_else(default_socket_path);
    let mut client = ViewDispatch::connect(socket).await?;
    match args.command {
        DispatchCommand::Start { key, mode } => client.start(&key, mode.as_deref()).await?,
        DispatchCommand::Stop { key } => client.stop(&key).await?,
    }
    Ok(())
}

/// Print what a session that is already running is running, and stop.
///
/// Like [`dispatch`] and for the same reason: with neither procs nor a screen
/// there is nothing to take down, so this is free to wait on the round trip
/// and to fail when there was nobody on the other end of it — a listing that
/// came back empty because nothing answered is the one wrong answer here.
async fn status(args: StatusArgs) -> Result<()> {
    let socket = args.socket.unwrap_or_else(default_socket_path);
    let mut client = ViewStatus::connect(socket).await?;
    client.print().await?;
    Ok(())
}

/// The screen a session asked for.
///
/// Both ways in go through the client rather than through the config, so an
/// `attach` — which never reads a file — is told the same things a `run`
/// reads for itself.
fn theme_for(client: &impl ViewClient) -> UiTheme {
    UiTheme {
        log_colors: client.settings().colors,
        ..UiTheme::default()
    }
}

/// Build a session, print it, and let something else attach to it.
///
/// The screen's job is split three ways here: the socket is where a client
/// reads the session, stdout is where a person does, and a signal is what
/// takes it down. Nothing a client does ends the session — that is the whole
/// difference from [`run`], where quitting the screen is quitting.
///
/// The socket closes first, so nothing new attaches to a session that is
/// going away; [`shutdown_printing`] is the rest of the order.
async fn serve(args: ServeArgs) -> Result<()> {
    let config = Config::from_path(&args.session.config)?;
    let app = Arc::new(App::new(&config)?);
    app.run_tasks();

    let path = match args.socket {
        Some(path) => path,
        None => default_socket_path(),
    };
    let server = Server::bind(app.clone(), path)?;
    println!("listening on {}", server.path().display());

    let printing = CancellationToken::new();
    let printer = tokio::spawn(ViewPrinter::new(&app).run(printing.clone()));

    schedule_targets(&app, args.session.targets);

    let result = server.run(cancel_on_interrupt()?).await;

    shutdown_printing(&app, printing, printer).await;
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
/// All three of them: `SIGINT` is the terminal a server was started in,
/// `SIGTERM` is everything else — a supervisor, a container stopping, `kill`
/// — and `SIGHUP` is that terminal going away while the session is still in
/// it. Ignoring any of them would leave the children for the system to kill
/// rather than shut down, which is the case the orderly path exists for.
///
/// The handlers are installed here, so failing to install one is a startup
/// failure and not a server that quietly cannot be stopped. The task that
/// waits on them is detached because there is nothing to join it to: it ends
/// at the first signal, and outlives what it cancels only when that failed
/// first — with the process already on its way out.
fn cancel_on_interrupt() -> Result<CancellationToken> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
            _ = hangup.recv() => {}
        }
        cancel.cancel();
    });
    Ok(token)
}
