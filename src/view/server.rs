use std::{
    fmt,
    path::{Path, PathBuf},
};

use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, StatusCode, Uri,
    body::{Bytes, Incoming},
    client::conn::http1::SendRequest,
    header::{self, HeaderValue},
};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;

use crate::{
    error::ViewSocketError,
    log::{LogLine, LogRegion},
    unit::{UnitChoice, UnitEvent},
    util::{
        bytes::BytesMutPool,
        str::{SmallStr, SmallStrBuilder},
    },
    view::client::{ViewSettings, ViewUnit},
};

enum ServerClientAddress {
    UnixSocket(PathBuf),
}

/// How much room a path, header, or a body buffer starts with, and asks for again
/// before each piece.
///
/// Enough for the longest of either whole — a log path with four numbers in it
/// — with room to spare, since asking for more than the tail holds is what
/// puts the buffer back at the start of its own allocation.
const BYTES_POOL_BUFFER: usize = 1024;

/// One connection to a session's socket, with a method per route.
///
/// Every route the server declares is spelled once, here. It was spelled at
/// the call sites before, and `/units` alone was written twice inside one
/// file — a client is where a route gets forgotten, so there is one client.
///
/// Above the HTTP and below the deciding: a method reports what the session
/// answered and nothing more. What a `404` means is the caller's, because the
/// two of them disagree — a screen firing a command at a row that has gone
/// keeps polling, and `tush dispatch` exits non-zero.
pub struct ServerClient {
    /// The address for the client
    address: ServerClientAddress,
    /// The sender. None if not connected
    sender: Option<SendRequest<Full<Bytes>>>,
    /// Where a body that arrived in more than one frame is gathered, kept
    /// between requests so that it is refilled rather than built. Empty and
    /// untouched for a body that came whole, which is nearly all of them.
    spill: Vec<u8>,
    /// Where some of the data is written
    bytes_pool: BytesMutPool<8>,
}

/// What the log route answered, and the rectangle it answered with.
///
/// The body is the caller's buffer and not the answer's: it keeps whatever was
/// last written into it, so a `Changed` refills it rather than building one.
/// That is the whole reason this is a struct and not an enum carrying the
/// rectangle — an enum would drop the buffer on every answer that is not a
/// body, which is most of them.
///
/// Which leaves the body meaning different things per kind, and it is worth
/// being exact: `Changed` has just rewritten it, `Unchanged` has not touched it
/// and does not need to — nothing moved, so what is in there still *is* the
/// window — and `Gone` has not touched it either, and there it is stale.
#[derive(Clone)]
pub struct ServerLog {
    pub kind: ServerLogKind,
    pub body: ServerLogBody,
}

impl Default for ServerLog {
    /// [`Gone`](ServerLogKind::Gone) over an empty rectangle, which is what a
    /// buffer nothing has been read into yet honestly holds.
    ///
    /// Written here rather than derived so that the two halves need no
    /// `Default` of their own: a bare [`ServerLogKind`] has no default answer
    /// and a bare [`ServerLogBody`] no default rectangle — only the pair,
    /// standing for "not asked yet", means anything.
    fn default() -> Self {
        Self {
            kind: ServerLogKind::Gone,
            body: ServerLogBody {
                region: LogRegion::new(0..0, 0..0),
                revision: 0,
                lines: Vec::new(),
            },
        }
    }
}

/// Which of the three things the log route said.
///
/// Three answers and not a `Result`, because two of them are not failures: a
/// `304` means the caller's copy is still current and a `404` means there is
/// nothing to show. Collapsing either into an error moves the decision back to
/// the call site, which is the thing this exists to take away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ServerLogKind {
    /// The body has been rewritten with the rectangle as it actually came out,
    /// which can hold fewer lines than were asked for.
    Changed,
    /// The revision that went out as `If-None-Match` is still current.
    Unchanged,
    /// No unit under that key, or no log under the unit.
    Gone,
}

/// A rectangle of one unit's log, as the route answers with it.
#[derive(Clone, serde::Deserialize)]
pub struct ServerLogBody {
    pub region: LogRegion,
    pub revision: u64,
    pub lines: Vec<LogLine>,
}

