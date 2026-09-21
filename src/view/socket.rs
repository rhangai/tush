use std::{path::PathBuf, sync::Arc, time::Duration};

use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode, body::Bytes, client::conn::http1::SendRequest, header};
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use tokio::{net::UnixStream, sync::Notify, sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    error::ViewSocketError,
    log::{LogLine, LogRegion},
    unit::{UnitChoice, UnitEvent, UnitKey},
    util::str::SmallStr,
    view::client::{ViewClient, ViewCommand, ViewLog, ViewUnit},
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
/// The name rides along so the task never has to turn a key back into one: a
/// URL addresses a unit by name, and the row the pane pointed at holds both.
#[derive(Clone)]
struct Wanted {
    unit_key: UnitKey,
    name: SmallStr,
    region: LogRegion,
}

/// One unit's log, as a frame carries it.
#[derive(Clone)]
struct FrameLog {
    unit_key: UnitKey,
    region: LogRegion,
    revision: u64,
    lines: Vec<LogLine>,
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
    choices_key: Option<UnitKey>,
    choices: Vec<UnitChoice>,
}

/// A command on its way out, with the name its URL needs.
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
    log: Option<FrameLog>,
    choices_key: Option<UnitKey>,
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
        let mut sender = dial(&path).await?;
        let units: Vec<ViewUnit> = fetch(&mut sender, Method::GET, "/units", None).await?;

        let shared = Arc::new(Shared {
            frame: Mutex::new(None),
            wanted: Mutex::new(None),
            moved: Notify::new(),
        });
        let (commands, incoming) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(
            Poller {
                path,
                poll,
                shared: shared.clone(),
                incoming,
                cancel: cancel.clone(),
            }
            .run(sender),
        );

        Ok(Self {
            units,
            log: None,
            choices_key: None,
            choices: Vec::new(),
            shared,
            commands,
            cancel,
            task: Some(task),
        })
    }

    /// The name a key belongs to, for the URL that addresses it.
    ///
    /// A scan and not a map: the rows are a session's worth of units, in
    /// name order, and this happens on a keypress rather than on a frame.
    fn name(&self, key: UnitKey) -> Option<SmallStr> {
        self.units
            .iter()
            .find(|unit| unit.unit_key == key)
            .map(|unit| unit.name.clone())
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

    /// Whatever the last poll fetched for this unit, and nothing for any
    /// other.
    ///
    /// Empty rather than stale when the menu opens on a unit the task has not
    /// asked about yet: the trait allows filling nothing, and a menu of
    /// another unit's verbs would act on the wrong proc.
    fn choices(&self, key: UnitKey, out: &mut Vec<UnitChoice>) {
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
            ViewCommand::Start { key } => self.name(key).map(Outgoing::Start),
            ViewCommand::Stop { key } => self.name(key).map(Outgoing::Stop),
            ViewCommand::Dispatch { key, event } => {
                self.name(key).map(|name| Outgoing::Dispatch(name, event))
            }
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
    fn set_log(&mut self, key: Option<UnitKey>, region: LogRegion) {
        let moved = self.log.as_ref().map(|log| log.unit_key) != key;
        if moved {
            self.log = None;
        }
        let wanted = key.and_then(|unit_key| {
            Some(Wanted {
                unit_key,
                name: self.name(unit_key)?,
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
            region: log.region,
            revision: log.revision,
            lines: &log.lines,
        })
    }
}

/// The half that talks, on a task of its own.
struct Poller {
    path: PathBuf,
    poll: Duration,
    shared: Arc<Shared>,
    incoming: mpsc::UnboundedReceiver<Outgoing>,
    cancel: CancellationToken,
}

impl Poller {
    /// Poll until cancelled, reconnecting for as long as it takes.
    ///
    /// A connection that fails ends that round and not the task: the session
    /// is a separate process and may be restarted under a screen that is
    /// still up. What the screen shows meanwhile is its last frame.
    async fn run(mut self, sender: SendRequest<Full<Bytes>>) {
        let mut sender = Some(sender);
        loop {
            let connected = match sender.take() {
                Some(sender) => sender,
                None => {
                    tokio::select! {
                        _ = self.cancel.cancelled() => return,
                        _ = tokio::time::sleep(RECONNECT_DELAY) => {}
                    }
                    match dial(&self.path).await {
                        Ok(sender) => sender,
                        Err(_) => continue,
                    }
                }
            };
            if self.rounds(connected).await {
                return;
            }
        }
    }

    /// Poll on one connection until it fails; `true` if it was cancelled.
    async fn rounds(&mut self, mut sender: SendRequest<Full<Bytes>>) -> bool {
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
            if self.round(&mut sender, &mut held).await.is_err() {
                return false;
            }
        }
    }

    /// One poll: everything asked for, then one frame published.
    ///
    /// The requests go one after another on the one connection, which is what
    /// makes "one in flight" structural rather than something to remember.
    async fn round(
        &mut self,
        sender: &mut SendRequest<Full<Bytes>>,
        held: &mut Option<FrameLog>,
    ) -> Result<(), ViewSocketError> {
        while let Ok(outgoing) = self.incoming.try_recv() {
            let (path, body) = match outgoing {
                Outgoing::Start(name) => (format!("/units/{name}/start"), None),
                Outgoing::Stop(name) => (format!("/units/{name}/stop"), None),
                Outgoing::Dispatch(name, event) => (
                    format!("/units/{name}/dispatch"),
                    Some(serde_json::to_vec(&event).unwrap_or_default()),
                ),
            };
            send(sender, Method::POST, &path, body).await?;
        }

        let units: Vec<ViewUnit> = fetch(sender, Method::GET, "/units", None).await?;
        let wanted = self.shared.wanted.lock().clone();

        let (log, choices_key, choices) = match wanted {
            None => {
                *held = None;
                (None, None, Vec::new())
            }
            Some(wanted) => {
                let log = self.fetch_log(sender, &wanted, held).await?;
                let choices = fetch(
                    sender,
                    Method::GET,
                    &format!("/units/{}/choices", wanted.name),
                    None,
                )
                .await
                .unwrap_or_default();
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
    /// onto the wire. A `304` answers with the lines already held rather than
    /// with a word meaning "keep yours": see [`Frame`].
    async fn fetch_log(
        &self,
        sender: &mut SendRequest<Full<Bytes>>,
        wanted: &Wanted,
        held: &mut Option<FrameLog>,
    ) -> Result<Option<FrameLog>, ViewSocketError> {
        let same = held
            .as_ref()
            .is_some_and(|log| log.unit_key == wanted.unit_key && log.region == wanted.region);
        let revision = same
            .then(|| held.as_ref().map(|log| log.revision))
            .flatten();

        let region = wanted.region;
        let path = format!(
            "/units/{}/log?line_start={}&line_end={}&column_start={}&column_end={}",
            wanted.name, region.line_start, region.line_end, region.column_start, region.column_end
        );
        let response = request(sender, Method::GET, &path, None, revision).await?;
        if response.0 == StatusCode::NOT_MODIFIED {
            // The lines are already here; only the wire was spared.
            return Ok(held.clone());
        }
        if response.0 == StatusCode::NOT_FOUND {
            *held = None;
            return Ok(None);
        }

        let body: LogBody = serde_json::from_slice(&response.1)?;
        let log = FrameLog {
            unit_key: wanted.unit_key,
            region: body.region,
            revision: body.revision,
            lines: body.lines,
        };
        *held = Some(log.clone());
        Ok(Some(log))
    }
}

/// What the log route answers with.
#[derive(serde::Deserialize)]
struct LogBody {
    region: LogRegion,
    revision: u64,
    lines: Vec<LogLine>,
}

/// Open a connection and put its driver on a task of its own.
///
/// The driver is what moves bytes; dropping it closes the connection, so it
/// is spawned and left to end when the socket does.
async fn dial(path: &PathBuf) -> Result<SendRequest<Full<Bytes>>, ViewSocketError> {
    let stream = UnixStream::connect(path)
        .await
        .map_err(ViewSocketError::Connect)?;
    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(ViewSocketError::Http)?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(sender)
}

/// One request, and the status and bytes it answered with.
async fn request(
    sender: &mut SendRequest<Full<Bytes>>,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
    revision: Option<u64>,
) -> Result<(StatusCode, Bytes), ViewSocketError> {
    let mut builder = Request::builder()
        .method(method)
        // Required of every HTTP/1.1 request, and meaningless here: there is
        // no host, only the socket the connection was opened on.
        .header(header::HOST, "localhost")
        .uri(path);
    if let Some(revision) = revision {
        builder = builder.header(header::IF_NONE_MATCH, format!("\"{revision}\""));
    }
    let body = match body {
        Some(body) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Full::new(Bytes::from(body))
        }
        None => Full::new(Bytes::new()),
    };
    let request = builder.body(body).map_err(ViewSocketError::Request)?;
    let response = sender
        .send_request(request)
        .await
        .map_err(ViewSocketError::Http)?;
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(ViewSocketError::Http)?
        .to_bytes();
    Ok((status, bytes))
}

/// A request whose answer is read as `T`.
async fn fetch<T: serde::de::DeserializeOwned>(
    sender: &mut SendRequest<Full<Bytes>>,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<T, ViewSocketError> {
    let (status, bytes) = request(sender, method, path, body, None).await?;
    if !status.is_success() {
        return Err(ViewSocketError::Status(status.as_u16()));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// A request whose answer is only whether it arrived.
async fn send(
    sender: &mut SendRequest<Full<Bytes>>,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<(), ViewSocketError> {
    request(sender, method, path, body, None).await?;
    Ok(())
}
