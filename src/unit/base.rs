use tokio::task::JoinSet;

use crate::{base::LogWriterRef, unit::handle::UnitHandle};

pub enum UnitState {
    Started,
    Running,
    Finished,
    Failed,
    Killing,
    Killed,
}

pub trait UnitRunner: Send + 'static {
    fn wait(&mut self) -> impl Future<Output = bool> + Send;
    fn kill(self) -> impl Future<Output = ()> + Send;
}

pub trait UnitDefinitonHelper {
    fn create_runner(&self, writer: LogWriterRef) -> impl UnitRunner;
}

pub trait UnitDefiniton {
    fn exec(&self, writer: LogWriterRef) -> UnitHandle;
    fn exec_in_set(&self, join_set: &mut JoinSet<()>, writer: LogWriterRef) -> UnitHandle;
}

impl<T: UnitDefinitonHelper> UnitDefiniton for T {
    fn exec(&self, writer: LogWriterRef) -> UnitHandle {
        UnitHandle::from_runner(self.create_runner(writer))
    }

    fn exec_in_set(&self, join_set: &mut JoinSet<()>, writer: LogWriterRef) -> UnitHandle {
        UnitHandle::from_runner_set(join_set, self.create_runner(writer))
    }
}
