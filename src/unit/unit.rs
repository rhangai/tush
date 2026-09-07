use enum_dispatch::enum_dispatch;

use crate::process::Process;

pub enum UnitState {
    Stopped,
    Started,
    Running,
    ExitSuccess,
    ExitError,
    Killing,
    Killed,
}

pub struct Unit {
    inner: UnitInner,
}

impl Unit {
    pub fn start(&self) -> anyhow::Result<()> {
        self.inner.start()
    }
    pub fn stop(&self) -> anyhow::Result<()> {
        self.inner.stop()
    }
    pub fn restart(&self) -> anyhow::Result<()> {
        self.inner.restart()
    }
    pub fn state(&self) -> UnitState {
        self.inner.state()
    }
    pub fn children(&self) -> Option<&Vec<Unit>> {
        self.inner.children()
    }
}

///
#[enum_dispatch]
trait UnitBehavior {
    fn start(&self) -> anyhow::Result<()>;
    fn restart(&self) -> anyhow::Result<()>;
    fn stop(&self) -> anyhow::Result<()>;
    fn state(&self) -> UnitState;
    async fn wait(&self);
    fn children(&self) -> Option<&Vec<Unit>>;
}

#[enum_dispatch(UnitBehavior)]
enum UnitInner {
    Process(UnitProcess),
    Multiple(UnitMultiple),
}

struct UnitProcess {
    process: Process,
}
impl UnitBehavior for UnitProcess {
    fn start(&self) -> anyhow::Result<()> {
        self.process.start()
    }

    fn restart(&self) -> anyhow::Result<()> {
        self.process.restart()
    }

    fn stop(&self) -> anyhow::Result<()> {
        self.process.stop()
    }

    fn state(&self) -> UnitState {
        UnitState::Stopped
    }

    async fn wait(&self) {}

    fn children(&self) -> Option<&Vec<Unit>> {
        None
    }
}

struct UnitMultiple {
    units: Vec<Unit>,
}
impl UnitBehavior for UnitMultiple {
    fn start(&self) -> anyhow::Result<()> {
        for unit in &self.units {
            unit.start()?;
        }
        Ok(())
    }

    fn restart(&self) -> anyhow::Result<()> {
        for unit in &self.units {
            unit.restart()?;
        }
        Ok(())
    }

    fn stop(&self) -> anyhow::Result<()> {
        for unit in &self.units {
            unit.stop()?;
        }
        Ok(())
    }

    fn state(&self) -> UnitState {
        UnitState::Stopped
    }

    async fn wait(&self) {}

    fn children(&self) -> Option<&Vec<Unit>> {
        Some(self.units.as_ref())
    }
}
