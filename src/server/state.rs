use std::{collections::HashMap, sync::Arc};

use parking_lot::Mutex;

use crate::{
    app::{App, AppUnitKey},
    log::{LogLine, LogReader, LogRegion},
};

/// One unit's log, as the server follows it.
///
/// The reader is kept rather than built per request because a fresh one has
/// seen nothing: its first [`sync`](LogReader::sync) is the worst case the
/// module names, the whole ring copied with the log's lock held. Kept, it is
/// nearly caught up and every sync is the delta.
struct ServerLog {
    /// This server's view of that log, on behalf of every client.
    reader: LogReader,
    /// Where a region is cut into, kept so the strings are refilled rather
    /// than a pane's worth allocated per request.
    lines: Vec<LogLine>,
}

/// What a connection reads the session through.
///
/// The [`App`] answers everything except the log: states, names, modes and
/// choices are all reads off the unit map, and a command is a call. The log
/// is the exception, and the reason this type exists — see [`ServerLog`].
pub struct ServerState {
    app: Arc<App>,
    /// One slot per unit, built here and never added to: the set of units
    /// comes from a config read once, so the map itself needs no lock and
    /// only the slot does.
    ///
    /// Each reader is built on its own unit's log, and so at that log's size.
    /// One reader per unit is what makes that possible — a screen moving one
    /// reader between units has to build it at the largest of them, and
    /// `log_size` is per proc.
    logs: HashMap<AppUnitKey, Mutex<ServerLog>>,
}

impl ServerState {
    pub fn new(app: Arc<App>) -> Self {
        let unit_map = app.unit_map();
        let logs = app
            .unit_map()
            .keys()
            .filter_map(|key| {
                Some((
                    key,
                    Mutex::new(ServerLog {
                        // Unreachable: the keys came out of this same map.
                        reader: unit_map.log_reader(key)?,
                        lines: Vec::with_capacity(1024),
                    }),
                ))
            })
            .collect();
        Self { app, logs }
    }

    /// The session itself, for everything that is not a log.
    pub fn app(&self) -> &Arc<App> {
        &self.app
    }

    /// Cut `region` out of a unit's log and hand it to `read`, with the
    /// revision it was taken at.
    ///
    /// `None` for a key no unit was declared under, which is not something a
    /// caller can act on beyond answering the client that there is nothing
    /// there. A unit whose log has gone quiet is not that case: it reads as
    /// the lines already copied, which is what a view of a finished process
    /// should show.
    ///
    /// **The lines are lent and not returned.** A closure is what keeps the
    /// guard out of an `async fn`: a handler holding one across an `.await`
    /// would block every other request for that unit behind whatever it was
    /// waiting on, and this way there is no guard for it to hold.
    pub fn read_log<T>(
        &self,
        key: AppUnitKey,
        region: LogRegion,
        read: impl FnOnce(&[LogLine], u64) -> T,
    ) -> Option<T> {
        let mut slot = self.logs.get(&key)?.lock();
        let ServerLog { reader, lines } = &mut *slot;
        reader.sync();
        let revision = reader.version();
        reader.copy_region(region, lines);
        Some(read(lines, revision))
    }
}
