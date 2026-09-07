use std::{collections::HashMap, sync::Arc};

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

    pub fn start(&self, key: impl AsRef<str>) {
        self.with_process(key, |p| p.start());
    }

    pub fn state(&self, key: impl AsRef<str>) -> Option<ProcessState> {
        self.with_process(key, |p| p.state())
    }

    pub fn stop(&self, key: impl AsRef<str>) {
        self.with_process(key, |p| p.stop());
    }

    pub fn restart(&self, key: impl AsRef<str>) {
        self.with_process(key, |p| p.restart());
    }

    pub async fn wait(&self, key: impl AsRef<str>) {
        if let Some(item) = self.map.get(key.as_ref()) {
            let mut proc = item.process.write();
            proc.wait().await;
        }
    }

    fn with_process<U>(
        &self,
        key: impl AsRef<str>,
        f: impl FnOnce(&mut Process) -> U,
    ) -> Option<U> {
        if let Some(item) = self.map.get(key.as_ref()) {
            let mut lock = item.process.write();
            Some(f(&mut lock))
        } else {
            None
        }
    }
}
