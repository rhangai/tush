use crate::{base::LogWriterRef, unit::state::UnitExitReason};

///
pub trait UnitRunner: Send + 'static {
    fn run(&mut self) -> impl Future<Output = anyhow::Result<UnitExitReason>> + Send;
    fn shutdown(&mut self) -> impl Future<Output = anyhow::Result<UnitExitReason>> + Send;
}

/// Description of an unit
pub trait UnitDescription {
    fn exec(&self, writer: Option<LogWriterRef>) -> anyhow::Result<impl UnitRunner>;
}
