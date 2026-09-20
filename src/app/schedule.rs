use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Weak},
};

use parking_lot::Mutex;

use crate::{
    app::unit_map::AppUnitMap,
    config::Config,
    unit::UnitKey,
    util::{
        event::{EventDispatcher, EventListener},
        graph::DependencyOrder,
    },
};

/// What turns "start this" into starts in dependency order.
///
/// A unit whose dependencies are not up yet cannot simply be started, and
/// nothing here can know when they will be: the answer arrives later, on
/// another task, as a unit resolving. So a request is recorded instead, and
/// [`AppScheduleRunnerTask`] re-reads it on every change to start whatever
/// has become startable since.
pub struct AppSchedule {
    /// Weakly, so recording a request against a session that is gone is a
    /// no-op rather than a way to keep its units reachable.
    unit_map: Weak<AppUnitMap>,
    /// The units' own dispatcher, so a fresh request wakes the loop by the
    /// same route a unit resolving does.
    event_dispatcher: EventDispatcher,
    /// What the loop shares with this: separate so the loop can hold it
    /// without holding the schedule, and so be dropped independently.
    inner: Arc<AppScheduleInner>,
}

impl AppSchedule {
    pub fn new(_config: &Config, unit_map: &Arc<AppUnitMap>) -> anyhow::Result<Self> {
        let event_dispatcher = unit_map.event_dispatcher().clone();
        let event_listener = event_dispatcher.create_listener();
        let inner = Arc::new(AppScheduleInner {
            event_listener,
            scheduled: Mutex::new(HashMap::new()),
        });
        Ok(Self {
            unit_map: Arc::downgrade(unit_map),
            event_dispatcher,
            inner,
        })
    }

    /// Ask for `key` to start, once what it depends on has.
    pub fn schedule(&self, key: UnitKey) {
        let Some(unit_map) = self.unit_map.upgrade() else {
            return;
        };
        let chain = unit_map.resolve_dependency_chain(&[key]);
        self.schedule_inner(chain, &[key]);
    }

    /// The same for every unit of a group, or nothing if no group is named
    /// `group` — an unknown group is a target that matched nothing, not a
    /// failure.
    pub fn schedule_group(&self, group: &str) {
        let Some(unit_map) = self.unit_map.upgrade() else {
            return;
        };
        let Some(group) = unit_map.group(group) else {
            return;
        };
        let chain = unit_map.resolve_dependency_chain(group);
        self.schedule_inner(chain, group);
    }

    /// Record the chain as wanted, then the roots as wanted directly.
    ///
    /// In that order, because a key can be both — named by the caller and
    /// reached again as something else's dependency — and being named is what
    /// decides whether it gets restarted.
    fn schedule_inner(&self, chain: DependencyOrder<UnitKey>, direct_keys: &[UnitKey]) {
        let mut lock = self.inner.scheduled.lock();
        for item in chain.order() {
            lock.entry(*item).or_insert(false);
        }
        for key in direct_keys {
            lock.insert(*key, true);
        }
        drop(lock);
        self.event_dispatcher.trigger();
    }

    /// The loop half, for whoever spawns the session's tasks.
    ///
    /// Built rather than spawned here, so the caller decides when it starts
    /// and what it is joined to.
    pub fn create_runner_task(&self) -> AppScheduleRunnerTask {
        AppScheduleRunnerTask {
            unit_map: self.unit_map.clone(),
            inner: Arc::downgrade(&self.inner),
        }
    }
}

/// The loop of an [`AppSchedule`], as something spawnable on its own.
///
/// Split off so the task holds neither the schedule nor the session: both
/// ends are weak, and what is left is a task that cannot keep alive the thing
/// it exists to serve.
pub struct AppScheduleRunnerTask {
    /// Checked on every wake, and what ends the loop: the session is gone, so
    /// there is nothing left to start.
    unit_map: Weak<AppUnitMap>,
    /// Checked once, before the first wake, and held strongly from then on:
    /// this is what stops a task built for a session that died before it was
    /// spawned from ever starting. It is not the loop's exit — `unit_map` is.
    inner: Weak<AppScheduleInner>,
}

impl AppScheduleRunnerTask {
    /// Start whatever has become startable, until the session ends.
    ///
    /// Spawned once and awaited by nobody. Each wake re-reads the pending set
    /// against the units that have resolved, and starts every pending unit
    /// whose *direct* dependencies are all resolved; the deeper ones need no
    /// checking, since they are pending too and this is the loop that clears
    /// them.
    pub async fn run(self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut event_listener = inner.event_listener.clone();
        let mut scheduled: HashMap<UnitKey, bool> = HashMap::new();
        let mut resolved: HashSet<UnitKey> = HashSet::new();
        let mut remove: HashSet<UnitKey> = HashSet::new();
        while event_listener.changed().await {
            //
            let Some(unit_map) = self.unit_map.upgrade() else {
                break;
            };
            {
                scheduled.clear();
                scheduled.extend(inner.scheduled.lock().iter());
            }
            unit_map.write_resolved(&mut resolved);
            for (scheduled, force) in &scheduled {
                let deps = unit_map.direct_dependencies(*scheduled);
                let all_resolved = deps.into_iter().flatten().all(|dep| resolved.contains(dep));
                if all_resolved {
                    if *force {
                        _ = unit_map.start(*scheduled);
                    } else {
                        _ = unit_map.ensure_started(*scheduled);
                    }
                    remove.insert(*scheduled);
                }
            }

            if !remove.is_empty() {
                let mut lock = inner.scheduled.lock();
                for remove_item in &remove {
                    lock.remove(remove_item);
                }
                drop(lock);
                remove.clear();
            }
        }
    }
}

/// The pending starts and the way to be woken about them — what the schedule
/// and its loop both need, and the only thing they share.
struct AppScheduleInner {
    /// What is waiting to start, and whether it was asked for directly: a
    /// direct request restarts a unit that is already running, one pulled in
    /// as a dependency leaves it alone.
    scheduled: Mutex<HashMap<UnitKey, bool>>,
    /// Taken here and not in [`run`](AppScheduleRunnerTask::run), which is
    /// spawned later: a listener only wakes for triggers after it was
    /// created, and a target named on the command line is scheduled before
    /// the loop is up.
    event_listener: EventListener,
}
