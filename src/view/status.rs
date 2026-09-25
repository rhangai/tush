use std::{fmt::Write as _, io::Write as _, path::PathBuf};

use crate::{error::ViewSocketError, runner::RunnerState, view::server::ServerClient};

/// Where every unit of a session in another process got to, printed once.
///
/// **This one waits and it fails**, for the same reason
/// [`ViewDispatch`](crate::view::ViewDispatch) does: a shell has no frame to
/// fall back on, and a list that came out empty because nothing answered
/// reads exactly like a session with nothing in it.
pub struct ViewStatus {
    client: ServerClient,
}

impl ViewStatus {
    /// Open a connection to the session listening on `path`.
    pub async fn connect(path: PathBuf) -> Result<Self, ViewSocketError> {
        Ok(Self {
            client: ServerClient::connect(&path).await?,
        })
    }

    /// One line per unit: the key, where its run is, and the mode it is on if
    /// it has one.
    ///
    /// The key and not the display name, because the key is what `tush
    /// dispatch` takes next and two procs may show the same name.
    ///
    /// A write that fails is dropped, as in
    /// [`ViewPrinter`](crate::view::ViewPrinter): a closed stdout is `tush
    /// status | head`, and not something the command failed at.
    pub async fn print(&mut self) -> Result<(), ViewSocketError> {
        let units = self.client.units().await?;
        let mut status = String::new();

        // Both columns measured before anything goes out, since a width is
        // the widest row and the widest row may be the last one.
        let keys = units
            .iter()
            .map(|unit| unit.key.chars().count())
            .max()
            .unwrap_or(0);
        let mut states = 0;
        for unit in &units {
            status.clear();
            write_status(&mut status, unit.state);
            states = states.max(status.chars().count());
        }

        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for unit in &units {
            status.clear();
            write_status(&mut status, unit.state);
            let _ = match &unit.mode {
                Some(mode) => {
                    writeln!(out, "{:keys$}  {status:states$}  {mode}", unit.key.as_str())
                }
                None => writeln!(out, "{:keys$}  {status}", unit.key.as_str()),
            };
        }
        let _ = out.flush();
        Ok(())
    }
}

/// Where a run got to, in a word — with the code beside it for the two
/// endings that carry one, which is most of what a person runs this to find
/// out.
///
/// The screen's words, spelled again rather than borrowed from
/// [`UiThemeTexts`](crate::ui::UiThemeTexts): those belong to a theme and are
/// there to be changed to fit a column, and this has no screen to fit.
fn write_status(out: &mut String, state: RunnerState) {
    // Infallible, a `String` being the sink; the `Result` is `fmt::Write`'s.
    let _ = match state {
        RunnerState::Stopped => out.write_str("stopped"),
        RunnerState::Waiting => out.write_str("waiting"),
        RunnerState::Started => out.write_str("starting"),
        RunnerState::Running => out.write_str("running"),
        RunnerState::Killing => out.write_str("stopping"),
        RunnerState::ExitSuccess => out.write_str("done"),
        RunnerState::ExitError(Some(code)) => write!(out, "exit {code}"),
        RunnerState::ExitError(None) => out.write_str("failed"),
        RunnerState::Killed(Some(code)) => write!(out, "killed {code}"),
        RunnerState::Killed(None) => out.write_str("killed"),
    };
}
