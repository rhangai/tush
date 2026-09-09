use std::sync::Arc;

use arc_swap::ArcSwapOption;

use crate::{
    base::{Log, LogWriterRef},
    runner::{RunnerHandle, RunnerState},
};

/// A runner
pub trait UnitDescription {
    fn spawn(&self, writer: Option<LogWriterRef>) -> anyhow::Result<Arc<RunnerHandle>>;
}

pub struct Unit {
    log: Log,
    handle: ArcSwapOption<RunnerHandle>,
}

impl Unit {
    pub fn new() -> Self {
        Self {
            log: Log::new(128),
            handle: ArcSwapOption::const_empty(),
        }
    }

    ///
    pub fn start(&self, description: &impl UnitDescription) -> anyhow::Result<Arc<RunnerHandle>> {
        self.stop();
        let handle = description.spawn(Some(self.log.writer()))?;
        let old_handle = self.handle.swap(Some(handle.clone()));
        Ok(handle)
    }

    ///
    pub fn stop(&self) {
        if let Some(handle) = self.handle.load().as_ref() {
            handle.abort();
        }
    }

    ///
    pub fn state(&self) -> RunnerState {
        self.handle
            .load()
            .as_ref()
            .map_or(RunnerState::Stopped, |s| s.state())
    }
}
