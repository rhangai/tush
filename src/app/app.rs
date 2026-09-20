use std::sync::Arc;

use anyhow::Result;

use crate::{
    app::{schedule::AppSchedule, unit_map::AppUnitMap},
    config::Config,
    unit::{UnitAction, UnitEvent, UnitKey},
};

/// A session that has been checked and is ready to be run.
///
/// The type is the proof: an `App` is a config that survived
/// [`new`](App::new), so anything holding one can stop asking whether the
/// procs it names exist or whether their dependencies can be satisfied.
///
/// The start order is not here yet.
pub struct App {
    /// Every proc as a [`Unit`](crate::unit::Unit), by its key.
    unit_map: Arc<AppUnitMap>,
    /// Holds the starts that cannot happen yet, and performs them when they
    /// can — see [`AppSchedule`].
    schedule: AppSchedule,
}

impl App {
    /// Check a config, and build the session from it.
    ///
    /// Checked: that no proc declares both `run` and `modes`, that every
    /// `depends` names a proc that exists, and that nothing depends on itself
    /// directly or through others.
    ///
    /// None of it stops at the first failure — see
    /// [`AppConfigError::Errors`](crate::error::AppConfigError). Which is
    /// also why a missing dependency does not prevent the cycle check: the
    /// edge is simply not added, so both kinds of problem come back together.
    pub fn new(config: &Config) -> Result<Self> {
        let unit_map = Arc::new(AppUnitMap::new(config)?);
        let schedule = AppSchedule::new(config, &unit_map)?;
        Ok(Self { unit_map, schedule })
    }

    /// Every proc as a [`Unit`](crate::unit::Unit), by its key.
    pub fn unit_map(&self) -> &Arc<AppUnitMap> {
        &self.unit_map
    }

    /// Ask for a proc to start, once what it depends on is up.
    ///
    /// Returns before any of that has happened: what actually starts it is
    /// [`run`](App::run), on its own task.
    pub fn schedule(&self, key: UnitKey) {
        self.schedule.schedule(key);
    }

    /// Stop a proc, without waiting for it to be gone.
    ///
    /// The `Err` is a key no unit was declared under, and the keys come from
    /// the map, so there is nothing for a caller to do with it.
    pub fn stop(&self, key: UnitKey) {
        _ = self.unit_map.stop(key);
    }

    /// [`schedule`](App::schedule) for every proc of a group.
    pub fn schedule_group(&self, group: &str) {
        self.schedule.schedule_group(group);
    }

    /// Drive the schedule for as long as the session lasts.
    ///
    /// Spawn it; it does not return on its own, and nothing that was
    /// scheduled starts until it is running.
    pub async fn run(&self) {
        self.schedule.run().await;
    }

    /// Stop every proc and wait until each one is really gone — see
    /// [`UnitMap::shutdown`](crate::unit::UnitMap::shutdown).
    pub async fn shutdown(&self) {
        self.unit_map.shutdown().await;
    }

    /// Hand `event` to the unit under `key` and carry out what it asks for.
    ///
    /// The behavior decides, which is why this is not two methods: an event
    /// may move a unit onto another mode before the start it also asks for,
    /// and only the behavior can do that.
    ///
    /// Carrying it out belongs here rather than in [`AppUnitMap`], which
    /// cannot reach the schedule: a start it asks for is a start in
    /// dependency order like any other.
    pub fn dispatch(&self, key: UnitKey, event: UnitEvent) -> anyhow::Result<()> {
        let Some(action) = self.unit_map().dispatch(key, event)? else {
            return Ok(());
        };
        match action {
            UnitAction::Start => {
                self.schedule(key);
                Ok(())
            }
            UnitAction::Stop => {
                self.unit_map.stop(key)?;
                Ok(())
            }
        }
    }
}
