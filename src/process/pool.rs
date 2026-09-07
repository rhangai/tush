use std::{collections::HashMap, sync::Arc};

use anyhow::anyhow;

use crate::process::{Process, state::ProcessState};

pub struct ProcessPool {
    log_capacity: usize,
    map: HashMap<String, Arc<Process>>,
}

impl ProcessPool {
    pub fn new(log_capacity: usize) -> Self {
        Self {
            log_capacity,
            map: HashMap::new(),
        }
    }
    pub fn add(
        &mut self,
        key: impl Into<String>,
        command: impl IntoIterator<Item = impl Into<String>>,
    ) {
        let process = Process::new(command);
        let item = Arc::new(process);
        self.map.insert(key.into(), item);
    }

    pub fn start(&self, key: impl AsRef<str>) -> anyhow::Result<()> {
        let proc = self.get_proc(key)?;
        proc.start()
    }

    pub fn state(&self, key: impl AsRef<str>) -> Option<ProcessState> {
        self.get_proc(key).ok().map(|s| s.state())
    }

    pub fn stop(&self, key: impl AsRef<str>) -> anyhow::Result<()> {
        let proc = self.get_proc(key)?;
        proc.stop()
    }

    pub fn restart(&self, key: impl AsRef<str>) -> anyhow::Result<()> {
        let proc = self.get_proc(key)?;
        proc.restart()
    }

    pub async fn wait(&self, key: impl AsRef<str>) -> anyhow::Result<()> {
        let proc = self.get_proc(key)?;
        proc.wait().await
    }

    fn get_proc(&self, key: impl AsRef<str>) -> anyhow::Result<Arc<Process>> {
        if let Some(item) = self.map.get(key.as_ref()) {
            Ok(item.clone())
        } else {
            Err(anyhow!("Invalid process {}", key.as_ref()))
        }
    }
}
