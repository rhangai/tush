use std::{mem, path::PathBuf, sync::Arc, time::Duration};

use parking_lot::Mutex;
use tokio::{sync::Notify, sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    app::AppUnitKey,
    error::ViewSocketError,
    log::LogRegion,
    unit::{UnitChoices, UnitEvent},
    util::str::SmallStr,
    view::{
        client::{ViewClient, ViewCommand, ViewLog, ViewSettings, ViewUnit},
        server::{ServerClient, ServerLog, ServerLogBody, ServerLogKind},
    },
};

/// How long to wait before reaching for a server that went away.
///
/// A session outliving its screen is the normal case here, so a lost
/// connection is something to sit through rather than to fail on. Long enough
/// not to spin, short enough that a restarted server is picked up before
/// anybody reaches for the keyboard.
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// What the log pane asked for, as the task reads it.
///
/// The config key rides along so the task never has to turn an [`AppUnitKey`]
/// back into one: the client addresses a unit by the key it was declared
/// under, and the row the pane pointed at holds both. A [`String`] and not a
/// [`SmallStr`], because the screen writes it again on every move and only a
/// [`String`] can be refilled.
struct Wanted {
    /// The unit the pane is showing, `None` for a pane showing none.
    unit_key: Option<AppUnitKey>,
    /// Its config key, left as it was rather than emptied when there is no
    /// unit: what it holds is an allocation, and nothing reads it.
    key: String,
    region: LogRegion,
}

/// The rows, and the flag that says whose turn it is to hold them.
///
/// Read every poll and not once at connect: the set of units is fixed, but the
/// state and the mode in each row are what the pane draws, and those change
/// under a running session. The fixed half costs nothing, the answer decoding
/// over the same slots.
struct DataUnits {
    rows: Vec<ViewUnit>,
    /// Set by the task when it swaps a buffer in, cleared by the screen when
    /// it takes one. Only means anything in [`Shared`]: the screen's own copy
    /// and the task's scratch both ignore theirs.
    fresh: bool,
}

impl DataUnits {
    fn new(rows: Vec<ViewUnit>) -> Self {
        Self { rows, fresh: false }
    }
}

/// One window of one log, as it is handed over.
struct DataLog {
    /// Which unit the lines are of: `None` both for a unit the session has no
    /// log under and for a buffer nothing has been read into yet.
    unit_key: Option<AppUnitKey>,
    /// The rectangle, the revision and the lines. Stale whenever `unit_key` is
    /// `None`, which is the point of it — the allocation outlives what it
    /// held, and the next answer is decoded over it.
    body: ServerLogBody,
    /// See [`DataUnits::fresh`]. Set only for an answer that carries lines: a
    /// `304` says the screen's copy stands, and a swap has no way to say that.
    fresh: bool,
}

impl DataLog {
    /// A buffer nothing has been read into yet.
    ///
    /// The body comes from [`ServerLog`]'s default, which is where "not asked
    /// yet" is spelled — an empty rectangle at revision zero.
    fn new() -> Self {
        Self {
            unit_key: None,
            body: ServerLog::default().body,
            fresh: false,
        }
    }
}

/// The verbs of one unit, replaced whole every round.
struct DataChoices {
    /// Which unit they are for, so a menu opened over another one shows
    /// nothing rather than the wrong verbs.
    unit_key: Option<AppUnitKey>,
    list: UnitChoices,
}

/// What the screen and the task share, and the only thing they share.
///
/// Every slot here is a handoff and not a copy: the task fills a buffer of its
/// own, swaps it in, and carries away the one the screen left to fill next
/// round. The same few buffers circulate for as long as the screen is up: a
/// sync is two swaps, and what the task reads next round it decodes over what
/// it was handed. What this replaced built a whole frame per poll and dropped
/// it on the next one.
struct Shared {
    /// The rows, as the last poll found them.
    units: Mutex<DataUnits>,
    /// The window of the unit the pane is on.
    log: Mutex<DataLog>,
    /// Its verbs, read out of here directly: a menu opens on a keypress, so it
    /// can pay for a copy and needs no buffer of its own.
    choices: Mutex<DataChoices>,
    /// The rectangle the pane wants, latest wins.
    wanted: Mutex<Wanted>,
    /// Woken when the pane moves to another unit.
    ///
    /// Without it a move waits for the next tick and then for the round after
    /// it, so the pane sits empty for up to two polls over what is one round
    /// trip on a local socket. `notify_one` and not `notify_waiters`, because
    /// a move between ticks has to keep its wakeup rather than lose it.
    wanted_notify: Notify,
}

