use std::sync::Arc;

use arc_swap::ArcSwapOption;

use crate::{
    base::Log,
    runner::{RunnerHandle, RunnerState},
    unit::description::UnitDescription,
};

pub struct Unit {
    log: Log,
    description: UnitDescription,
    handle: ArcSwapOption<RunnerHandle>,
}

impl Unit {
    pub fn new(description: UnitDescription) -> Self {
        Self {
            log: Log::new(128),
            description,
            handle: ArcSwapOption::const_empty(),
        }
    }

    /// Start running the process
    pub fn start(&self) -> anyhow::Result<Arc<RunnerHandle>> {
        self.start_with_description(&self.description)
    }

    /// Start running the process
    pub fn start_with_description(
        &self,
        desc: impl AsRef<UnitDescription>,
    ) -> anyhow::Result<Arc<RunnerHandle>> {
        let handle = desc.as_ref().spawn(Some(self.log.writer()))?;
        self.set_handle(handle)
    }

    /// Set the handle internally
    fn set_handle(&self, handle: Arc<RunnerHandle>) -> anyhow::Result<Arc<RunnerHandle>> {
        let old_handle = self.handle.swap(Some(handle.clone()));
        match old_handle {
            None => {
                handle.start();
            }
            Some(old_handle) if old_handle.state().is_finished() => {
                handle.start();
            }
            Some(old_handle) => {
                old_handle.abort();
                let handle = handle.clone();
                tokio::spawn(async move {
                    old_handle.wait().await;
                    handle.start();
                });
            }
        }
        Ok(handle)
    }

    /// Stop the
    pub fn stop(&self) {
        if let Some(handle) = self.handle.load().as_ref() {
            handle.abort();
        }
    }

    /// State for the
    pub fn state(&self) -> RunnerState {
        self.handle
            .load()
            .as_ref()
            .map_or(RunnerState::Stopped, |s| s.state())
    }
}
