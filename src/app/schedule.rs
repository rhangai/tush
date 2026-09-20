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
/// [`run`](AppSchedule::run) re-reads it on every change to start whatever
/// has become startable since.
pub struct AppSchedule {
    /// Weakly, so both the recording and the loop give up once the map is
    /// gone rather than keeping a dead session's units reachable.
    unit_map: Weak<AppUnitMap>,
    /// What is waiting to start, and whether it was asked for directly: a
    /// direct request restarts a unit that is already running, one pulled in
    /// as a dependency leaves it alone.
    scheduled: Mutex<HashMap<UnitKey, bool>>,
    /// The units' own dispatcher, so a fresh request wakes the loop by the
    /// same route a unit resolving does.
    event_dispatcher: EventDispatcher,
    /// Taken here and not in [`run`](AppSchedule::run), which is spawned
    /// later: a listener only wakes for triggers after it was created, and a
    /// target named on the command line is scheduled before the loop is up.
    event_listener: EventListener,
}

impl AppSchedule {
    pub fn new(_config: &Config, unit_map: &Arc<AppUnitMap>) -> anyhow::Result<Self> {
        let event_dispatcher = unit_map.event_dispatcher().clone();
        let event_listener = event_dispatcher.create_listener();
        Ok(Self {
            unit_map: Arc::downgrade(unit_map),
            scheduled: Mutex::new(HashMap::new()),
            event_dispatcher,
            event_listener,
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
        let mut lock = self.scheduled.lock();
        for item in chain.order() {
            lock.entry(*item).or_insert(false);
        }
        for key in direct_keys {
            lock.insert(*key, true);
        }
        drop(lock);
        self.event_dispatcher.trigger();
    }

    /// Start whatever has become startable, until the session ends.
    ///
    /// Spawned once and awaited by nobody. Each wake re-reads the pending set
    /// against the units that have resolved, and starts every pending unit
    /// whose *direct* dependencies are all resolved; the deeper ones need no
    /// checking, since they are pending too and this is the loop that clears
    /// them.
    pub async fn run(&self) {
        let mut event_listener = self.event_listener.clone();
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
                scheduled.extend(self.scheduled.lock().iter());
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
                let mut lock = self.scheduled.lock();
                for remove in &remove {
                    lock.remove(remove);
                }
                remove.clear();
            }
        }
    }
}
