use std::process::Stdio;

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
    child: Child,
    reader: JoinHandle<()>,
}

impl ProcessChild {
    /// Spawns `command`, piping its stdout into a log with `capacity` lines.
    pub fn spawn(mut command: Command, mut writer: LogWriterRef) -> std::io::Result<Self> {
        command.stdout(Stdio::piped());
        let mut child = command.spawn()?;

        let stdout = child
            .stdout
            .take()
            .expect("stdout should be piped after Stdio::piped()");

        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                writer.write_line(line);
            }
        });

        Ok(Self { child, reader })
    }

    /// Waits for the process to exit, draining the remaining stdout.
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.wait().await?;
        let _ = (&mut self.reader).await;
        Ok(status)
    }

    /// Kills the process.
    pub async fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill().await
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
        let mut process = ProcessChild::spawn(command, log.writer()).unwrap();
        process.wait().await.unwrap();

        let buffer = log.new_buffer();
        let lines: Vec<String> = buffer.lines().cloned().collect();
        assert_eq!(lines, vec!["starting server", "tudo", "bem"]);
    }
}
