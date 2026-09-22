use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;

use crate::{
    app::AppUnitKey,
    log::{LogLine, LogRegion},
    server::state::ServerState,
    unit::{UnitChoice, UnitEvent},
    view::{ViewSettings, ViewUnit},
};

/// Every route a client speaks, over whatever the caller is listening on.
///
/// Units are addressed by the key the config declared them under, and not by
/// [`AppUnitKey`]: a key is an interned symbol that means nothing outside the
/// process that made it, and a path a person can type is most of why this is
/// HTTP at all. The conversion happens in the unit map, which is the one place
/// text becomes a key.
///
/// The key and not the display name, which is a label the config may change
/// and two procs may share — the interner only ever saw the key.
pub fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/settings", get(settings))
        .route("/units", get(units))
        .route("/units/{key}/log", get(log))
        .route("/units/{key}/choices", get(choices))
        .route("/units/{key}/start", post(start))
        .route("/units/{key}/stop", post(stop))
        .route("/units/{key}/dispatch", post(dispatch))
        .with_state(state)
}

/// What a log request answers with, when it answers at all.
///
/// Reports the region it actually cut rather than the one asked for, because
/// a region counted back from the end can hold fewer lines than it names —
/// which is what lets a pane draw the overlap instead of blanking.
#[derive(Serialize)]
struct LogBody<'a> {
    region: LogRegion,
    revision: u64,
    lines: &'a [LogLine],
}

/// What the session says about how it should be shown.
///
/// Read once by a client when it attaches: it comes from a config file the
/// session read before it existed, so there is nothing here that can change
/// while it is listening.
async fn settings(State(state): State<Arc<ServerState>>) -> Json<ViewSettings> {
    Json(ViewSettings {
        colors: state.app().ui().colors,
    })
}

/// Every unit, as a row.
///
/// Built per request rather than kept: the names never change, but a state
/// does, and a handful of inline strings is cheaper than a copy that has to
/// be invalidated.
async fn units(State(state): State<Arc<ServerState>>) -> Json<Vec<ViewUnit>> {
    let unit_map = state.app().unit_map();
    let mut units: Vec<ViewUnit> = unit_map
        .keys()
        .filter_map(|unit_key| {
            let entry = unit_map.entry(unit_key).ok()?;
            let unit = entry.unit();
            let settings = entry.settings();
            Some(ViewUnit {
                unit_key,
                key: entry.key().clone(),
                name: unit.name(),
                name_short: unit.name_short(),
                mode: unit.mode(),
                mode_short: unit.mode_short(),
                state: unit.state(),
                panel: settings.panel,
            })
        })
        .collect();
    units.sort_by(|a, b| (a.panel, &a.name).cmp(&(b.panel, &b.name)));
    Json(units)
}

/// A rectangle of one unit's log.
///
/// `If-None-Match` carries the revision the client already drew, and a log
/// that has not moved answers `304` with no body — the same test
/// [`ViewApp`](crate::view::ViewApp) does against its own reader, which is
/// most syncs, and the reason a revision is carried at all.
///
/// The region is a query and not a body so that a person can ask for one with
/// a browser: `?line_start=0&line_end=50&column_start=0&column_end=200`.
async fn log(
    State(state): State<Arc<ServerState>>,
    Path(unit): Path<String>,
    Query(region): Query<LogRegion>,
    headers: HeaderMap,
) -> Response {
    let Some(key) = key(&state, &unit) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let drawn = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim_matches('"').parse::<u64>().ok());

    // Everything that touches the reader happens inside here, so there is no
    // guard alive at an `.await` — see `ServerState::read_log`.
    let answer = state.read_log(key, region, |lines, revision| {
        if drawn == Some(revision) {
            return None;
        }
        Some((
            revision,
            serde_json::to_string(&LogBody {
                region,
                revision,
                lines,
            }),
        ))
    });

    match answer {
        None => StatusCode::NOT_FOUND.into_response(),
        Some(None) => StatusCode::NOT_MODIFIED.into_response(),
        Some(Some((_, Err(_)))) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Some(Some((revision, Ok(body)))) => (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::ETAG, &format!("\"{revision}\"")),
            ],
            body,
        )
            .into_response(),
    }
}

/// Everything that can be asked of a unit right now.
async fn choices(
    State(state): State<Arc<ServerState>>,
    Path(unit): Path<String>,
) -> Result<Json<Vec<UnitChoice>>, StatusCode> {
    let key = key(&state, &unit).ok_or(StatusCode::NOT_FOUND)?;
    let mut out = Vec::new();
    let unit_map = state.app().unit_map();
    let entry = unit_map.entry(key).map_err(|_| StatusCode::NOT_FOUND)?;
    entry.unit().choices(&mut out);
    Ok(Json(out))
}

/// Ask for a unit to run, once what it depends on is up.
///
/// `202` and not `200`: the session records what is wanted and a task
/// performs it, so by the time this answers nothing has started yet. Every
/// command here says the same thing.
async fn start(
    State(state): State<Arc<ServerState>>,
    Path(unit): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let key = key(&state, &unit).ok_or(StatusCode::NOT_FOUND)?;
    state.app().schedule(key);
    Ok(StatusCode::ACCEPTED)
}

/// Stop a unit, without waiting for it to be gone.
async fn stop(
    State(state): State<Arc<ServerState>>,
    Path(unit): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let key = key(&state, &unit).ok_or(StatusCode::NOT_FOUND)?;
    state.app().stop(key);
    Ok(StatusCode::ACCEPTED)
}

/// Hand a unit an event and let its behavior decide.
async fn dispatch(
    State(state): State<Arc<ServerState>>,
    Path(unit): Path<String>,
    Json(event): Json<UnitEvent>,
) -> Result<StatusCode, StatusCode> {
    let key = key(&state, &unit).ok_or(StatusCode::NOT_FOUND)?;
    state
        .app()
        .dispatch(key, event)
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(StatusCode::ACCEPTED)
}

/// The [`AppUnitKey`] a config key was interned under, or nothing for a key no
/// proc was declared with.
fn key(state: &ServerState, key: &str) -> Option<AppUnitKey> {
    state.app().unit_map().key(key)
}
