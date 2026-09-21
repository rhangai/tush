use std::{collections::HashMap, sync::Arc};

use parking_lot::Mutex;

use crate::{
    app::App,
    log::{LogLine, LogReader, LogRegion},
    unit::UnitKey,
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
    /// Each reader starts [detached](LogReader::is_detached) and is put on its
    /// unit's log by the first read of it. The memory is taken here either
    /// way, a reader being the log's size over again; only the attach waits.
    logs: HashMap<UnitKey, Mutex<ServerLog>>,
}

impl ServerState {
    pub fn new(app: Arc<App>) -> Self {
        let unit_map = app.unit_map();
        let logs = app
            .unit_map()
            .keys()
            .map(|key| {
                (
                    key,
                    Mutex::new(ServerLog {
                        reader: LogReader::empty(unit_map.log_capacity()),
                        lines: Vec::with_capacity(1024),
                    }),
                )
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
        key: UnitKey,
        region: LogRegion,
        read: impl FnOnce(&[LogLine], u64) -> T,
    ) -> Option<T> {
        let mut slot = self.logs.get(&key)?.lock();
        if slot.reader.is_detached() {
            self.app.unit_map().log_reader_into(key, &mut slot.reader);
        }
        let ServerLog { reader, lines } = &mut *slot;
        reader.sync();
        let revision = reader.version();
        reader.copy_region(region, lines);
        Some(read(lines, revision))
    }
}
