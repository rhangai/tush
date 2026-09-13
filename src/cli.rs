//! The command line, parsed.
//!
//! Three ways to run, differing in where the session is and where the screen
//! is:
//!
//! ```text
//! tush run     --config x.yaml  [targets]   both here; quitting takes it down
//! tush serve   --config x.yaml  [targets]   the session, for something else to attach to
//! tush attach                               the screen, over a socket
//! ```
//!
//! Only `run` does anything yet. `serve` and `attach` parse, so that the
//! shape of the thing is settled and the flags they will need are already
//! spelled the way they will be spelled — and then say they are not built.

use std::{str::FromStr, time::Duration};

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::app::Target;

/// How often the screen is redrawn when nothing is being pressed, by default.
///
/// A run changes state without anybody asking — a process exits, a line is
/// half written — and with a client that cannot push, the only way to find
/// out is to look. Ten a second is under what a person reads as lag and, now
/// that a frame allocates nothing, costs about what the polling does.
const DEFAULT_FPS: f64 = 10.0;

/// The most and least often a screen may be redrawn.
///
/// Not a matter of taste: below the floor a `Duration` from a division starts
/// rounding to nothing, and above the ceiling a redraw is slower than the
/// thing it is redrawing for.
const FPS_RANGE: std::ops::RangeInclusive<f64> = 0.01..=1000.0;

#[derive(Parser, Debug)]
#[command(
    name = "tush",
    version,
    about = "A process manager built around log tailing"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

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
}

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

#[derive(Args, Debug)]
pub struct ServeArgs {
    #[command(flatten)]
    pub session: SessionArgs,
}

#[derive(Args, Debug)]
pub struct AttachArgs {
    #[command(flatten)]
    pub screen: ScreenArgs,
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
/// Two ways of saying one number, and they are the same number read from
/// either end: a rate of frames, or the time between them. Which one reads
/// better depends on what you are thinking about — `--fps 60` for a screen
/// that has to keep up, `--refresh-rate 5s` for one nobody is watching — so
/// both are here and neither is the real one.
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
/// A newtype so that the parsing lives with the flag it belongs to and the
/// error says what the flag accepts. Only the two units anybody would reach
/// for here: a screen redraws in milliseconds or in seconds, and anything
/// slower than that is a screen nobody is looking at.
#[derive(Clone, Copy, Debug)]
pub struct Period(pub Duration);

impl FromStr for Period {
    type Err = anyhow::Error;

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
