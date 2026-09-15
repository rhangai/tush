use std::num::NonZeroU8;
use std::{process::Stdio, time::Duration};

use anyhow::anyhow;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout_at};

use crate::base::ExitReason;
use crate::log::LogWriterRef;

/// How long a graceful shutdown waits after `SIGTERM` before escalating to
/// `SIGKILL`, in milliseconds. The same budget bounds the wait after the
/// `SIGKILL`, so a process stuck in an uninterruptible wait cannot hang us.
const SHUTDOWN_TIMER: u64 = 10_000;

/// A child process whose stdout is captured into a [`Log`](crate::log::Log).
///
/// A `Process` is created unspawned ([`Process::new`]) and only touches the OS
/// on [`Process::start`], which lets a behavior be built long before it is
/// run. Once started, stdout is read line by line on a background task and
/// pushed into the log through the [`LogWriterRef`] given at construction; pass
/// `None` instead to send the output to `/dev/null`.
///
/// # Process groups
///
/// The child is spawned in its own process group (`setpgid`), and every signal
/// is sent to the whole group (`kill(-pid, ...)`). Killing a `bash -c '...'`
/// wrapper therefore takes its children down with it, which is what a process
/// manager wants: no orphaned dev servers left holding a port. A shutdown is
/// not done when the leader exits but when the group empties out, since a
/// leaked grandchild is what still holds the port.
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
            self.writer_wait().await;
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
    /// `SIGKILL`s the whole group and waits for it to be empty.
    pub async fn kill(&mut self) -> anyhow::Result<ExitReason> {
        self.kill_inner(false).await
    }

    /// Shutdown the process
    ///
    /// `SIGTERM`s the whole group and gives it [`SHUTDOWN_TIMER`] ms to drain;
    /// whatever is left over is then `SIGKILL`ed as in [`Process::kill`].
    pub async fn shutdown(&mut self) -> anyhow::Result<ExitReason> {
        let reason = self.kill_inner(true).await?;
        self.writer_wait().await;
        Ok(reason)
    }

    async fn writer_wait(&mut self) {
        if let ProcessInner::Running {
            writer_task: Some(writer_task),
            ..
        } = &mut self.inner
        {
            _ = writer_task.await;
        };
    }

    /// Inner function to handle the shutdown logic
    ///
    /// Shared by [`Process::kill`] and [`Process::shutdown`]; the flag selects
    /// whether the polite `SIGTERM` phase happens at all.
    ///
    /// Both phases act on the whole group, and both wait for the whole group:
    /// the leader exiting says nothing about the children it spawned, which are
    /// exactly the processes a manager must not leak. The returned
    /// [`ExitReason`] is the leader's, since that is the one the runner shows —
    /// a leader that had already exited keeps its real status, so a clean exit
    /// stays [`ExitReason::Success`] rather than becoming `Killed`.
    async fn kill_inner(&mut self, shutdown_gracefully: bool) -> anyhow::Result<ExitReason> {
        let ProcessInner::Running { child, pid, .. } = &mut self.inner else {
            return Err(anyhow!("Process was not running"));
        };
        let pid = *pid;
        let deadline = Instant::now() + Duration::from_millis(SHUTDOWN_TIMER);

        // `None` until the leader has been reaped.
        let mut reason = child.try_wait().ok().flatten().map(exited_reason);

        // Polite phase. The `SIGTERM` goes out even when the leader is already
        // gone, because that is precisely when children are left behind.
        if shutdown_gracefully && group::terminate(pid) {
            if reason.is_none() {
                reason = timeout_at(deadline, child.wait())
                    .await
                    .ok()
                    .map(killed_reason);
            }
            // A zombie still answers `kill(2)`, so the group can only read as
            // empty once the wait above has reaped the leader.
            if let Some(reason) = reason
                && group::wait(pid, deadline).await
            {
                return Ok(reason);
            }
        }

        // Forced phase, for whatever ignored the `SIGTERM` or never got one.
        // `start_kill` is the non-unix fallback, where there is no group to
        // signal and only the leader can be reached.
        if !group::kill(pid) && reason.is_none() {
            child.start_kill()?;
        }
        let reason = match reason {
            Some(reason) => reason,
            None => killed_reason(child.wait().await),
        };
        group::wait(pid, Instant::now() + Duration::from_millis(SHUTDOWN_TIMER)).await;
        Ok(reason)
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

/// The status a process reports for itself, as opposed to one we ended.
fn exited_reason(status: std::process::ExitStatus) -> ExitReason {
    if status.success() {
        ExitReason::Success
    } else {
        ExitReason::Error(status.code().and_then(|v| NonZeroU8::new(v as u8)))
    }
}

/// The status of a process we signalled. A failed wait still means gone.
fn killed_reason(result: std::io::Result<std::process::ExitStatus>) -> ExitReason {
    match result {
        Ok(status) if status.success() => ExitReason::Success,
        Ok(status) => ExitReason::Killed(status.code().and_then(|v| NonZeroU8::new(v as u8))),
        Err(_) => ExitReason::Killed(None),
    }
}

/// Signalling and waiting on the child's process group.
///
/// Split out so [`Process::kill_inner`] reads the same everywhere: off unix
/// there is no group, every call is a no-op reporting `false`, and the caller
/// falls back to the leader alone.
#[cfg(unix)]
mod group {
    use std::time::Duration;
    use tokio::time::{Instant, sleep};

    /// Bounds of the backoff between liveness checks.
    ///
    /// The common case is a group that empties as soon as the leader does, and
    /// one short wait catches it; a group that is going to sit out the whole
    /// timer is not worth checking hundreds of times, so the wait grows into
    /// it and the escalation costs a handful of syscalls rather than a poll.
    const POLL_MIN: Duration = Duration::from_millis(20);
    const POLL_MAX: Duration = Duration::from_millis(400);

    /// `SIGTERM` the group, reporting whether there was one to signal.
    pub fn terminate(pid: Option<u32>) -> bool {
        signal(pid, libc::SIGTERM)
    }

    /// `SIGKILL` the group, reporting whether there was one to signal.
    pub fn kill(pid: Option<u32>) -> bool {
        signal(pid, libc::SIGKILL)
    }

    /// Wait until the group holds nothing, or `deadline` passes.
    ///
    /// Polled rather than waited on: `waitpid` only reaches our own children,
    /// and the processes that matter here are the grandchildren. The leader
    /// must already be reaped when this is called — a zombie is still a member.
    pub async fn wait(pid: Option<u32>, deadline: Instant) -> bool {
        let Some(pid) = pid else { return true };
        let mut delay = POLL_MIN;
        while alive(pid) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            // Clamped, so a long backoff cannot overshoot the escalation.
            sleep(delay.min(deadline.saturating_duration_since(now))).await;
            delay = (delay * 2).min(POLL_MAX);
        }
        true
    }

    fn signal(pid: Option<u32>, signal: i32) -> bool {
        let Some(pid) = pid else { return false };
        unsafe { libc::kill(-(pid as i32), signal) };
        true
    }

    /// `kill(-pgid, 0)` fails with `ESRCH` only when the group is gone, which
    /// makes it the liveness test. `EPERM` is a member we may not signal, and
    /// that still counts as alive.
    fn alive(pid: u32) -> bool {
        if unsafe { libc::kill(-(pid as i32), 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}

#[cfg(not(unix))]
mod group {
    use tokio::time::Instant;

    pub fn terminate(_pid: Option<u32>) -> bool {
        false
    }

    pub fn kill(_pid: Option<u32>) -> bool {
        false
    }

    pub async fn wait(_pid: Option<u32>, _deadline: Instant) -> bool {
        true
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
    Running {
        child: Child,
        pid: Option<u32>,
        writer_task: Option<JoinHandle<std::io::Result<()>>>,
    },
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
                command.stdin(Stdio::null());
                let (child, writer_task) = if let Some(writer) = writer {
                    command.stdout(Stdio::piped());
                    command.stderr(Stdio::piped());
                    let mut child = command.spawn()?;
                    let stdout = child
                        .stdout
                        .take()
                        .ok_or(anyhow!("stdout should be piped after Stdio::piped()"))?;
                    let stderr = child
                        .stderr
                        .take()
                        .ok_or(anyhow!("stderr should be piped after Stdio::piped()"))?;
                    let writer_task = writer.consume_spawn_stderr(stdout, stderr);
                    (child, Some(writer_task))
                } else {
                    command.stdout(Stdio::null());
                    command.stderr(Stdio::null());
                    let child = command.spawn()?;
                    (child, None)
                };
                let pid = child.id();
                *self = ProcessInner::Running {
                    child,
                    pid,
                    writer_task,
                };
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
