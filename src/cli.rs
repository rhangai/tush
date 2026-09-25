//! The command line, parsed.
//!
//! Five ways in, differing in where the session is and where the screen is:
//!
//! ```text
//! tush run       --config x.yaml  [targets]   both here; quitting takes it down
//! tush serve     --config x.yaml  [targets]   the session, for something else to attach to
//! tush attach                                 the screen, over a socket
//! tush dispatch  start|stop KEY               neither: one command, over the socket
//! tush status                                 neither: one listing, over the socket
//! ```

use std::{path::PathBuf, str::FromStr, time::Duration};

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::app::Target;

/// How often the screen redraws when nothing is being pressed.
///
/// A run changes state without anybody asking and a client cannot push, so
/// the only way to find out is to look. Ten a second is under what reads as
/// lag and, a frame allocating nothing, costs about what the polling does.
const DEFAULT_FPS: f64 = 10.0;

/// The most and least often a screen may redraw. Not taste: below the floor
/// a `Duration` from a division rounds to nothing, and above the ceiling a
/// redraw is slower than what it redraws for.
const FPS_RANGE: std::ops::RangeInclusive<f64> = 0.01..=1000.0;

/// Where both ends read the socket path from when no flag says.
///
/// One variable for serving and attaching, so a shell that exports it once
/// has both halves pointing at the same session. The flag wins over it, which
/// is clap's own precedence and the one a person expects.
const SOCKET_ENV: &str = "TUSH_SOCKET";

/// `tush`, as the command line spells it.
#[derive(Parser, Debug)]
#[command(
    name = "tush",
    version,
    about = "A process manager built around log tailing"
)]
pub struct Cli {
    /// Which of the five ways in was asked for.
    #[command(subcommand)]
    pub command: Command,
}

/// The five ways in, differing in where the session and the screen are.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run a session and show it, in one process.
    ///
    /// Quitting the screen takes the processes with it, because this is the
    /// mode that owns them.
    Run(RunArgs),
    /// Run a session with no screen, for something else to attach to.
    Serve(ServeArgs),
    /// Show a session that is already running.
    Attach(AttachArgs),
    /// Tell a session that is already running to do one thing.
    Dispatch(DispatchArgs),
    /// Print where every proc of a session that is already running got to.
    Status(StatusArgs),
}

/// Session and screen both, in this process.
#[derive(Args, Debug)]
pub struct RunArgs {
    #[command(flatten)]
    pub session: SessionArgs,
    #[command(flatten)]
    pub screen: ScreenArgs,
    /// Print the output line by line instead of drawing a screen.
    ///
    /// For a terminal that is not one — a CI log, a pipe, an editor's output
    /// pane — where a screen that redraws itself is noise.
    #[arg(long)]
    pub no_tui: bool,
}