/// A [`ViewClient`] over a session in another process.
///
/// **Nothing here waits.** The task polls on its own and swaps what it read
/// into [`Shared`]; [`sync`](ViewClient::sync) swaps it back out — so a slow or
/// dead server is a screen showing what it last took, never a screen that
/// stops redrawing.
///
/// The one round trip a caller does wait on is
/// [`connect`](ViewSocket::connect): the trait promises that the set of units
/// does not change, and the only way to keep that promise is to know them
/// before the first frame.
pub struct ViewSocket {
    /// The rows the pane draws, traded for the task's at each sync.
    units: DataUnits,
    /// What the session said about the screen when this one attached.
    settings: ViewSettings,
    /// The window the pane draws, on the same terms.
    log: DataLog,
    /// The unit last asked for, which is what says whether
    /// [`log`](ViewClient::log) is about the one on screen.
    wanted_key: Option<AppUnitKey>,
    shared: Arc<Shared>,
    commands: mpsc::UnboundedSender<Outgoing>,
    /// Ends the task; see [`Drop`](ViewSocket::drop).
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl ViewSocket {
    /// Attach to the session listening on `path`, and start following it.
    ///
    /// The units are read here rather than waited for, so a screen is never
    /// drawn against a session it has not met — and a socket with nothing on
    /// the other end fails here, where there is still a terminal to print to.
    pub async fn connect(path: PathBuf, poll: Duration) -> Result<Self, ViewSocketError> {
        let mut client = ServerClient::connect(&path).await?;
        let rows = client.units().await?;
        // Asked for once, here: they come from a config the session read
        // before it existed, so no later poll would ever find them changed.
        let settings = client.settings().await?;

        let shared = Arc::new(Shared {
            units: Mutex::new(DataUnits::new(Vec::new())),
            log: Mutex::new(DataLog::new()),
            choices: Mutex::new(DataChoices {
                unit_key: None,
                list: UnitChoices::new(),
            }),
            wanted: Mutex::new(Wanted {
                unit_key: None,
                key: String::new(),
                region: LogRegion::new(0..0, 0..0),
            }),
            wanted_notify: Notify::new(),
        });
        let (commands, incoming) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(
            Poller {
                poll,
                shared: shared.clone(),
                incoming,
                cancel: cancel.clone(),
                client,
                units: DataUnits::new(Vec::new()),
                choices: UnitChoices::new(),
                log: ServerLog::default(),
                unit_key: None,
                key: SmallStr::default(),
                sent: None,
            }
            .run(),
        );

        Ok(Self {
            units: DataUnits::new(rows),
            settings,
            log: DataLog::new(),
            wanted_key: None,
            shared,
            commands,
            cancel,
            task: Some(task),
        })
    }

    /// The config key an [`AppUnitKey`] stands for.
    ///
    /// The key and not [`name`](ViewUnit::name): the server resolves a unit
    /// through the interner, and the interner only ever saw the key. Handing
    /// it over raw is deliberate — turning one into a path segment belongs to
    /// [`ServerClient`], which is the only thing that knows what a route
    /// looks like.
    ///
    /// A scan and not a map: the rows are a session's worth of units, in
    /// name order, and this happens on a keypress rather than on a frame.
    fn config_key(&self, key: AppUnitKey) -> Option<&SmallStr> {
        let unit = self.units.rows.iter().find(|unit| unit.unit_key == key)?;
        Some(&unit.key)
    }
}

impl Drop for ViewSocket {
    /// Take the task with the client.
    ///
    /// Cancelled and not just dropped: the task owns the connection, and a
    /// detached one would go on polling a session nothing is showing.
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl ViewClient for ViewSocket {
    /// Trade buffers with the task, for whichever of them it has finished.
    ///
    /// Two locks, two flags and at most two swaps — and nothing at all when
    /// the task has not been round since, which at any frame rate above the
    /// poll is most of them.
    fn sync(&mut self) {
        let mut units = self.shared.units.lock();
        if units.fresh {
            mem::swap(&mut *units, &mut self.units);
            units.fresh = false;
        }
        drop(units);

        let mut log = self.shared.log.lock();
        if log.fresh {
            mem::swap(&mut *log, &mut self.log);
            log.fresh = false;
        }
    }

    fn units(&self) -> &[ViewUnit] {
        &self.units.rows
    }

    /// What the session said when this one attached — see
    /// [`connect`](ViewSocket::connect).
    fn settings(&self) -> ViewSettings {
        self.settings
    }

    /// Whatever the last poll fetched for this unit, and nothing for any
    /// other.
    ///
    /// Read out of the shared slot rather than from a copy of it, and the
    /// clone is what that costs: a menu opens on a keypress, so there is
    /// nothing here worth a buffer of its own.
    fn choices(&self, key: AppUnitKey, out: &mut UnitChoices) {
        out.clear();
        let choices = self.shared.choices.lock();
        if choices.unit_key == Some(key) {
            out.extend(choices.list.iter().cloned());
        }
    }

    /// Write it down for the task to send.
    ///
    /// Dropped on the floor when the channel is gone, which is a task that
    /// has ended — the same silence the in-process client answers a refused
    /// command with, and the same gap.
    fn send(&self, command: ViewCommand) {
        let outgoing = match command {
            ViewCommand::Start { key } => self.config_key(key).cloned().map(Outgoing::Start),
            ViewCommand::Stop { key } => self.config_key(key).cloned().map(Outgoing::Stop),
            ViewCommand::Dispatch { key, event } => self
                .config_key(key)
                .cloned()
                .map(|key| Outgoing::Dispatch(key, event)),
        };
        if let Some(outgoing) = outgoing {
            let _ = self.commands.send(outgoing);
        }
    }

    /// Say what the pane wants; the task is what goes and gets it.
    ///
    /// Written into the slot the task reads, refilling the key in place: this
    /// is said again on every move and every resize.
    ///
    /// **A move blanks nothing.** What is held stays held and
    /// [`log`](ViewClient::log) answers `None` until the new unit's window
    /// turns up — the lines are the unit the pane just left, and drawn under
    /// the new name they are not late, they are wrong. Empty until the task
    /// answers is what [`ViewApp`](crate::view::ViewApp) does for the same
    /// reason, and the wakeup below is what keeps that gap to a round trip.
    fn set_log(&mut self, key: Option<AppUnitKey>, region: LogRegion) {
        let mut wanted = self.shared.wanted.lock();
        let moved = wanted.unit_key != key;
        if moved {
            wanted.unit_key = key;
            wanted.key.clear();
            if let Some(config_key) = key.and_then(|key| self.config_key(key)) {
                wanted.key.push_str(config_key);
            }
        }
        wanted.region = region;
        drop(wanted);

        self.wanted_key = key;
        if moved {
            self.shared.wanted_notify.notify_one();
        }
    }

    /// The lines, when they are the ones the pane is showing.
    ///
    /// The comparison is what trading buffers costs: a sync takes whatever is
    /// in the slot so that the task gets one back, and what the task published
    /// may be the unit the pane has just left.
    fn log(&self) -> Option<ViewLog<'_>> {
        let unit_key = self.log.unit_key?;
        if Some(unit_key) != self.wanted_key {
            return None;
        }
        Some(ViewLog {
            region: self.log.body.region,
            revision: self.log.body.revision,
            lines: &self.log.body.lines,
        })
    }
}

/// A command on its way out, with the key the config declared its unit
/// under. Encoding that into a path segment belongs to [`ServerClient`],
/// which is the only thing that knows what a route looks like.
enum Outgoing {
    Start(SmallStr),
    Stop(SmallStr),
    Dispatch(SmallStr, UnitEvent),
}

/// The window last handed over: what a `304` would be answering about.
#[derive(Clone, Copy)]
struct Sent {
    unit_key: AppUnitKey,
    /// The rectangle asked for, and not the one that came back — a log with
    /// fewer lines than the pane answers with fewer, and it is the question a
    /// revision is compared against.
    region: LogRegion,
    revision: u64,
}

/// The half that talks, on a task of its own.
struct Poller {
    poll: Duration,
    shared: Arc<Shared>,
    incoming: mpsc::UnboundedReceiver<Outgoing>,
    cancel: CancellationToken,
    client: ServerClient,
    /// The rows to fill next, which is the buffer the screen left behind.
    units: DataUnits,
    /// The verbs to fill next, on the same terms.
    choices: UnitChoices,
    /// The window to read into.
    ///
    /// Scratch and not a record: after a handover it holds whatever the screen
    /// left, so nothing may be read back out of it — what was published is in
    /// [`sent`](Poller::sent). Being written over is the whole reason it is
    /// kept between rounds.
    log: ServerLog,
    /// The unit being followed, and the key its routes take.
    ///
    /// Copied out of [`Wanted`] on a move rather than per round: the screen
    /// writes the key as a [`String`] so it can refill one, and the routes
    /// take the crate's own string.
    unit_key: Option<AppUnitKey>,
    key: SmallStr,
    /// The window the screen holds, or will hold at its next sync.
    ///
    /// Here and not in the buffer, because `If-None-Match` has to name what is
    /// on screen: the buffer that answer came in belongs to the screen from
    /// the moment it is swapped in.
    sent: Option<Sent>,
}

impl Poller {
    /// Poll until cancelled, reconnecting for as long as it takes.
    ///
    /// A connection that fails ends that round and not the task: the session
    /// is a separate process and may be restarted under a screen that is
    /// still up. What the screen shows meanwhile is what it last took.
    async fn run(mut self) {
        loop {
            let is_connected = self.client.ensure_connected().await.is_ok();
            if is_connected && self.rounds().await {
                return;
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(RECONNECT_DELAY) => {}
            }
            _ = self.client.reconnect().await;
        }
    }

