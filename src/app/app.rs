use std::sync::Arc;

use anyhow::Result;

use crate::{
    app::{schedule::AppSchedule, unit_map::AppUnitMap},
    config::Config,
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
        Ok(Self { unit_map })
    }

    /// Every proc as a [`Unit`](crate::unit::Unit), by its key.
    pub fn unit_map(&self) -> &Arc<AppUnitMap> {
        &self.unit_map
    }

    /// Stop every proc and wait until each one is really gone — see
    /// [`UnitMap::shutdown`](crate::unit::UnitMap::shutdown).
    pub async fn shutdown(&self) {
        self.unit_map.shutdown().await;
    }
}
