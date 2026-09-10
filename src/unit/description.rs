use std::sync::Arc;

use anyhow::Result;
use enum_dispatch::enum_dispatch;
use tokio::process::Command;

use crate::{
    base::{LogWriterRef, Process},
    runner::RunnerHandle,
};

pub struct UnitDescription {
    inner: UnitDescriptionInner,
}

impl UnitDescription {
    pub fn program() -> Self {
        Self {
            inner: UnitDescriptionInner::Program(DescProgram {}),
        }
    }

    pub fn noop() -> Self {
        Self {
            inner: UnitDescriptionInner::Noop(DescNoop {}),
        }
    }

    pub fn spawn(&self, writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        self.inner.spawn(writer)
    }
}

impl AsRef<UnitDescription> for UnitDescription {
    fn as_ref(&self) -> &UnitDescription {
        self
    }
}

#[enum_dispatch]
enum UnitDescriptionInner {
    Noop(DescNoop),
    Program(DescProgram),
}

#[enum_dispatch(UnitDescriptionInner)]
trait UnitDescriptionBehavior {
    fn spawn(&self, writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>>;
}

struct DescProgram {}
impl UnitDescriptionBehavior for DescProgram {
    fn spawn(&self, writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        let mut command = Command::new("bash");
        command.args(["-c", "echo 'oi'; sleep 1; echo 'tchau'; exit 2"]);
        let proc = Process::new(command, writer);
        Ok(RunnerHandle::new(proc))
    }
}

struct DescNoop {}
impl UnitDescriptionBehavior for DescNoop {
    fn spawn(&self, _writer: Option<LogWriterRef>) -> Result<Arc<RunnerHandle>> {
        Ok(RunnerHandle::new(()))
    }
}
