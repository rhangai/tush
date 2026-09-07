use std::{collections::HashMap, sync::Arc};

use parking_lot::{RwLock, RwLockWriteGuard};

use crate::{
    base::LogBuffer,
    process::{Process, state::ProcessState},
    util::ring::RingStrLines,
};

struct ProcessPoolItem {
    process: RwLock<Process>,
    log_buffer: RwLock<LogBuffer>,
}

impl ProcessPoolItem {
    fn sync(&self) {
        let process = self.process.read();
        let mut buffer = self.log_buffer.write();
        process.log().update_buffer(&mut buffer);
    }

    fn use_lines_sync(&self, f: impl FnOnce(RingStrLines)) {
        let read = {
            let process = self.process.read();
            let mut buffer = self.log_buffer.write();
            process.log().update_buffer(&mut buffer);
            RwLockWriteGuard::downgrade(buffer)
        };
        f(read.lines())
    }
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
        let process = Process::new(self.log_capacity, command);
        let log_buffer = process.log().new_buffer();
        let item = Arc::new(ProcessPoolItem {
            process: RwLock::new(process),
            log_buffer: RwLock::new(log_buffer),
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

    pub fn log_sync(&self) {
        for value in self.map.values() {
            value.sync();
        }
    }

    pub fn use_lines(&self, key: impl AsRef<str>, f: impl FnOnce(RingStrLines<'_>)) {
        if let Some(item) = self.map.get(key.as_ref()) {
            item.use_lines_sync(f);
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
