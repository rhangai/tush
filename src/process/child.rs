use std::process::Stdio;

use anyhow::anyhow;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    task::JoinHandle,
};

use crate::base::LogWriterRef;

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
        writer: LogWriterRef,
    },
    Running {
        child: Child,
        reader: JoinHandle<()>,
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
                mut writer,
            } => {
                command.stdout(Stdio::piped());
                let mut child = command.spawn().unwrap();
                let stdout = child
                    .stdout
                    .take()
                    .ok_or(anyhow!("stdout should be piped after Stdio::piped()"))?;
                let reader = tokio::spawn(async move {
                    let mut lines = BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        writer.write_line(line);
                    }
                });
                *self = ProcessChildInner::Running { child, reader };
                Ok(())
            }
        }
    }
}

impl ProcessChild {
    /// Create the child, unspawned
    pub fn new(command: Command, writer: LogWriterRef) -> Self {
        let inner = ProcessChildInner::Setup { command, writer };
        Self { inner }
    }

    /// Spawns `command`, piping its stdout into a log with `capacity` lines.
    pub async fn spawn(command: Command, writer: LogWriterRef) -> anyhow::Result<Self> {
        let mut child = Self::new(command, writer);
        child.start().await?;
        Ok(child)
    }

    /// Try to start the proccess
    pub async fn start(&mut self) -> anyhow::Result<()> {
        self.inner.start()
    }

    /// Waits for the process to exit, draining the remaining stdout.
    pub async fn wait(&mut self) -> anyhow::Result<std::process::ExitStatus> {
        if let ProcessChildInner::Running { child, .. } = &mut self.inner {
            let status = child.wait().await?;
            Ok(status)
        } else {
            Err(anyhow!("Child is not running"))
        }
    }

    /// Kills the process.
    pub async fn kill(&mut self) -> anyhow::Result<()> {
        if let ProcessChildInner::Running { child, .. } = &mut self.inner {
            _ = child.kill().await?;
            Ok(())
        } else {
            Ok(())
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
        let mut process = ProcessChild::spawn(command, log.writer()).await.unwrap();
        process.wait().await.unwrap();

        let buffer = log.new_buffer();
        let lines: Vec<String> = buffer.lines().cloned().collect();
        assert_eq!(lines, vec!["starting server", "tudo", "bem"]);
    }
}