/// A session with no screen.
#[derive(Args, Debug)]
pub struct ServeArgs {
    #[command(flatten)]
    pub session: SessionArgs,
    /// Where to listen, as a path to a Unix socket.
    ///
    /// Not given is one fixed path under the runtime directory, which two
    /// sessions on one machine would both want — the second is refused, and
    /// this is how it gets one of its own.
    #[arg(long, env = SOCKET_ENV, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

/// A screen with no session of its own, drawn over a socket.
#[derive(Args, Debug)]
pub struct AttachArgs {
    #[command(flatten)]
    pub screen: ScreenArgs,
    /// Which session to show, as the path to its Unix socket.
    ///
    /// The same default as the serving side, which is what one fixed name
    /// buys: a session started with nothing said is attached to with nothing
    /// said. Only a session that had to be moved off that path needs this.
    #[arg(long, env = SOCKET_ENV, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

/// One command for a session somewhere else.
///
/// No `--config`: the session is the one that read the file, so a key is
/// checked over there and an unknown one comes back named rather than being
/// guessed at here.
#[derive(Args, Debug)]
pub struct DispatchArgs {
    /// Which session to tell, as the path to its Unix socket.
    ///
    /// The same default and the same variable as `serve` and `attach`, so a
    /// shell that set one up reaches it with nothing said.
    #[arg(long, env = SOCKET_ENV, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    #[command(subcommand)]
    pub command: DispatchCommand,
}

/// What to ask of one proc.
#[derive(Subcommand, Debug)]
pub enum DispatchCommand {
    /// Start a proc, restarting it if it is already up.
    Start {
        /// The key the proc is declared under, which is what the config wrote
        /// and not the display name two procs may share.
        #[arg(value_name = "KEY")]
        key: String,
        /// Which mode to run, as its name or its position in the list.
        ///
        /// Nothing given runs whichever mode the proc is already on, which is
        /// what pressing `r` on the screen does.
        #[arg(value_name = "MODE")]
        mode: Option<String>,
    },
    /// Stop a proc, without waiting for it to be gone.
    Stop {
        /// The key the proc is declared under.
        #[arg(value_name = "KEY")]
        key: String,
    },
}

/// Every proc of a session somewhere else, as a listing.
///
/// No `--config` and no target, for the same reason `dispatch` has none: the
/// session is the one that read the file, and what it is running is what it
/// answers with.
#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Which session to ask, as the path to its Unix socket.
    ///
    /// The same default and the same variable as `serve`, `attach` and
    /// `dispatch`.
    #[arg(long, env = SOCKET_ENV, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

/// What makes a session: the file it is declared in, and what to start.
#[derive(Args, Debug)]
pub struct SessionArgs {
    /// The config file the procs are declared in.
    #[arg(long, short, value_name = "PATH")]
    pub config: String,
    /// What to start: a proc by name, or `group:name` for every proc in a
    /// group.
    ///
    /// Nothing named starts nothing, which is a session sitting there waiting
    /// to be told.
    #[arg(value_name = "TARGET")]
    pub targets: Vec<Target>,
}

/// How often the screen redraws.
///
/// Two spellings of one number, and neither is the real one: `--fps 60` reads
/// better for a screen that has to keep up, `--refresh-rate 5s` for one
/// nobody is watching.
#[derive(Args, Debug)]
pub struct ScreenArgs {
    /// Frames per second.
    #[arg(long, value_name = "N", conflicts_with = "refresh_rate")]
    pub fps: Option<f64>,
    /// Time between frames, as `100ms`, `2s` or `1500ms`.
    #[arg(long, value_name = "DURATION")]
    pub refresh_rate: Option<Period>,
}

impl ScreenArgs {
    /// How long to wait between frames.
    ///
    /// `--refresh-rate` is taken as it stands; `--fps` is inverted. They
    /// cannot both be given — clap refuses that — so the order here settles
    /// nothing and is only what it is.
    pub fn refresh(&self) -> Result<Duration> {
        if let Some(period) = self.refresh_rate {
            return Ok(period.0);
        }
        let fps = self.fps.unwrap_or(DEFAULT_FPS);
        if !FPS_RANGE.contains(&fps) {
            bail!(
                "--fps must be between {} and {}, not {fps}",
                FPS_RANGE.start(),
                FPS_RANGE.end()
            );
        }
        Ok(Duration::from_secs_f64(1.0 / fps))
    }
}

/// A length of time, written the way a person writes one.
///
/// A newtype so the parsing lives with the flag and the error says what the
/// flag accepts. Only `ms` and `s`: a screen redraws in one or the other.
#[derive(Clone, Copy, Debug)]
pub struct Period(pub Duration);

impl FromStr for Period {
    type Err = anyhow::Error;

    /// `100ms` or `2s`. A number with no unit is refused rather than guessed
    /// at, since the two readings differ by a thousand.
    fn from_str(period: &str) -> Result<Self> {
        let period = period.trim();
        let (number, millis) = match period.strip_suffix("ms") {
            Some(number) => (number, true),
            None => match period.strip_suffix('s') {
                Some(number) => (number, false),
                None => bail!("`{period}` has no unit; write `100ms` or `2s`"),
            },
        };
        let value: f64 = number
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("`{number}` is not a number"))?;
        if !(value.is_finite() && value > 0.0) {
            bail!("a period has to be more than nothing, and `{period}` is not");
        }
        Ok(Self(Duration::from_secs_f64(match millis {
            true => value / 1000.0,
            false => value,
        })))
    }
}
