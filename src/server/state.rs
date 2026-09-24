use std::{collections::HashMap, sync::Arc};

use hyper::StatusCode;
use parking_lot::Mutex;
use serde::Serialize;
use tokio_util::bytes::Bytes;

use crate::{
    app::{App, AppUnitKey},
    log::{LogLine, LogReader, LogRegion},
    runner::RunnerState,
    unit::UnitChoices,
    util::bytes::{BytesMutSyncPool, BytesReusable},
    view::ViewUnit,
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

/// The rows, as the last request left them.
///
/// Built once and refreshed in place: the set of units, their keys, names and
/// panels come from a config read before the session existed, so a request has
/// only three fields per row to look at. What that buys is the revision — a
/// refresh that finds nothing different leaves it alone, and the route answers
/// `304` instead of the rows.
struct ServerUnits {
    /// In the order they are drawn in, settled at build time: a list that
    /// resequenced itself would move the row out from under the cursor.
    rows: Vec<ViewUnit>,
    /// Bumped only when a refresh finds a row different, which is what makes
    /// it comparable against an `If-None-Match`.
    revision: u64,
    /// The rows as JSON, rebuilt when the revision moves and cloned per
    /// answer — a [`Bytes`] clone being a refcount rather than a copy.
    body: BytesReusable,
}

/// What a connection reads the session through.
///
/// The [`App`] answers everything except the log: states, names, modes and
/// choices are all reads off the unit map, and a command is a call. The log
/// is the exception, and the reason this type exists — see [`ServerLog`].
pub struct ServerState {
    app: Arc<App>,
    /// The rows every client polls for, behind one lock: they are the same
    /// rows for everybody, unlike a log, which is followed per unit.
    units: Mutex<ServerUnits>,
    /// One slot per unit, built here and never added to: the set of units
    /// comes from a config read once, so the map itself needs no lock and
    /// only the slot does.
    ///
    /// Each reader is built on its own unit's log, and so at that log's size.
    /// One reader per unit is what makes that possible — a screen moving one
    /// reader between units has to build it at the largest of them, and
    /// `log_size` is per proc.
    logs: HashMap<AppUnitKey, Mutex<ServerLog>>,
    /// Where the short pieces of an answer are written: an `ETag`, and
    /// anything else of that size.
    bytes_pool: BytesMutSyncPool,
    /// Where a body is serialized. Fewer slots and far more room each, a body
    /// being the longest thing written here.
    bytes_json_pool: BytesMutSyncPool,
}

/// What the log route has to answer with.
///
/// Four answers and not a `Result`, because two of them are not failures: a
/// caller whose copy is current and a key nothing is declared under are both
/// things the session has to say, and collapsing either into an error moves
/// the decision back to the handler.
pub enum ServerLogResult<E> {
    /// The revision the client sent is the one the log is still at.
    NotModified,
    /// No unit under that key.
    NotFound,
    /// The body as `read` wrote it, and the revision it was cut at.
    Read(Bytes, u64),
    /// `read` itself failed, with whatever it failed with.
    Error(E),
}

/// What the units route has to answer with. No `NotFound`: the rows are the
/// whole session, and there is always a session.
pub enum ServerUnitResult {
    NotModified,
    Read(Bytes, u64),
}

impl ServerState {
    pub fn new(app: Arc<App>) -> Self {
        let unit_map = app.unit_map();
        let logs = app
            .unit_map()
            .entries()
            .map(|(key, entry)| {
                (
                    key,
                    Mutex::new(ServerLog {
                        reader: entry.log_reader(),
                        lines: Vec::with_capacity(1024),
                    }),
                )
            })
            .collect();

        let mut rows: Vec<ViewUnit> = unit_map
            .entries()
            .map(|(unit_key, entry)| {
                let unit = entry.unit();
                ViewUnit {
                    unit_key,
                    key: entry.key().clone(),
                    name: unit.name(),
                    name_short: unit.name_short(),
                    // Read here only to have something to write over; the
                    // first refresh is what makes them true.
                    mode: None,
                    mode_short: None,
                    state: RunnerState::Stopped,
                    panel: entry.settings().panel,
                }
            })
            .collect();
        rows.sort_by(|a, b| (a.panel, &a.name).cmp(&(b.panel, &b.name)));

        Self {
            app,
            units: Mutex::new(ServerUnits {
                rows,
                // Zero is what no client can be holding: a revision only
                // reaches one by having been answered with, and the first
                // refresh moves it before anything is sent.
                revision: 0,
                body: BytesReusable::with_capacity(1024),
            }),
            logs,
            bytes_pool: BytesMutSyncPool::with_capacity(8, 256),
            bytes_json_pool: BytesMutSyncPool::with_capacity(4, 8192),
        }
    }

    /// Refresh the rows, and answer with them and the revision they are at.
    ///
    /// The three fields a run changes are compared rather than written over:
    /// the comparison is what the whole route rests on, since equal rows mean
    /// the body already sent is still current and the client can be told so in
    /// sixty six bytes.
    ///
    /// The body goes back as a [`Bytes`] and the guard ends here, which is
    /// what keeps it out of the handler's `async fn`: one holding the rows
    /// every client shares across an `.await` would queue every other poll
    /// behind itself.
    pub fn read_units(&self, last_revision: Option<u64>) -> ServerUnitResult {
        let unit_map = self.app.unit_map();
        let mut slot = self.units.lock();

        let mut changed = false;
        for row in &mut slot.rows {
            // Unreachable: the keys came out of this same map.
            let Ok(entry) = unit_map.entry(row.unit_key) else {
                continue;
            };
            let unit = entry.unit();
            let state = unit.state();
            let mode = unit.mode();
            let mode_short = unit.mode_short();
            if row.state != state || row.mode != mode || row.mode_short != mode_short {
                row.state = state;
                row.mode = mode;
                row.mode_short = mode_short;
                changed = true;
            }
        }

        // Also on the first call, where the body is empty and no revision has
        // been answered with yet.
        if changed || slot.body.is_empty() {
            slot.revision += 1;
            let ServerUnits { rows, body, .. } = &mut *slot;
            body.json(rows);
            return ServerUnitResult::Read(body.bytes().clone(), slot.revision);
        }
        if Some(slot.revision) == last_revision {
            return ServerUnitResult::NotModified;
        }
        ServerUnitResult::Read(slot.body.bytes().clone(), slot.revision)
    }

    /// Everything that can be asked of one unit, as JSON.
    ///
    /// Built per request and not kept like the rows: a menu opens on a
    /// keypress rather than on a clock, and the revision is taken only
    /// because the route carries one — nothing here versions a list.
    pub fn choices(
        &self,
        key: AppUnitKey,
        _last_revision: Option<u64>,
    ) -> Result<Bytes, StatusCode> {
        let unit_map = self.app.unit_map();
        let entry = unit_map.entry(key).map_err(|_| StatusCode::NOT_FOUND)?;
        let unit = entry.unit();
        let mut choices = UnitChoices::new();
        unit.choices(&mut choices);
        self.bytes_json_pool
            .json(&choices)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    }

    /// The session itself, for everything that is not a log.
    pub fn app(&self) -> &Arc<App> {
        &self.app
    }

    /// The [`AppUnitKey`] a config key was interned under, or nothing for a key no
    /// proc was declared with.
    pub fn key(&self, key: &str) -> Option<AppUnitKey> {
        self.app.unit_map().key(key)
    }

    /// Cut `region` out of a unit's log and hand it to `read`, with the
    /// revision it was taken at.
    ///
    /// [`NotFound`](ServerLogResult::NotFound) for a key no unit was declared
    /// under, which is not something a caller can act on beyond answering the
    /// client that there is nothing there. A unit whose log has gone quiet is
    /// not that case: it reads as the lines already copied, which is what a
    /// view of a finished process should show.
    ///
    /// **The lines are lent and not returned.** A closure is what keeps the
    /// guard out of an `async fn`: a handler holding one across an `.await`
    /// would block every other request for that unit behind whatever it was
    /// waiting on, and this way there is no guard for it to hold.
    pub fn read_log<E>(
        &self,
        key: AppUnitKey,
        last_revision: Option<u64>,
        region: LogRegion,
        read: impl FnOnce(&[LogLine], u64) -> Result<Bytes, E>,
    ) -> ServerLogResult<E> {
        let Some(entry) = self.logs.get(&key) else {
            return ServerLogResult::NotFound;
        };
        let mut slot = entry.lock();
        let ServerLog { reader, lines } = &mut *slot;
        reader.sync();
        let revision = reader.version();
        if Some(revision) == last_revision {
            return ServerLogResult::NotModified;
        }
        reader.copy_region(region, lines);
        match read(lines, revision) {
            Err(err) => ServerLogResult::Error(err),
            Ok(data) => ServerLogResult::Read(data, revision),
        }
    }

    /// One short piece of an answer — an `ETag` — out of the pool it shares
    /// with every other request.
    pub fn write(&self, args: std::fmt::Arguments<'_>) -> Bytes {
        self.bytes_pool.write(args)
    }

    /// One body, serialized out of the pool sized for bodies.
    pub fn json<T: ?Sized + Serialize>(&self, data: &T) -> Result<Bytes, serde_json::Error> {
        self.bytes_json_pool.json(data)
    }
}
