use std::{path::PathBuf, sync::Arc, time::Duration};

use parking_lot::Mutex;
use tokio::{sync::Notify, sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    app::AppUnitKey,
    error::ViewSocketError,
    log::{LogLine, LogRegion},
    unit::{UnitChoice, UnitEvent},
    util::str::SmallStr,
    view::{
        client::{ViewClient, ViewCommand, ViewLog, ViewSettings, ViewUnit},
        server::{ServerClient, ServerLog, ServerLogKind},
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
/// under, and the row the pane pointed at holds both.
#[derive(Clone)]
struct Wanted {
    unit_key: AppUnitKey,
    key: SmallStr,
    region: LogRegion,
}

/// One unit's log, as a frame carries it.
///
/// The key and the answer, and nothing taken apart from them: the rectangle,
/// the revision and the lines are all inside the [`ServerLog`], which is the
/// buffer [`log_in_place`](ServerClient::log_in_place) writes into. Read
/// through methods so that reading cannot take it apart — a caller that only
/// wanted to look at the region has no business owning the lines.
#[derive(Clone)]
struct FrameLog {
    /// Which unit the answer is about, and `None` for a buffer nothing has
    /// been read into yet: an [`AppUnitKey`] has no empty value, every one of
    /// them having come from the interner.
    unit_key: Option<AppUnitKey>,
    log: ServerLog,
}

impl FrameLog {
    /// An empty buffer, naming no unit.
    fn new() -> Self {
        Self {
            unit_key: None,
            log: ServerLog::default(),
        }
    }

    /// Which unit the answer inside is about.
    fn unit_key(&self) -> Option<AppUnitKey> {
        self.unit_key
    }

    /// Say which unit the next answer is about.
    ///
    /// Apart from [`server_log_mut`](FrameLog::server_log_mut) because the two
    /// are written at different moments: the key is known before the request
    /// goes out, the answer only after it comes back.
    fn set_unit_key(&mut self, unit_key: AppUnitKey) {
        self.unit_key = Some(unit_key);
    }

    /// The buffer to read the next answer into.
    fn server_log_mut(&mut self) -> &mut ServerLog {
        &mut self.log
    }

    /// Which of the three things the session last said.
    fn kind(&self) -> ServerLogKind {
        self.log.kind
    }

    /// The lines as they stand. What they are worth depends on
    /// [`kind`](FrameLog::kind) — see [`ServerLog`].
    fn lines(&self) -> &Vec<LogLine> {
        &self.log.body.lines
    }

    /// The rectangle those lines actually came out as, which can be smaller
    /// than the one asked for.
    fn region(&self) -> LogRegion {
        self.log.body.region
    }

    /// The revision they were read at, for the next `If-None-Match`.
    fn revision(&self) -> u64 {
        self.log.body.revision
    }
}

/// One poll's worth of session, published whole.
///
/// Whole is the point, and it is load bearing twice over. A pane reads the
/// units and the log in one frame, so a half written snapshot is the tear the
/// trait's `sync` exists to prevent. And the slot a frame goes into replaces
/// rather than queues — a frame nobody took is dropped — so a frame may never
/// say "keep what you have": there is no knowing what the screen has, and an
/// instruction that depends on the one before it having arrived freezes the
/// pane the first time one does not.
///
/// Which is why a `304` costs a clone of the lines here rather than a word:
/// the saving it buys is on the wire, and paying for it in the frame is what
/// went wrong.
struct Frame {
    units: Vec<ViewUnit>,
    log: Option<FrameLog>,
    choices_key: Option<AppUnitKey>,
    choices: Vec<UnitChoice>,
}

/// A command on its way out, with the encoded key its URL needs.
enum Outgoing {
    Start(SmallStr),
    Stop(SmallStr),
    Dispatch(SmallStr, UnitEvent),
}

/// What the screen and the task share, and the only thing they share.
struct Shared {
    /// The newest finished frame, waiting to be taken. Replaced and not
    /// queued: a screen that stalled wants the latest, never a backlog of
    /// frames it would draw one per tick to catch up.
    frame: Mutex<Option<Frame>>,
    /// The rectangle the pane wants, latest wins. `None` is a pane showing no
    /// unit, which is the client holding nothing for it.
    wanted: Mutex<Option<Wanted>>,
    /// Woken when the pane moves to another unit.
    ///
    /// Without it a move waits for the next tick and then for the frame after
    /// it, so the pane sits empty for up to two polls over what is one round
    /// trip on a local socket. `notify_one` and not `notify_waiters`, because
    /// a move between ticks has to keep its wakeup rather than lose it.
    moved: Notify,
}

/// A [`ViewClient`] over a session in another process.
///
/// **Nothing here waits.** The task polls on its own, publishes a whole frame
/// into [`Shared`], and [`sync`](ViewClient::sync) takes whatever is there —
/// so a slow or dead server is a screen showing its last frame, never a
/// screen that stops redrawing.
///
/// The one round trip a caller does wait on is
/// [`connect`](ViewSocket::connect): the trait promises that the set of units
/// does not change, and the only way to keep that promise is to know them
/// before the first frame.
pub struct ViewSocket {
    /// The last frame taken, which is what every read answers from.
    units: Vec<ViewUnit>,
    /// What the session said about the screen when this one attached.
    settings: ViewSettings,
    log: Option<FrameLog>,
    choices_key: Option<AppUnitKey>,
    choices: Vec<UnitChoice>,
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
        let units = client.units().await?;
        // Asked for once, here: they come from a config the session read
        // before it existed, so no later poll would ever find them changed.
        let settings = client.settings().await?;

        let shared = Arc::new(Shared {
            frame: Mutex::new(None),
            wanted: Mutex::new(None),
            moved: Notify::new(),
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
            }
            .run(),
        );

        Ok(Self {
            units,
            settings,
            log: None,
            choices_key: None,
            choices: Vec::new(),
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
    fn config_key(&self, key: AppUnitKey) -> Option<SmallStr> {
        let unit = self.units.iter().find(|unit| unit.unit_key == key)?;
        Some(unit.key.clone())
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
    /// Take the newest frame, if the task has finished one since the last.
    ///
    /// Nothing there is the normal case at any rate faster than the poll, and
    /// the answer is to keep the frame already held — which is why this
    /// cannot fail and never blanks a pane.
    fn sync(&mut self) {
        let Some(frame) = self.shared.frame.lock().take() else {
            return;
        };
        self.units = frame.units;
        self.log = frame.log;
        self.choices_key = frame.choices_key;
        self.choices = frame.choices;
    }

    fn units(&self) -> &[ViewUnit] {
        &self.units
    }

    /// What the session said when this one attached — see
    /// [`connect`](ViewSocket::connect).
    fn settings(&self) -> ViewSettings {
        self.settings
    }

    /// Whatever the last poll fetched for this unit, and nothing for any
    /// other.
    ///
    /// Empty rather than stale when the menu opens on a unit the task has not
    /// asked about yet: the trait allows filling nothing, and a menu of
    /// another unit's verbs would act on the wrong proc.
    fn choices(&self, key: AppUnitKey, out: &mut Vec<UnitChoice>) {
        out.clear();
        if self.choices_key == Some(key) {
            out.extend(self.choices.iter().cloned());
        }
    }

    /// Write it down for the task to send.
    ///
    /// Dropped on the floor when the channel is gone, which is a task that
    /// has ended — the same silence the in-process client answers a refused
    /// command with, and the same gap.
    fn send(&self, command: ViewCommand) {
        let outgoing = match command {
            ViewCommand::Start { key } => self.config_key(key).map(Outgoing::Start),
            ViewCommand::Stop { key } => self.config_key(key).map(Outgoing::Stop),
            ViewCommand::Dispatch { key, event } => self
                .config_key(key)
                .map(|key| Outgoing::Dispatch(key, event)),
        };
        if let Some(outgoing) = outgoing {
            let _ = self.commands.send(outgoing);
        }
    }

    /// Say what the pane wants; the task is what goes and gets it.
    ///
    /// **A move drops what is held.** The lines are the unit the pane just
    /// left, and there is no frame for the new one yet — drawn under the new
    /// name they are not late, they are wrong. Empty until the task answers
    /// is what [`ViewApp`](crate::view::ViewApp) does for the same reason,
    /// and the wakeup below is what keeps that gap to a round trip.
    fn set_log(&mut self, key: Option<AppUnitKey>, region: LogRegion) {
        let moved = self.log.as_ref().and_then(FrameLog::unit_key) != key;
        if moved {
            self.log = None;
        }
        let wanted = key.and_then(|unit_key| {
            Some(Wanted {
                unit_key,
                key: self.config_key(unit_key)?,
                region,
            })
        });
        *self.shared.wanted.lock() = wanted;
        if moved {
            self.shared.moved.notify_one();
        }
    }

    fn log(&self) -> Option<ViewLog<'_>> {
        let log = self.log.as_ref()?;
        Some(ViewLog {
            region: log.region(),
            revision: log.revision(),
            lines: log.lines(),
        })
    }
}

/// The half that talks, on a task of its own.
struct Poller {
    poll: Duration,
    shared: Arc<Shared>,
    incoming: mpsc::UnboundedReceiver<Outgoing>,
    cancel: CancellationToken,
    client: ServerClient,
}

impl Poller {
    /// Poll until cancelled, reconnecting for as long as it takes.
    ///
    /// A connection that fails ends that round and not the task: the session
    /// is a separate process and may be restarted under a screen that is
    /// still up. What the screen shows meanwhile is its last frame.
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
        // Held here rather than in the frame so a `304` costs a clone of what
        // is already known instead of the lines themselves crossing again.
        let mut held: Option<FrameLog> = None;
        let mut ticks = tokio::time::interval(self.poll);
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return true,
                // Same round either way: the tick is the steady rate, and the
                // move is what stops a keypress waiting for it.
                _ = self.shared.moved.notified() => {}
                _ = ticks.tick() => {}
            }
            if self.round(&mut held).await.is_err() {
                return false;
            }
        }
    }

    /// One poll: everything asked for, then one frame published.
    ///
    /// The requests go one after another on the one connection, which is what
    /// makes "one in flight" structural rather than something to remember.
    async fn round(&mut self, held: &mut Option<FrameLog>) -> Result<(), ViewSocketError> {
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

        let units = self.client.units().await?;
        let wanted = self.shared.wanted.lock().clone();

        let (log, choices_key, choices) = match wanted {
            None => {
                *held = None;
                (None, None, Vec::new())
            }
            Some(wanted) => {
                let log = self.fetch_log(&wanted, held).await?;
                let choices = self.client.choices(&wanted.key).await.unwrap_or_default();
                (log, Some(wanted.unit_key), choices)
            }
        };

        *self.shared.frame.lock() = Some(Frame {
            units,
            log,
            choices_key,
            choices,
        });
        Ok(())
    }

    /// The wanted rectangle, whether or not it had to cross the socket.
    ///
    /// The revision goes out as `If-None-Match`, which is the test
    /// [`ViewApp`](crate::view::ViewApp) does against its own reader moved
    /// onto the wire. [`Unchanged`](ServerLog::Unchanged) answers with the
    /// lines already held rather than with a word meaning "keep yours": see
    /// [`Frame`].
    async fn fetch_log(
        &mut self,
        wanted: &Wanted,
        held: &mut Option<FrameLog>,
    ) -> Result<Option<FrameLog>, ViewSocketError> {
        let same = held.as_ref().is_some_and(|log| {
            log.unit_key() == Some(wanted.unit_key) && log.region() == wanted.region
        });
        let revision = same
            .then(|| held.as_ref().map(FrameLog::revision))
            .flatten();

        let answer = self
            .client
            .log(&wanted.key, wanted.region, revision)
            .await?;
        match answer.kind {
            // The lines are already here; only the wire was spared.
            ServerLogKind::Unchanged => Ok(held.clone()),
            ServerLogKind::Gone => {
                *held = None;
                Ok(None)
            }
            ServerLogKind::Changed => {
                let mut log = FrameLog::new();
                log.set_unit_key(wanted.unit_key);
                *log.server_log_mut() = answer;
                *held = Some(log.clone());
                Ok(Some(log))
            }
        }
    }
}
