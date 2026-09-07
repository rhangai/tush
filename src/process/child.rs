use std::{
    os::unix::process::ExitStatusExt,
    process::{ExitCode, ExitStatus, Stdio},
    time::Duration,
};

use anyhow::anyhow;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    time::timeout,
};

use crate::{base::LogWriterRef, process::state::ProcessState};

const SHUTDOWN_TIMER: u64 = 10_000;

/// A running process whose stdout is captured into a [`Log`].
///
/// The stdout of the child is read line by line on a background task and
/// pushed into a ring buffer. Call [`Process::log`] to get a reader over the
/// most recent output.
pub struct ProcessChild {
    inner: ProcessChildInner,
}

enum ProcessChildInner {
    Empty,
    Setup {
        command: Command,
        writer: Option<LogWriterRef>,
    },
    Running {
        child: Child,
    },
}

impl ProcessChildInner {
    /// Waits for the process to exit, draining the remaining stdout.
    pub fn start(&mut self) -> anyhow::Result<()> {
        let old = std::mem::replace(self, ProcessChildInner::Empty);
        match old {
            ProcessChildInner::Empty => Err(anyhow!("Empty")),
            ProcessChildInner::Running { .. } => Ok(()),
            ProcessChildInner::Setup {
                mut command,
                writer,
            } => {
                let child = if let Some(mut writer) = writer {
                    command.stdout(Stdio::piped());
                    let mut child = command.spawn()?;
                    let stdout = child
                        .stdout
                        .take()
                        .ok_or(anyhow!("stdout should be piped after Stdio::piped()"))?;

                    tokio::spawn(async move {
                        let mut lines = BufReader::new(stdout).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            writer.write_line(line);
                        }
                    });
                    child
                } else {
                    command.stdout(Stdio::null());
                    let child = command.spawn()?;
                    child
                };
                *self = ProcessChildInner::Running { child };
                Ok(())
            }
        }
    }
}

impl ProcessChild {
    /// Create the child, unspawned
    pub fn new(command: Command, writer: Option<LogWriterRef>) -> Self {
        let inner = ProcessChildInner::Setup {
            command,
            writer: writer,
        };
        Self { inner }
    }

    /// Spawns `command`, piping its stdout into a log with `capacity` lines.
    pub async fn spawn(command: Command, writer: LogWriterRef) -> anyhow::Result<Self> {
        let mut child = Self::new(command, Some(writer));
        child.start().await?;
        Ok(child)
    }

    /// Try to start the proccess
    pub async fn start(&mut self) -> anyhow::Result<()> {
        self.inner.start()
    }

    /// Waits for the process to exit, draining the remaining stdout.
    pub async fn wait(&mut self) -> anyhow::Result<ProcessState> {
        if let ProcessChildInner::Running { child, .. } = &mut self.inner {
            let wait_result = child.wait().await;
            Ok(match wait_result {
                Ok(status) if status.success() => ProcessState::ExitSuccess,
                Ok(status) => ProcessState::ExitError(status.code()),
                Err(_) => ProcessState::ExitError(None),
            })
        } else {
            Err(anyhow!("Child is not running"))
        }
    }

    /// Kills the process.
    ///
    /// Sends a SIGKILL and wait for it to terminate
    pub async fn kill(&mut self) -> anyhow::Result<ProcessState> {
        self.kill_inner(false).await
    }

    /// Shutdown the process
    ///
    /// First send a sigterm then waits for n milliseconds
    /// If the process did not shutdown, it sends a SIGKILL and terminates
    pub async fn shutdown(&mut self) -> anyhow::Result<ProcessState> {
        self.kill_inner(true).await
    }

    /// Inner function to handle the shutdown logic
    async fn kill_inner(&mut self, shutdown_gracefully: bool) -> anyhow::Result<ProcessState> {
        let ProcessChildInner::Running { child, .. } = &mut self.inner else {
            return Err(anyhow!("Process was not running"));
        };

        // Check if already exited
        if let Ok(Some(exit_status)) = child.try_wait() {
            return Ok(if exit_status.success() {
                ProcessState::ExitSuccess
            } else {
                ProcessState::ExitError(exit_status.code())
            });
        }

        // Try to shutdown
        if shutdown_gracefully {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                let timer = timeout(Duration::from_millis(SHUTDOWN_TIMER), child.wait()).await;
                if let Ok(wait_result) = timer {
                    return Ok(match wait_result {
                        Ok(status) if status.success() => ProcessState::ExitSuccess,
                        Ok(status) => ProcessState::Killed(status.code()),
                        Err(_) => ProcessState::Killed(None),
                    });
                }
            }
        }

        // Kill and return the status
        child.start_kill()?;
        let wait_result = child.wait().await;
        return Ok(match wait_result {
            Ok(status) if status.success() => ProcessState::ExitSuccess,
            Ok(status) => ProcessState::Killed(status.code()),
            Err(_) => ProcessState::Killed(None),
        });
    }
}

#[cfg(test)]
mod test {
    use crate::base::Log;

    use super::*;

    #[tokio::test]
    async fn captures_stdout() {
        let mut command = Command::new("printf");
        command.arg("starting server\ntudo\nbem\n");

        let log = Log::new(1024);
        let mut process = ProcessChild::spawn(command, log.writer()).await.unwrap();
        process.wait().await.unwrap();

        let buffer = log.new_buffer();
        let lines: Vec<String> = buffer.lines().cloned().collect();
        assert_eq!(lines, vec!["starting server", "tudo", "bem"]);
    }
}
