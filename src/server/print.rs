use std::{
    io::{StdoutLock, Write},
    time::Duration,
};

use tokio_util::sync::CancellationToken;

use crate::{app::App, log::LogReader, util::str::SmallStr};

/// How often the logs are looked at.
///
/// A server has no frame to hang this off, so it is a plain tick, and the
/// choice is latency against wakeups: a line appears within this long of
/// being written, and an idle session costs one atomic load per unit per
/// tick, which is what a log with no writer costs to poll.
const PRINT_INTERVAL: Duration = Duration::from_millis(100);

/// One unit's log, and where its last line got to.
struct ServerPrinterUnit {
    /// What goes in front of each line. Taken once, a proc not being renamed.
    name: SmallStr,
    /// This printer's own view of the log — one reader per view, like any
    /// other.
    reader: LogReader,
    /// Whether the next piece begins a line, and so needs the prefix.
    ///
    /// A line longer than a chunk arrives in several pieces, and only the
    /// last of them ends a line; prefixing each would cut one line into
    /// several that look whole.
    at_line_start: bool,
}

/// Every unit's output, multiplexed onto stdout.
///
/// Chunks only, so a line is printed once and when it is finished — the
/// screen shows a line as it is being typed, and a stream that did the same
/// would print its beginning twice. What that costs is latency on a process
/// that writes without a newline, which is a prompt, and there is no prompt
/// to answer here.
pub struct ServerPrinter {
    units: Vec<ServerPrinterUnit>,
}

impl ServerPrinter {
    /// Follow every unit of `app`.
    ///
    /// The set is taken once because it cannot change: it comes from a config
    /// read before the [`App`] was built. Nothing here holds the session — a
    /// [`LogReader`] keeps only a weak handle on the log it mirrors — so a
    /// printer left running cannot be what stops a session from ending.
    pub fn new(app: &App) -> Self {
        let unit_map = app.unit_map();
        let units = unit_map
            .keys()
            .filter_map(|key| {
                Some(ServerPrinterUnit {
                    name: unit_map.name(key).unwrap_or_default(),
                    reader: unit_map.log_reader(key)?,
                    at_line_start: true,
                })
            })
            .collect();
        Self { units }
    }

    /// Print until `cancel`, and then once more.
    ///
    /// The last drain is the point of taking a token rather than being
    /// aborted: the final lines of a session are the notes saying each
    /// process exited, and they are written during shutdown — which is after
    /// anything that cancels this has happened.
    pub async fn run(mut self, cancel: CancellationToken) {
        let mut ticks = tokio::time::interval(PRINT_INTERVAL);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticks.tick() => self.drain(),
            }
        }
        self.drain();
    }

    /// Put out whatever arrived since the last look.
    ///
    /// The lock is taken once for the whole pass rather than per line, which
    /// is what keeps one tick's output together instead of interleaved with
    /// whatever else the process is writing.
    ///
    /// A write that fails is dropped: stdout being a closed pipe is not a
    /// reason to take a session down, and there is nowhere left to report it
    /// to anyway.
    fn drain(&mut self) {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for unit in &mut self.units {
            unit.drain(&mut out);
        }
        let _ = out.flush();
    }
}

impl ServerPrinterUnit {
    /// The pieces this unit gained since the last drain.
    ///
    /// [`sync`](LogReader::sync) reports how many chunks are new and they are
    /// the newest ones held, so the walk is the whole reader filtered down to
    /// them. That walks pieces already printed to reach the ones that are
    /// not — the cost the log module warns about — and it is paid only when
    /// something arrived, never on an idle tick.
    fn drain(&mut self, out: &mut StdoutLock<'_>) {
        let fresh = self.reader.sync();
        if fresh == 0 {
            return;
        }
        let first = self.reader.len() - fresh;
        for piece in self.reader.iter_unsync() {
            if piece.index() < first {
                continue;
            }
            if self.at_line_start {
                let _ = write!(out, "[{}] ", self.name);
            }
            let _ = out.write_all(piece.as_str().as_bytes());
            self.at_line_start = piece.newline();
            if piece.newline() {
                let _ = out.write_all(b"\n");
            }
        }
    }
}
