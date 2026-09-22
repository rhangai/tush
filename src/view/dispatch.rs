use std::path::PathBuf;

use http_body_util::Full;
use hyper::{Method, StatusCode, body::Bytes, client::conn::http1::SendRequest};

use crate::{
    error::{ViewDispatchError, ViewSocketError},
    unit::{UnitChoice, UnitEvent},
    util::str::SmallStr,
    view::socket::{dial, encode_path_segment, fetch, request},
};

/// One command for a session in another process, for a caller with no screen.
///
/// The same routes [`ViewSocket`](crate::view::ViewSocket) drives, without the
/// following: `tush dispatch` says one thing and exits, so there is no frame to
/// publish and nothing to poll.
///
/// **This one waits and it fails**, which the rest of [`view`](crate::view) is
/// documented never to do. That rule is there for a screen that must not stall
/// on a round trip; a shell waiting for an exit code is the opposite case, and
/// a command silently not delivered is the failure worth avoiding here.
pub struct ViewDispatch {
    sender: SendRequest<Full<Bytes>>,
}

impl ViewDispatch {
    /// Open a connection to the session listening on `path`.
    pub async fn connect(path: PathBuf) -> Result<Self, ViewDispatchError> {
        Ok(Self {
            sender: dial(&path).await?,
        })
    }

    /// Run `key`, on `mode` if one was named and on the mode it is already on
    /// if not.
    ///
    /// Two routes, because a mode is not a start with an argument on it:
    /// naming one has to go through the unit's behavior, which is what moves
    /// the index before the run is built.
    pub async fn start(&mut self, key: &str, mode: Option<&str>) -> Result<(), ViewDispatchError> {
        let key = SmallStr::new(key);
        let path = encode_path_segment(&key);
        let Some(mode) = mode else {
            return self.post(&key, &format!("/units/{path}/start"), None).await;
        };
        let event = self.mode_event(&key, &path, mode).await?;
        let body = serde_json::to_vec(&event).map_err(ViewSocketError::Body)?;
        self.post(&key, &format!("/units/{path}/dispatch"), Some(body))
            .await
    }

    /// Stop `key`, without waiting for it to be gone.
    pub async fn stop(&mut self, key: &str) -> Result<(), ViewDispatchError> {
        let key = SmallStr::new(key);
        let path = encode_path_segment(&key);
        self.post(&key, &format!("/units/{path}/stop"), None).await
    }

    /// Which mode `mode` names, as the event that runs it.
    ///
    /// The list is asked for rather than counted here: the indices belong to
    /// the behavior, and this route is the only thing that reports them.
    ///
    /// By name first and by index second, so a mode a config actually called
    /// `1` wins over the second one in the list. Names match without case
    /// because they are written to read in a menu and typed at a shell.
    async fn mode_event(
        &mut self,
        key: &SmallStr,
        path: &SmallStr,
        mode: &str,
    ) -> Result<UnitEvent, ViewDispatchError> {
        let choices: Vec<UnitChoice> = fetch(
            &mut self.sender,
            Method::GET,
            &format!("/units/{path}/choices"),
            None,
        )
        .await
        .map_err(|error| unknown_unit(error, key))?;
        let modes: Vec<(usize, SmallStr)> = choices
            .into_iter()
            .filter_map(|choice| match (choice.event, choice.mode) {
                (UnitEvent::StartMode(index), Some(name)) => Some((index, name)),
                _ => None,
            })
            .collect();

        if modes.is_empty() {
            return Err(ViewDispatchError::NoModes(key.to_string()));
        }
        if let Some((index, _)) = modes
            .iter()
            .find(|(_, name)| name.eq_ignore_ascii_case(mode))
        {
            return Ok(UnitEvent::StartMode(*index));
        }
        if let Ok(index) = mode.parse::<usize>()
            && modes.iter().any(|(candidate, _)| *candidate == index)
        {
            return Ok(UnitEvent::StartMode(index));
        }
        Err(ViewDispatchError::UnknownMode {
            unit: key.to_string(),
            mode: mode.to_string(),
            modes: modes.into_iter().map(|(_, name)| name).collect(),
        })
    }

    /// One command, and whether the session took it.
    ///
    /// The status is read here and not in
    /// [`send`](crate::view::ViewSocket), which drops it: a screen firing a
    /// command has nowhere to put a failure, and this is the caller that does.
    async fn post(
        &mut self,
        key: &SmallStr,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<(), ViewDispatchError> {
        let (status, _) = request(&mut self.sender, Method::POST, path, body, None)
            .await
            .map_err(|error| unknown_unit(error, key))?;
        match status {
            StatusCode::NOT_FOUND => Err(ViewDispatchError::UnknownUnit(key.to_string())),
            status if status.is_success() => Ok(()),
            status => Err(ViewSocketError::Status(status.as_u16()).into()),
        }
    }
}

/// A `404` off any of these routes means one thing — no proc is declared under
/// that key — and saying so beats handing a person a status code.
fn unknown_unit(error: ViewSocketError, key: &SmallStr) -> ViewDispatchError {
    match error {
        ViewSocketError::Status(404) => ViewDispatchError::UnknownUnit(key.to_string()),
        error => error.into(),
    }
}
