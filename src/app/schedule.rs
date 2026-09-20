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

pub struct AppSchedule {
    unit_map: Weak<AppUnitMap>,
    scheduled: Mutex<HashMap<UnitKey, bool>>,
    event_dispatcher: EventDispatcher,
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

    pub fn schedule(&self, key: UnitKey) {
        let Some(unit_map) = self.unit_map.upgrade() else {
            return;
        };
        let chain = unit_map.resolve_dependency_chain(&[key]);
        self.schedule_inner(chain, &[key]);
    }

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

    /// Stop every proc and wait until each one is really gone — see
    /// [`UnitMap::shutdown`](crate::unit::UnitMap::shutdown).
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
            }
        }
    }
}

struct AppScheduleRunner {}
