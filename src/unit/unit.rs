use parking_lot::Mutex;

use crate::{
    base::LogWriterRef,
    unit::{base::UnitDescription, handle::UnitHandle, state::UnitState},
};

pub struct Unit<D: UnitDescription> {
    description: D,
    handle: Mutex<Option<UnitHandle>>,
}

impl<D: UnitDescription> Unit<D> {
    pub fn new(description: D) -> Self {
        Self {
            description,
            handle: Mutex::new(None),
        }
    }

    pub fn start(&self) -> anyhow::Result<()> {
        let mut handle = self.handle.lock();
        let is_stopped = handle.as_ref().is_none_or(|h| h.state().is_finished());
        if is_stopped {
            *handle = Some(self.spawn(None)?);
        }
        Ok(())
    }

    pub fn stop(&self) -> anyhow::Result<()> {
        let mut handle = self.handle.lock();
        if let Some(handle) = handle.as_mut() {
            handle.abort();
        }
        Ok(())
    }

    pub fn restart(&self) -> anyhow::Result<()> {
        *self.handle.lock() = Some(self.spawn(None)?);
        Ok(())
    }

    /// Spawn a new handle for this unit
    pub fn spawn(&self, writer: Option<LogWriterRef>) -> anyhow::Result<UnitHandle> {
        let handle = UnitHandle::new(self.description.exec(writer)?);
        Ok(handle)
    }

    pub fn state(&self) -> UnitState {
        self.handle
            .lock()
            .as_ref()
            .map(|h| h.state())
            .unwrap_or(UnitState::Stopped)
    }
}
