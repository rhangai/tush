use std::num::NonZeroUsize;

use tokio::task::JoinSet;

use crate::{
    base::{Log, LogWriter},
    unit::{
        Unit, UnitDefiniton, UnitProcess,
        base::{UnitDefinitonHelper, UnitRunner},
        handle::UnitHandle,
    },
};

pub struct UnitGroup {
    units: Vec<Unit>,
}

impl UnitGroup {
    pub fn new() -> Self {
        Self { units: Vec::new() }
    }

    pub fn add(&mut self, unit: impl Into<Unit>) {
        self.units.push(unit.into());
    }
}

impl UnitDefinitonHelper for UnitGroup {
    fn create_runner(&self, writer: LogWriter) -> impl UnitRunner {
        let mut join_set = JoinSet::new();
        let mut handles: Vec<UnitHandle> = Vec::with_capacity(self.units.len());
        for unit in &self.units {
            let writer = writer.share();
            let handle = unit.exec_in_set(&mut join_set, writer);
            handles.push(handle);
        }
        UnitGroupRunner { join_set, handles }
    }
}

struct UnitGroupRunner {
    join_set: JoinSet<()>,
    handles: Vec<UnitHandle>,
}
impl UnitRunner for UnitGroupRunner {
    async fn wait(&mut self) -> bool {
        while let Some(_) = self.join_set.join_next().await {}
        true
    }

    async fn kill(self) {
        for handle in self.handles {
            handle.kill();
        }
    }
}
