use std::{process::Stdio, time::Duration};

use anyhow::anyhow;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    time::timeout,
};

use crate::base::LogWriterRef;

const SHUTDOWN_TIMER: u64 = 10_000;

/// Exit state for the process
#[derive(Clone, Copy, Debug)]
pub enum ProcessExit {
    Success,
    Error(Option<i32>),
    Killed(Option<i32>),
}

/// A running process whose stdout is captured into a [`Log`].
///
/// The stdout of the child is read line by line on a background task and
/// pushed into a ring buffer. Call [`Process::log`] to get a reader over the
/// most recent output.
pub struct Process {
    inner: ProcessInner,
}

impl Process {
    /// Create the child, unspawned
    pub fn new(command: Command, writer: Option<LogWriterRef>) -> Self {
        let inner = ProcessInner::Setup {
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
    pub async fn wait(&mut self) -> anyhow::Result<ProcessExit> {
        if let ProcessInner::Running { child, .. } = &mut self.inner {
            let wait_result = child.wait().await;
            Ok(match wait_result {
                Ok(status) if status.success() => ProcessExit::Success,
                Ok(status) => ProcessExit::Error(status.code()),
                Err(_) => ProcessExit::Error(None),
            })
        } else {
            Err(anyhow!("Child is not running"))
        }
    }

    /// Kills the process.
    ///
    /// Sends a SIGKILL and wait for it to terminate
    pub async fn kill(&mut self) -> anyhow::Result<ProcessExit> {
        self.kill_inner(false).await
    }

    /// Shutdown the process
    ///
    /// First send a sigterm then waits for n milliseconds
    /// If the process did not shutdown, it sends a SIGKILL and terminates
    pub async fn shutdown(&mut self) -> anyhow::Result<ProcessExit> {
        self.kill_inner(true).await
    }

    /// Inner function to handle the shutdown logic
    async fn kill_inner(&mut self, shutdown_gracefully: bool) -> anyhow::Result<ProcessExit> {
        let ProcessInner::Running { child, pid, .. } = &mut self.inner else {
            return Err(anyhow!("Process was not running"));
        };
        let pid = pid.clone();

        // Check if already exited
        if let Ok(Some(exit_status)) = child.try_wait() {
            #[cfg(unix)]
            if let Some(pid) = pid {
                unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
            }

            return Ok(if exit_status.success() {
                ProcessExit::Success
            } else {
                ProcessExit::Error(exit_status.code())
            });
        }

        // Try to shutdown
        if shutdown_gracefully {
            #[cfg(unix)]
            if let Some(pid) = pid {
                unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
                let timer = timeout(Duration::from_millis(SHUTDOWN_TIMER), child.wait()).await;
                if let Ok(wait_result) = timer {
                    return Ok(match wait_result {
                        Ok(status) if status.success() => ProcessExit::Success,
                        Ok(status) => ProcessExit::Killed(status.code()),
                        Err(_) => ProcessExit::Killed(None),
                    });
                }
            }
        }

        #[cfg(unix)]
        if let Some(pid) = pid {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            let wait_result = child.wait().await;
            return Ok(match wait_result {
                Ok(status) if status.success() => ProcessExit::Success,
                Ok(status) => ProcessExit::Killed(status.code()),
                Err(_) => ProcessExit::Killed(None),
            });
        }

        // Kill and return the status
        child.start_kill()?;
        let wait_result = child.wait().await;
        return Ok(match wait_result {
            Ok(status) if status.success() => ProcessExit::Success,
            Ok(status) => ProcessExit::Killed(status.code()),
            Err(_) => ProcessExit::Killed(None),
        });
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let ProcessInner::Running { pid, .. } = &mut self.inner {
            #[cfg(unix)]
            if let Some(pid) = *pid {
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
        };
    }
}

/// Inner data for process
enum ProcessInner {
    Empty,
    Setup {
        command: Command,
        writer: Option<LogWriterRef>,
    },
    Running {
        child: Child,
        pid: Option<u32>,
    },
}

impl ProcessInner {
    /// Waits for the process to exit, draining the remaining stdout.
    pub fn start(&mut self) -> anyhow::Result<()> {
        let old = std::mem::replace(self, ProcessInner::Empty);
        match old {
            ProcessInner::Empty => Err(anyhow!("Empty")),
            ProcessInner::Running { .. } => Ok(()),
            ProcessInner::Setup {
                mut command,
                writer,
            } => {
                command.process_group(0);
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
                let pid = child.id();
                *self = ProcessInner::Running { child, pid };
                Ok(())
            }
        }
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
        let mut process = Process::spawn(command, log.writer()).await.unwrap();
        process.wait().await.unwrap();

        let buffer = log.new_buffer();
        let lines: Vec<String> = buffer.lines().cloned().collect();
        assert_eq!(lines, vec!["starting server", "tudo", "bem"]);
    }
}
