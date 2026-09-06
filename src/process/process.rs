use tokio::{process::Command, task::JoinSet};

use crate::{
    base::{Log, LogBuffer},
    process::{child::ProcessChild, handle::ProcessHandle},
};

pub struct Process {
    log: Log,
    log_buffer: LogBuffer,
    handle: Option<ProcessHandle>,
}

impl Process {
    pub fn new(capacity: usize) -> Self {
        let log = Log::new(capacity);
        let log_buffer = log.new_buffer();
        Self {
            log,
            log_buffer,
            handle: None,
        }
    }

    pub fn log_sync(&mut self) {
        self.log.update_buffer(&mut self.log_buffer);
    }

    pub fn lines(&self) -> impl IntoIterator<Item = &String> {
        self.log_buffer.lines()
    }

    pub fn lines_sync(&mut self) -> impl IntoIterator<Item = &String> {
        self.log_sync();
        self.lines()
    }

    pub fn start(&mut self) {
        if let Some(handle) = &self.handle {
            if handle.state().is_finished() {
                self.start_inner(None);
            }
            return;
        }
        self.start_inner(None);
    }

    pub fn restart(&mut self) {
        self.start_inner(None);
    }

    pub fn stop(&mut self) {
        if let Some(mut handle) = self.handle.take() {
            handle.kill();
        }
    }

    pub async fn run(&mut self) {
        let mut join_set = JoinSet::new();
        self.start_inner(Some(&mut join_set));
        join_set.join_all().await;
    }

    fn start_inner(&mut self, join_set: Option<&mut JoinSet<()>>) {
        let writer = self.log.writer();
        let mut command = Command::new("bash");
        command.args(&["-c", "echo 'oi'; sleep 1; echo 'tchau'"]);
        let child = ProcessChild::spawn(command, writer).unwrap();
        let handle = if let Some(join_set) = join_set {
            ProcessHandle::new_in_join_set(child, join_set)
        } else {
            ProcessHandle::new(child)
        };
        self.handle = Some(handle);
    }
}
