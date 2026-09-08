use crate::{base::LogWriterRef, unit::state::UnitExitReason};

type Exit = anyhow::Result<UnitExitReason>;

pub trait UnitRunner: Send + 'static {
    fn run(&mut self) -> impl Future<Output = Exit> + Send;
    fn shutdown(&mut self) -> impl Future<Output = Exit> + Send;
}

pub trait UnitDescription {
    fn exec(&self, writer: Option<LogWriterRef>) -> anyhow::Result<impl UnitRunner>;
}
