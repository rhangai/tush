use std::num::NonZeroU8;
use std::{process::Stdio, time::Duration};

use anyhow::anyhow;
use tokio::{
    process::{Child, Command},
    time::timeout,
};

use crate::base::ExitReason;
use crate::log::LogWriterRef;

/// How long a graceful shutdown waits after `SIGTERM` before escalating to
/// `SIGKILL`, in milliseconds.
const SHUTDOWN_TIMER: u64 = 10_000;

/// A child process whose stdout is captured into a [`Log`](crate::base::Log).
///
/// A `Process` is created unspawned ([`Process::new`]) and only touches the OS
/// on [`Process::start`], which lets a description be built long before it is
/// run. Once started, stdout is read line by line on a background task and
/// pushed into the log through the [`LogWriterRef`] given at construction; pass
/// `None` instead to send the output to `/dev/null`.
///
/// # Process groups
///
/// The child is spawned in its own process group (`setpgid`), and every signal
/// is sent to the whole group (`kill(-pid, ...)`). Killing a `bash -c '...'`
/// wrapper therefore takes its children down with it, which is what a process
/// manager wants: no orphaned dev servers left holding a port.
///
/// # Drop
///
/// Dropping a running `Process` `SIGKILL`s its group. Nothing is awaited, so
/// this is a last resort safety net rather than a substitute for
/// [`Process::shutdown`].
pub struct Process {
    inner: ProcessInner,
}

impl Process {
    /// Create the child, unspawned
    ///
    /// Nothing runs until [`Process::start`] is called. `writer` is where
    /// stdout will be sent; `None` discards it.
    pub fn new(command: Command, writer: Option<LogWriterRef>) -> Self {
        let inner = ProcessInner::Setup { command, writer };
        Self { inner }
    }

    /// Create and immediately start a process piping its stdout into `writer`.
    pub async fn spawn(command: Command, writer: LogWriterRef) -> anyhow::Result<Self> {
        let mut child = Self::new(command, Some(writer));
        child.start().await?;
        Ok(child)
    }

    /// Try to start the proccess
    ///
    /// Spawning an already running process is a no-op; spawning a consumed one
    /// is an error.
    pub async fn start(&mut self) -> anyhow::Result<()> {
        self.inner.start()
    }

    /// Waits for the process to exit, draining the remaining stdout.
    ///
    /// Errors if the process was never started. A failed `wait` is reported as
    /// [`ExitReason::Error(None)`](ExitReason::Error) rather than an `Err`,
    /// since the process is gone either way.
    pub async fn wait(&mut self) -> anyhow::Result<ExitReason> {
        if let ProcessInner::Running { child, .. } = &mut self.inner {
            let wait_result = child.wait().await;
            Ok(match wait_result {
                Ok(status) if status.success() => ExitReason::Success,
                Ok(status) => {
                    ExitReason::Error(status.code().and_then(|v| NonZeroU8::new(v as u8)))
                }
                Err(_) => ExitReason::Error(None),
            })
        } else {
            Err(anyhow!("Child is not running"))
        }
    }

    /// Kills the process.
    ///
    /// Sends a SIGKILL and wait for it to terminate
    pub async fn kill(&mut self) -> anyhow::Result<ExitReason> {
        self.kill_inner(false).await
    }

    /// Shutdown the process
    ///
    /// First send a sigterm then waits for n milliseconds
    /// If the process did not shutdown, it sends a SIGKILL and terminates
    pub async fn shutdown(&mut self) -> anyhow::Result<ExitReason> {
        self.kill_inner(true).await
    }

