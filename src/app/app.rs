use std::sync::Arc;

use anyhow::Result;

use crate::{
    app::{
        AppUnitKey,
        schedule::{AppSchedule, AppScheduleRunnerTask},
        unit_map::AppUnitMap,
    },
    config::{Config, ConfigUi},
    unit::{UnitAction, UnitEvent},
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
    /// What the file said about the screen, kept because a client asks the
    /// session for it — the file is read here and nowhere else.
    ui: ConfigUi,
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
        Ok(Self {
            unit_map,
            schedule,
            ui: config.ui,
        })
    }

    /// What the config said about the screen.
    pub fn ui(&self) -> ConfigUi {
        self.ui
    }

    /// Every proc as a [`Unit`](crate::unit::Unit), by its key.
    pub fn unit_map(&self) -> &Arc<AppUnitMap> {
        &self.unit_map
    }

    /// Ask for a proc to start, once what it depends on is up.
    ///
    /// Returns before any of that has happened: what actually starts it is
    /// the task from [`run_tasks`](App::run_tasks).
    pub fn schedule(&self, key: AppUnitKey) {
        self.schedule.schedule(key);
    }

    /// Stop a proc, without waiting for it to be gone.
    ///
    /// The `Err` is a key no unit was declared under, and the keys come from
    /// the map, so there is nothing for a caller to do with it.
    pub fn stop(&self, key: AppUnitKey) {
        _ = self.unit_map.stop(key);
    }

    /// [`schedule`](App::schedule) for every proc of a group.
    pub fn schedule_group(&self, group: &str) {
        self.schedule.schedule_group(group);
    }

    /// Everything this session needs running, as one thing to spawn.
    ///
    /// Nothing that is scheduled starts until it is. Built and not spawned,
    /// for a caller that wants it on a runtime of its own —
    /// [`run_tasks`](App::run_tasks) is the usual way.
    pub fn create_runner_task(&self) -> AppRunnerTask {
        AppRunnerTask {
            schedule_runner: self.schedule.create_runner_task(),
        }
    }

    /// Spawn the session's tasks, which run until the session is dropped.
    ///
    /// The handle is there for a caller that wants to abort or join them;
    /// dropping it detaches, which is what a caller that owns the [`App`] for
    /// the life of the process wants.
    pub fn run_tasks(&self) -> tokio::task::JoinHandle<()> {
        let runner = self.create_runner_task();
        tokio::spawn(runner.run())
    }

    /// Stop every proc and wait until each one is really gone — see
    /// [`AppUnitMap::shutdown`](crate::app::AppUnitMap::shutdown).
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
    pub fn dispatch(&self, key: AppUnitKey, event: UnitEvent) -> anyhow::Result<()> {
        let Some(action) = self.unit_map().entry(key)?.unit().dispatch(event) else {
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

/// The session's tasks, before they are spawned.
///
/// One task drives all of them, so a session is one spawn and one handle
/// however many loops it grows.
pub struct AppRunnerTask {
    schedule_runner: AppScheduleRunnerTask,
}

impl AppRunnerTask {
    pub async fn run(self) {
        self.schedule_runner.run().await;
    }
}