impl ServerClient {
    /// Only creates the client
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            address: ServerClientAddress::UnixSocket(path.into()),
            sender: None,
            spill: Vec::new(),
            bytes_pool: BytesMutPool::with_capacity(BYTES_POOL_BUFFER),
        }
    }
    /// Open a connection and put its driver on a task of its own.
    ///
    /// The driver is what moves bytes; dropping it closes the connection, so
    /// it is spawned and left to end when the socket does.
    pub async fn connect(path: &Path) -> Result<Self, ViewSocketError> {
        let mut client = Self::new(path);
        client.reconnect().await?;
        Ok(client)
    }

    /// Ensure the client is connected
    pub async fn ensure_connected(&mut self) -> Result<(), ViewSocketError> {
        if self.sender.is_none() {
            self.reconnect().await?;
        }
        Ok(())
    }

    /// Reconnects the sender, if needed
    pub async fn reconnect(&mut self) -> Result<(), ViewSocketError> {
        self.sender = None;
        let stream = match &self.address {
            ServerClientAddress::UnixSocket(path_buf) => UnixStream::connect(path_buf)
                .await
                .map_err(ViewSocketError::Connect)?,
        };
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(ViewSocketError::Http)?;
        self.sender = Some(sender);
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(())
    }

    /// Every unit, as a row.
    ///
    /// The vec it fills is a fresh one, and that is all that separates it from
    /// [`units_in_place`](ServerClient::units_in_place) — one route, decoded
    /// one way, for a caller that has no buffer to lend.
    pub async fn units(&mut self) -> Result<Vec<ViewUnit>, ViewSocketError> {
        let mut out = Vec::new();
        self.units_in_place(&mut out).await?;
        Ok(out)
    }

    /// The same rows, written into `out` rather than handed back in a vec of
    /// their own — for a caller polling on a clock, which would otherwise
    /// build and drop one every round.
    pub async fn units_in_place(&mut self, out: &mut Vec<ViewUnit>) -> Result<(), ViewSocketError> {
        // Static, and not written into the buffer: a constant path needs
        // neither the room nor the piece, `Uri::from_static` keeping the bytes
        // it was handed. This is the one request of every poll.
        self.fetch_in_place(out, Method::GET, Uri::from_static("/units"), None)
            .await
    }

    /// What the session says about how it should be shown.
    pub async fn settings(&mut self) -> Result<ViewSettings, ViewSocketError> {
        self.fetch(Method::GET, Uri::from_static("/settings"), None)
            .await
    }

    /// Everything that can be asked of one unit right now.
    ///
    /// Over a fresh vec; see [`units`](ServerClient::units).
    pub async fn choices(&mut self, key: &SmallStr) -> Result<Vec<UnitChoice>, ViewSocketError> {
        let mut out = Vec::new();
        self.choices_in_place(&mut out, key).await?;
        Ok(out)
    }

    /// The same list, written into `out`. See
    /// [`units_in_place`](ServerClient::units_in_place).
    pub async fn choices_in_place(
        &mut self,
        out: &mut Vec<UnitChoice>,
        key: &SmallStr,
    ) -> Result<(), ViewSocketError> {
        let key = key_path(key);
        let uri = self.uri(format_args!("/units/{key}/choices"))?;
        self.fetch_in_place(out, Method::GET, uri, None).await
    }

    /// Ask for a unit to run, once what it depends on is up.
    pub async fn start(&mut self, key: &SmallStr) -> Result<StatusCode, ViewSocketError> {
        let key = key_path(key);
        let uri = self.uri(format_args!("/units/{key}/start"))?;
        self.post(uri, None).await
    }

    /// Stop a unit, without waiting for it to be gone.
    pub async fn stop(&mut self, key: &SmallStr) -> Result<StatusCode, ViewSocketError> {
        let key = key_path(key);
        let uri = self.uri(format_args!("/units/{key}/stop"))?;
        self.post(uri, None).await
    }

    /// Hand a unit an event and let its behavior decide.
    pub async fn dispatch(
        &mut self,
        key: &SmallStr,
        event: UnitEvent,
    ) -> Result<StatusCode, ViewSocketError> {
        let key = key_path(key);
        // The body first, so that both pieces come out of the buffer before
        // either goes anywhere: they are two cuts of the same allocation and
        // live side by side until the request is done.
        let body = self.body(&event)?;
        let uri = self.uri(format_args!("/units/{key}/dispatch"))?;
        self.post(uri, Some(body)).await
    }

    /// A rectangle of one unit's log, or word that nothing changed.
    ///
    /// Over a fresh body; see [`units`](ServerClient::units).
    pub async fn log(
        &mut self,
        key: &SmallStr,
        region: LogRegion,
        revision: Option<u64>,
    ) -> Result<ServerLog, ViewSocketError> {
        let mut out = ServerLog::default();
        self.log_in_place(&mut out, key, region, revision).await?;
        Ok(out)
    }

    /// The same answer, written into `out`.
    ///
    /// Sets `out.kind` whatever happened, and rewrites `out.body` only for a
    /// [`Changed`](ServerLogKind::Changed) — the other two say to keep and to
    /// drop what the caller has, and neither is this function's to decide.
    ///
    /// A failure leaves both alone: `out` is the caller's buffer from the last
    /// round, and a request that did not answer is no reason to lose it.
    ///
    /// `revision` is what the caller already drew, sent as `If-None-Match`.
    ///
    /// The region goes out field by field rather than serialized, and has to
    /// keep agreeing with the `Query<LogRegion>` the route parses it back
    /// with — a field added there and not here is one the server would take
    /// and no client would ever send.
    pub async fn log_in_place(
        &mut self,
        out: &mut ServerLog,
        key: &SmallStr,
        region: LogRegion,
        revision: Option<u64>,
    ) -> Result<(), ViewSocketError> {
        let key = key_path(key);
        let uri = self.uri(format_args!(
            "/units/{key}/log?line_start={}&line_end={}&column_start={}&column_end={}",
            region.line_start, region.line_end, region.column_start, region.column_end
        ))?;
        let response = self.send(Method::GET, uri, None, revision).await?;
        let status = response.status();
        let body = response.into_body();
        let kind = match status {
            StatusCode::NOT_MODIFIED => {
                self.read_body(body, |_| Ok(ServerLogKind::Unchanged))
                    .await?
            }
            StatusCode::NOT_FOUND => self.read_body(body, |_| Ok(ServerLogKind::Gone)).await?,
            status if status.is_success() => {
                let into = &mut out.body;
                self.read_body(body, |bytes| {
                    decode_in_place(into, bytes)?;
                    Ok(ServerLogKind::Changed)
                })
                .await?
            }
            status => {
                self.read_body(body, |_| Ok(())).await?;
                return Err(ViewSocketError::Status(status.as_u16()));
            }
        };
        out.kind = kind;
        Ok(())
    }

    /// A request whose answer is read as `T`, with anything but a success
    /// taken as the failure it is.
    async fn fetch<T: serde::de::DeserializeOwned>(
        &mut self,
        method: Method,
        uri: Uri,
        body: Option<Bytes>,
    ) -> Result<T, ViewSocketError> {
        let response = self.send(method, uri, body, None).await?;
        let status = response.status();
        let body = response.into_body();
        if !status.is_success() {
            self.read_body(body, |_| Ok(())).await?;
            return Err(ViewSocketError::Status(status.as_u16()));
        }
        self.read_body(body, |bytes| Ok(serde_json::from_slice(bytes)?))
            .await
    }

    /// A request whose answer is written into `out`.
    ///
    /// [`deserialize_in_place`](serde::Deserialize::deserialize_in_place) and
    /// not a fetch assigned over `out`: `Vec`'s implementation of it overwrites
    /// the elements that are already there and pushes only what is left over,
    /// so a list that came back the same length costs no allocation at all.
    ///
    /// How much each element saves is its own business: `Vec` reuses the slot,
    /// and whether the value in it reuses what it holds depends on its own
    /// `deserialize_in_place`.
    async fn fetch_in_place<T: serde::de::DeserializeOwned>(
        &mut self,
        out: &mut T,
        method: Method,
        uri: Uri,
        body: Option<Bytes>,
    ) -> Result<(), ViewSocketError> {
        let response = self.send(method, uri, body, None).await?;
        let status = response.status();
        let body = response.into_body();
        if !status.is_success() {
            self.read_body(body, |_| Ok(())).await?;
            return Err(ViewSocketError::Status(status.as_u16()));
        }
        self.read_body(body, |bytes| decode_in_place(out, bytes))
            .await
    }

    /// A command, and the status it came back with.
    ///
    /// The status is handed over rather than turned into an error: a session
    /// that refused a command is answering, and only the caller knows whether
    /// that is worth stopping for.
    async fn post(&mut self, uri: Uri, body: Option<Bytes>) -> Result<StatusCode, ViewSocketError> {
        let response = self.send(Method::POST, uri, body, None).await?;
        let status = response.status();
        self.read_body(response.into_body(), |_| Ok(())).await?;
        Ok(status)
    }

    /// Read a response body and hand it to `decode` as one slice.
    ///
    /// Reads one frame ahead: until a second turns up, the first is the whole
    /// body, and being a slice of hyper's own read buffer it is decoded where
    /// it lies. Only a body split across frames is gathered, into
    /// [`spill`](ServerClient::spill), which is kept between requests and
    /// refilled rather than built.
    ///
    /// **Every caller reads to the end, including the ones that want nothing.**
    /// The poller puts its requests down one connection, one after another, and
    /// hyper only reuses a connection whose last body was finished — a `304` or
    /// a command's empty answer dropped unread costs a reconnect per round.
    async fn read_body<R>(
        &mut self,
        mut body: Incoming,
        decode: impl FnOnce(&[u8]) -> Result<R, ViewSocketError>,
    ) -> Result<R, ViewSocketError> {
        let first = next_data(&mut body).await?.unwrap_or_default();
        let Some(second) = next_data(&mut body).await? else {
            return decode(&first);
        };

        self.spill.clear();
        self.spill.extend_from_slice(&first);
        self.spill.extend_from_slice(&second);
        while let Some(data) = next_data(&mut body).await? {
            self.spill.extend_from_slice(&data);
        }
        decode(&self.spill)
    }

    /// One request, answered but not read.
    ///
    /// Answering and reading are separate because the status decides what the
    /// body is worth — and the ones it is worth nothing for still have to be
    /// drained. See [`read_body`](ServerClient::read_body).
    async fn send(
        &mut self,
        method: Method,
        uri: Uri,
        body: Option<Bytes>,
        revision: Option<u64>,
    ) -> Result<hyper::Response<Incoming>, ViewSocketError> {
        // Before the sender is borrowed, since writing it is a borrow of the
        // buffer and the two are one `self`.
        let revision = match revision {
            Some(revision) => Some(self.revision(revision)?),
            None => None,
        };

        let sender = self.sender.as_mut().ok_or(ViewSocketError::NotConnected)?;
        let mut builder = Request::builder()
            .method(method)
            // Required of every HTTP/1.1 request, and meaningless here: there
            // is no host, only the socket the connection was opened on.
            .header(header::HOST, HeaderValue::from_static("localhost"))
            .uri(uri);
        if let Some(revision) = revision {
            builder = builder.header(header::IF_NONE_MATCH, revision);
        }
        let body = match body {
            Some(body) => {
                builder = builder.header(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                Full::new(body)
            }
            None => Full::new(Bytes::new()),
        };
        let request = builder.body(body).map_err(ViewSocketError::Request)?;
        sender
            .send_request(request)
            .await
            .map_err(ViewSocketError::Http)
    }

    /// The path of a request, out of [`path`](ServerClient::path).
    fn uri(&mut self, args: fmt::Arguments<'_>) -> Result<Uri, ViewSocketError> {
        let path = self.bytes_pool.write(args);
        Ok(Uri::from_maybe_shared(path)?)
    }

    /// The revision a log request already drew, out of
    /// [`header_revision`](ServerClient::header_revision).
    fn revision(&mut self, revision: u64) -> Result<HeaderValue, ViewSocketError> {
        let value = self.bytes_pool.write(format_args!("\"{revision}\""));
        Ok(HeaderValue::from_maybe_shared(value)?)
    }

    /// One event as the JSON body of a request, out of
    /// [`payload`](ServerClient::payload).
    fn body(&mut self, event: &UnitEvent) -> Result<Bytes, ViewSocketError> {
        let buf = self.bytes_pool.json(event)?;
        Ok(buf)
    }
}