    /// Inner function to handle the shutdown logic
    ///
    /// Shared by [`Process::kill`] and [`Process::shutdown`]; the flag selects
    /// whether the polite `SIGTERM` phase happens at all. The paths are:
    ///
    /// 1. already exited — the group is still `SIGTERM`ed to sweep up children
    ///    that outlived the leader, and the real status is returned as-is
    ///    (so a clean exit stays [`ExitReason::Success`], not `Killed`);
    /// 2. graceful — `SIGTERM` the group, wait up to [`SHUTDOWN_TIMER`] ms;
    /// 3. forced — `SIGKILL` the group and wait.
    ///
    /// The final `start_kill` branch is the non-unix fallback, where signaling
    /// the group is not available.
    async fn kill_inner(&mut self, shutdown_gracefully: bool) -> anyhow::Result<ExitReason> {
        let ProcessInner::Running { child, pid, .. } = &mut self.inner else {
            return Err(anyhow!("Process was not running"));
        };
        let pid = *pid;

        // Check if already exited
        if let Ok(Some(exit_status)) = child.try_wait() {
            #[cfg(unix)]
            if let Some(pid) = pid {
                unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
            }

            return Ok(if exit_status.success() {
                ExitReason::Success
            } else {
                ExitReason::Error(exit_status.code().and_then(|v| NonZeroU8::new(v as u8)))
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
                        Ok(status) if status.success() => ExitReason::Success,
                        Ok(status) => {
                            ExitReason::Killed(status.code().and_then(|v| NonZeroU8::new(v as u8)))
                        }
                        Err(_) => ExitReason::Killed(None),
                    });
                }
            }
        }

        #[cfg(unix)]
        if let Some(pid) = pid {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            let wait_result = child.wait().await;
            return Ok(match wait_result {
                Ok(status) if status.success() => ExitReason::Success,
                Ok(status) => {
                    ExitReason::Killed(status.code().and_then(|v| NonZeroU8::new(v as u8)))
                }
                Err(_) => ExitReason::Killed(None),
            });
        }

        // Kill and return the status
        child.start_kill()?;
        let wait_result = child.wait().await;
        Ok(match wait_result {
            Ok(status) if status.success() => ExitReason::Success,
            Ok(status) => ExitReason::Killed(status.code().and_then(|v| NonZeroU8::new(v as u8))),
            Err(_) => ExitReason::Killed(None),
        })
    }
}

impl Drop for Process {
    /// `SIGKILL` the process group so a dropped handle never leaks a child.
    fn drop(&mut self) {
        #[cfg(unix)]
        if let ProcessInner::Running { pid: Some(pid), .. } = &mut self.inner {
            unsafe { libc::kill(-(*pid as i32), libc::SIGKILL) };
        };
    }
}

/// Inner data for process
///
/// The lifecycle is `Setup -> Running`. `Empty` is the transient state used
/// while the command is moved out of `Setup` during the spawn, and is also
/// where a process lands if that spawn fails.
enum ProcessInner {
    /// Consumed or failed to spawn: nothing left to run or signal.
    Empty,
    /// Built but not spawned yet.
    Setup {
        command: Command,
        writer: Option<LogWriterRef>,
    },
    /// Spawned. `pid` is `None` if the child already exited and was reaped.
    Running { child: Child, pid: Option<u32> },
}

impl ProcessInner {
    /// Spawn the command, wiring stdout to the writer when there is one.
    ///
    /// The child gets its own process group so signals can be delivered to the
    /// whole tree, and the stdout pump runs detached: it ends by itself when
    /// the pipe closes.
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
                let child = if let Some(writer) = writer {
                    command.stdout(Stdio::piped());
                    let mut child = command.spawn()?;
                    let stdout = child
                        .stdout
                        .take()
                        .ok_or(anyhow!("stdout should be piped after Stdio::piped()"))?;
                    writer.consume_spawn(stdout);
                    child
                } else {
                    command.stdout(Stdio::null());
                    command.spawn()?
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
    use crate::log::Log;

    use super::*;

    #[tokio::test]
    async fn captures_stdout() {
        let mut command = Command::new("printf");
        command.arg("starting server\ntudo\nbem\n");

        let log = Log::new(1024);
        let mut process = Process::spawn(command, log.writer()).await.unwrap();
        process.wait().await.unwrap();
    }
}
