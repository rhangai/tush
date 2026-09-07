use std::{collections::HashMap, sync::Arc};

use anyhow::anyhow;
use parking_lot::RwLock;

use crate::process::{Process, state::ProcessState};

struct ProcessPoolItem {
    process: RwLock<Process>,
}

pub struct ProcessPool {
    log_capacity: usize,
    map: HashMap<String, Arc<ProcessPoolItem>>,
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
        let item = Arc::new(ProcessPoolItem {
            process: RwLock::new(process),
        });
        self.map.insert(key.into(), item);
    }

    pub fn start(&self, key: impl AsRef<str>) -> anyhow::Result<()> {
        self.with_process(key, |p| p.start()).flatten()
    }

    pub fn state(&self, key: impl AsRef<str>) -> Option<ProcessState> {
        self.with_process(key, |p| p.state()).ok()
    }

    pub fn stop(&self, key: impl AsRef<str>) -> anyhow::Result<()> {
        self.with_process(key, |p| p.stop())
    }

    pub fn restart(&self, key: impl AsRef<str>) -> anyhow::Result<()> {
        self.with_process(key, |p| p.restart()).flatten()
    }

    pub async fn wait(&self, key: impl AsRef<str>) -> anyhow::Result<()> {
        let Some(item) = self.map.get(key.as_ref()) else {
            return Err(anyhow!("Invalid process {}", key.as_ref()));
        };
        let mut proc = item.process.write();
        proc.wait().await;
        Ok(())
    }

    fn with_process<U>(
        &self,
        key: impl AsRef<str>,
        f: impl FnOnce(&mut Process) -> U,
    ) -> anyhow::Result<U> {
        if let Some(item) = self.map.get(key.as_ref()) {
            let mut lock = item.process.write();
            Ok(f(&mut lock))
        } else {
            Err(anyhow!("Invalid process {}", key.as_ref()))
        }
    }
}