/// The next chunk of body, skipping trailers and empty frames, and `None` at
/// the end of it.
async fn next_data(body: &mut Incoming) -> Result<Option<Bytes>, ViewSocketError> {
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(ViewSocketError::Http)?;
        if let Ok(data) = frame.into_data()
            && !data.is_empty()
        {
            return Ok(Some(data));
        }
    }
    Ok(None)
}

/// Decode `bytes` over whatever `out` already holds.
///
/// The trailing bytes are checked separately: `deserialize_in_place` stops at
/// the end of the value, and a body with something after it is a server this
/// one does not speak.
fn decode_in_place<T: serde::de::DeserializeOwned>(
    out: &mut T,
    bytes: &[u8],
) -> Result<(), ViewSocketError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    T::deserialize_in_place(&mut deserializer, out)?;
    deserializer.end()?;
    Ok(())
}

/// `key` with everything a path segment cannot carry percent-encoded.
///
/// Done here and not by the callers, so a route added later cannot be the one
/// that forgets: every method above takes the key the config declared and this
/// is the only thing that turns one into a segment.
///
/// Unreserved only (RFC 3986 §2.3), which is the conservative set: a key is
/// whatever the config file declared it under, and a space in one makes a
/// request line that does not parse while a `/` makes one that routes
/// somewhere else. Over-encoding costs nothing, the server decoding the
/// segment before it looks the key up.
///
/// Returned untouched when there is nothing to encode — which is every key
/// anybody writes — so the common path is the copy it already was.
fn key_path(key: &SmallStr) -> SmallStr {
    if key.bytes().all(is_unreserved) {
        return key.clone();
    }
    // Byte at a time, so a multi-byte character becomes one `%XX` per byte
    // and the server's decoder puts the same UTF-8 back together.
    let mut encoded = SmallStrBuilder::new();
    for byte in key.bytes() {
        if is_unreserved(byte) {
            encoded.push(byte as char);
            continue;
        }
        encoded.push('%');
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0xf) as usize] as char);
    }
    encoded.finish()
}

/// Upper case, which is the spelling RFC 3986 says to produce.
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// The characters a path segment carries as themselves.
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}