    /// Poll on one connection until it fails; `true` if it was cancelled.
    async fn rounds(&mut self) -> bool {
        let mut ticks = tokio::time::interval(self.poll);
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return true,
                // Same round either way: the tick is the steady rate, and the
                // move is what stops a keypress waiting for it.
                _ = self.shared.wanted_notify.notified() => {}
                _ = ticks.tick() => {}
            }
            if self.round().await.is_err() {
                return false;
            }
        }
    }

    /// One poll: everything asked for, each handed over as it arrives.
    ///
    /// The requests go one after another on the one connection, which is what
    /// makes "one in flight" structural rather than something to remember.
    async fn round(&mut self) -> Result<(), ViewSocketError> {
        while let Ok(outgoing) = self.incoming.try_recv() {
            // The status is dropped: a command is fire and forget from here,
            // and a `404` off a row that has since gone is not a reason to
            // throw the connection away. The `?` is the connection itself.
            let _ = match outgoing {
                Outgoing::Start(key) => self.client.start(&key).await?,
                Outgoing::Stop(key) => self.client.stop(&key).await?,
                Outgoing::Dispatch(key, event) => self.client.dispatch(&key, event).await?,
            };
        }

        self.client.units_in_place(&mut self.units.rows).await?;
        // Scoped, and not dropped by hand: a guard whose scope reaches an
        // await makes the whole task's future non-`Send`, whether or not it is
        // alive by then.
        {
            let mut slot = self.shared.units.lock();
            mem::swap(&mut *slot, &mut self.units);
            slot.fresh = true;
        }

        // A pane showing nothing leaves both of the unit's slots alone: what
        // is in them is a buffer each, and nothing reads either until the pane
        // asks again.
        let Some((unit_key, region)) = self.follow() else {
            return Ok(());
        };
        self.fetch_log(unit_key, region).await?;
        self.fetch_choices(unit_key).await;
        Ok(())
    }

    /// What the pane wants now, and the key to ask for it with.
    ///
    /// Under the lock for three fields and no longer; the key is copied out of
    /// it only when the unit changed.
    fn follow(&mut self) -> Option<(AppUnitKey, LogRegion)> {
        let wanted = self.shared.wanted.lock();
        let unit_key = wanted.unit_key?;
        if self.unit_key != Some(unit_key) {
            self.unit_key = Some(unit_key);
            self.key = SmallStr::new(&wanted.key);
        }
        Some((unit_key, wanted.region))
    }

    /// Read the wanted rectangle, and hand it over if it changed.
    ///
    /// The revision goes out as `If-None-Match` only when the question is the
    /// one the last answer came back to: the server compares it against the
    /// log's version, so it means "still the same lines" and nothing about
    /// which lines were asked for.
    ///
    /// The three answers are three different handovers — lines swapped in,
    /// nothing said, and the window dropped — and only the first may touch the
    /// buffers.
    async fn fetch_log(
        &mut self,
        unit_key: AppUnitKey,
        region: LogRegion,
    ) -> Result<(), ViewSocketError> {
        let revision = match self.sent {
            Some(sent) if sent.unit_key == unit_key && sent.region == region => Some(sent.revision),
            _ => None,
        };

        // Split, so that the client and the buffer are two borrows of `self`
        // and not one.
        let Self {
            client, log, key, ..
        } = self;
        client.log_in_place(log, key, region, revision).await?;

        match self.log.kind {
            // What the screen has is current, and a swap cannot say so.
            ServerLogKind::Unchanged => {}
            ServerLogKind::Changed => {
                self.sent = Some(Sent {
                    unit_key,
                    region,
                    revision: self.log.body.revision,
                });
                let mut slot = self.shared.log.lock();
                mem::swap(&mut slot.body, &mut self.log.body);
                slot.unit_key = Some(unit_key);
                slot.fresh = true;
            }
            // Nothing to show, and nothing left to answer a `304` from. The
            // body goes over as it stands: with no unit named, it is an
            // allocation and not a window.
            ServerLogKind::Gone => {
                self.sent = None;
                let mut slot = self.shared.log.lock();
                slot.unit_key = None;
                slot.fresh = true;
            }
        }
        Ok(())
    }

    /// Read the unit's verbs and hand them over.
    ///
    /// A failure leaves the menu empty rather than ending the round: what it
    /// usually means is a row that has gone, and if it was the connection then
    /// the next round's first request is where that is found out.
    async fn fetch_choices(&mut self, unit_key: AppUnitKey) {
        let Self {
            client,
            choices,
            key,
            ..
        } = self;
        if client.choices_in_place(choices, key).await.is_err() {
            choices.clear();
        }
        let mut slot = self.shared.choices.lock();
        mem::swap(&mut slot.list, &mut self.choices);
        slot.unit_key = Some(unit_key);
    }
}
