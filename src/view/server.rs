use std::path::Path;

use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode, body::Bytes, client::conn::http1::SendRequest, header};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;

use crate::{
    error::ViewSocketError,
    log::{LogLine, LogRegion},
    unit::{UnitChoice, UnitEvent},
    util::str::{SmallStr, SmallStrBuilder},
    view::client::{ViewSettings, ViewUnit},
};

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
    sender: SendRequest<Full<Bytes>>,
}

/// What the log route answered.
///
/// Three answers and not a `Result`, because two of them are not failures: a
/// `304` means the caller's copy is still current and a `404` means there is
/// nothing to show. Collapsing either into an error moves the decision back to
/// the call site, which is the thing this type exists to take away.
pub enum ServerLog {
    /// The rectangle as it actually came out, which can hold fewer lines than
    /// were asked for.
    Body(ServerLogBody),
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
    /// Open a connection and put its driver on a task of its own.
    ///
    /// The driver is what moves bytes; dropping it closes the connection, so
    /// it is spawned and left to end when the socket does.
    pub async fn connect(path: &Path) -> Result<Self, ViewSocketError> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(ViewSocketError::Connect)?;
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(ViewSocketError::Http)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Self { sender })
    }

    /// Every unit, as a row.
    pub async fn units(&mut self) -> Result<Vec<ViewUnit>, ViewSocketError> {
        self.fetch(Method::GET, "/units", None).await
    }

    /// What the session says about how it should be shown.
    pub async fn settings(&mut self) -> Result<ViewSettings, ViewSocketError> {
        self.fetch(Method::GET, "/settings", None).await
    }

    /// Everything that can be asked of one unit right now.
    pub async fn choices(&mut self, key: &SmallStr) -> Result<Vec<UnitChoice>, ViewSocketError> {
        let key = key_path(key);
        self.fetch(Method::GET, &format!("/units/{key}/choices"), None)
            .await
    }

    /// Ask for a unit to run, once what it depends on is up.
    pub async fn start(&mut self, key: &SmallStr) -> Result<StatusCode, ViewSocketError> {
        let key = key_path(key);
        self.post(&format!("/units/{key}/start"), None).await
    }

    /// Stop a unit, without waiting for it to be gone.
    pub async fn stop(&mut self, key: &SmallStr) -> Result<StatusCode, ViewSocketError> {
        let key = key_path(key);
        self.post(&format!("/units/{key}/stop"), None).await
    }

    /// Hand a unit an event and let its behavior decide.
    pub async fn dispatch(
        &mut self,
        key: &SmallStr,
        event: UnitEvent,
    ) -> Result<StatusCode, ViewSocketError> {
        let key = key_path(key);
        let body = serde_json::to_vec(&event)?;
        self.post(&format!("/units/{key}/dispatch"), Some(body))
            .await
    }

    /// A rectangle of one unit's log, or word that nothing changed.
    ///
    /// `revision` is what the caller already drew, sent as `If-None-Match`.
    ///
    /// The region goes out field by field rather than serialized, and has to
    /// keep agreeing with the `Query<LogRegion>` the route parses it back
    /// with — a field added there and not here is one the server would take
    /// and no client would ever send.
    pub async fn log(
        &mut self,
        key: &SmallStr,
        region: LogRegion,
        revision: Option<u64>,
    ) -> Result<ServerLog, ViewSocketError> {
        let key = key_path(key);
        let path = format!(
            "/units/{key}/log?line_start={}&line_end={}&column_start={}&column_end={}",
            region.line_start, region.line_end, region.column_start, region.column_end
        );
        let (status, bytes) = self.request(Method::GET, &path, None, revision).await?;
        match status {
            StatusCode::NOT_MODIFIED => Ok(ServerLog::Unchanged),
            StatusCode::NOT_FOUND => Ok(ServerLog::Gone),
            status if status.is_success() => Ok(ServerLog::Body(serde_json::from_slice(&bytes)?)),
            status => Err(ViewSocketError::Status(status.as_u16())),
        }
    }

    /// A request whose answer is read as `T`, with anything but a success
    /// taken as the failure it is.
    async fn fetch<T: serde::de::DeserializeOwned>(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<T, ViewSocketError> {
        let (status, bytes) = self.request(method, path, body, None).await?;
        if !status.is_success() {
            return Err(ViewSocketError::Status(status.as_u16()));
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// A command, and the status it came back with.
    ///
    /// The status is handed over rather than turned into an error: a session
    /// that refused a command is answering, and only the caller knows whether
    /// that is worth stopping for.
    async fn post(
        &mut self,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<StatusCode, ViewSocketError> {
        Ok(self.request(Method::POST, path, body, None).await?.0)
    }

    /// One request, and the status and bytes it answered with.
    async fn request(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        revision: Option<u64>,
    ) -> Result<(StatusCode, Bytes), ViewSocketError> {
        let mut builder = Request::builder()
            .method(method)
            // Required of every HTTP/1.1 request, and meaningless here: there
            // is no host, only the socket the connection was opened on.
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
        let response = self
            .sender
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
