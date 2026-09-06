use tokio::task::JoinSet;

use crate::{
    base::LogWriterRef,
    unit::{
        UnitDefiniton, base::UnitDefinitonHelper, group::UnitGroup, handle::UnitHandle,
        process::UnitProcess,
    },
};

enum UnitInner {
    Process(UnitProcess),
    Group(UnitGroup),
}

pub struct Unit {
    inner: UnitInner,
}

impl From<UnitProcess> for Unit {
    fn from(value: UnitProcess) -> Self {
        let inner = UnitInner::Process(value);
        Self { inner }
    }
}

impl From<UnitGroup> for Unit {
    fn from(value: UnitGroup) -> Self {
        let inner = UnitInner::Group(value);
        Self { inner }
    }
}

impl UnitDefiniton for Unit {
    fn exec(&self, writer: LogWriterRef) -> UnitHandle {
        match &self.inner {
            UnitInner::Process(unit_process) => {
                UnitHandle::from_runner(unit_process.create_runner(writer))
            }
            UnitInner::Group(unit_group) => {
                UnitHandle::from_runner(unit_group.create_runner(writer))
            }
        }
    }

    fn exec_in_set(&self, join_set: &mut JoinSet<()>, writer: LogWriterRef) -> UnitHandle {
        match &self.inner {
            UnitInner::Process(unit_process) => {
                UnitHandle::from_runner_set(join_set, unit_process.create_runner(writer))
            }
            UnitInner::Group(unit_group) => {
                UnitHandle::from_runner_set(join_set, unit_group.create_runner(writer))
            }
        }
    }
}
