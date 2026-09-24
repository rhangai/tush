use std::path::PathBuf;

use hyper::StatusCode;

use crate::{
    error::{ViewDispatchError, ViewSocketError},
    unit::{UnitChoice, UnitEvent},
    util::str::SmallStr,
    view::server::ServerClient,
};

/// One command for a session in another process, for a caller with no screen.
///
/// The same [`ServerClient`] the screen polls through, said once and then
/// dropped: `tush dispatch` has no frame to publish and nothing to follow.
///
/// **This one waits and it fails**, which the rest of [`view`](crate::view) is
/// documented never to do. That rule is there for a screen that must not stall
/// on a round trip; a shell waiting for an exit code is the opposite case, and
/// a command silently not delivered is the failure worth avoiding here.
pub struct ViewDispatch {
    client: ServerClient,
}

impl ViewDispatch {
    /// Open a connection to the session listening on `path`.
    pub async fn connect(path: PathBuf) -> Result<Self, ViewDispatchError> {
        Ok(Self {
            client: ServerClient::connect(&path).await?,
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
        let answer = match mode {
            None => self.client.start(&key).await,
            Some(mode) => {
                let event = self.mode_event(&key, mode).await?;
                self.client.dispatch(&key, event).await
            }
        };
        accepted(answer, &key)
    }

    /// Stop `key`, without waiting for it to be gone.
    pub async fn stop(&mut self, key: &str) -> Result<(), ViewDispatchError> {
        let key = SmallStr::new(key);
        let answer = self.client.stop(&key).await;
        accepted(answer, &key)
    }

    /// Which mode `mode` names, as the event that runs it.
    ///
    /// The list is asked for rather than counted here: the indices belong to
    /// the behavior, and this route is the only thing that reports them.
    ///
    /// Full name, then short name, then position — full first so a mode a
    /// config actually called `1` wins over the second one in the list. Both
    /// names match without case, because they are written to read in a menu
    /// and typed at a shell.
    async fn mode_event(
        &mut self,
        key: &SmallStr,
        mode: &str,
    ) -> Result<UnitEvent, ViewDispatchError> {
        let choices = self
            .client
            .choices(key)
            .await
            .map_err(|error| unknown_unit(error, key))?;
        let modes: Vec<UnitChoice> = choices
            .into_iter()
            .filter(|choice| mode_index(choice).is_some())
            .collect();

        if modes.is_empty() {
            return Err(ViewDispatchError::NoModes(key.to_string()));
        }
        let named = modes
            .iter()
            .find(|choice| names(&choice.mode, mode))
            .or_else(|| modes.iter().find(|choice| names(&choice.mode_short, mode)));
        if let Some(choice) = named {
            return Ok(choice.event);
        }
        if let Ok(index) = mode.parse::<usize>()
            && modes.iter().any(|choice| mode_index(choice) == Some(index))
        {
            return Ok(UnitEvent::StartMode(index));
        }
        Err(ViewDispatchError::UnknownMode {
            unit: key.to_string(),
            mode: mode.to_string(),
            modes: modes.into_iter().filter_map(|choice| choice.mode).collect(),
        })
    }
}

/// The session's answer to one command, as this command's outcome.
///
/// [`ServerClient`] hands the status over instead of ruling on it, because the
/// screen and this disagree about what a `404` is worth — here it is the whole
/// answer, and it names the key rather than the number.
fn accepted(
    answer: Result<StatusCode, ViewSocketError>,
    key: &SmallStr,
) -> Result<(), ViewDispatchError> {
    match answer.map_err(|error| unknown_unit(error, key))? {
        StatusCode::NOT_FOUND => Err(ViewDispatchError::UnknownUnit(key.to_string())),
        status if status.is_success() => Ok(()),
        status => Err(ViewSocketError::Status(status.as_u16()).into()),
    }
}

/// Which mode a choice runs, and `None` for the entries that are not a mode —
/// every list has a `Stop` on the end.
fn mode_index(choice: &UnitChoice) -> Option<usize> {
    match choice.event {
        UnitEvent::StartMode(index) => Some(index),
        _ => None,
    }
}

/// Whether `typed` is that name. A mode with no short name is named by nothing
/// rather than by the empty string.
fn names(name: &Option<SmallStr>, typed: &str) -> bool {
    name.as_ref()
        .is_some_and(|name| name.eq_ignore_ascii_case(typed))
}

/// A `404` off any of these routes means one thing — no proc is declared under
/// that key — and saying so beats handing a person a status code.
fn unknown_unit(error: ViewSocketError, key: &SmallStr) -> ViewDispatchError {
    match error {
        ViewSocketError::Status(404) => ViewDispatchError::UnknownUnit(key.to_string()),
        error => error.into(),
    }
}
