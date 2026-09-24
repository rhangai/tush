use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header, response::Builder},
    response::Response,
    routing::{get, post},
};
use serde::Serialize;
use tokio_util::bytes::Bytes;

use crate::{
    app::AppUnitKey,
    log::{LogLine, LogRegion},
    server::state::{ServerLogResult, ServerState, ServerUnitResult},
    unit::UnitEvent,
    view::ViewSettings,
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
/// Kept and refreshed rather than built per request — see
/// [`read_units`](ServerState::read_units) — which is what lets this answer
/// `If-None-Match` the way the log route does: the rows of a session sitting
/// still are the same rows poll after poll, and saying so costs sixty six
/// bytes against eight hundred.
async fn units(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let drawn = drawn_revision(&headers);
    let result = state.read_units(drawn);
    match result {
        ServerUnitResult::NotModified => Err(StatusCode::NOT_MODIFIED),
        ServerUnitResult::Read(bytes, revision) => json_response(&state, Some(revision), bytes),
    }
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
) -> Result<Response, StatusCode> {
    let Some(key) = key(&state, &unit) else {
        return Err(StatusCode::NOT_FOUND);
    };
    let drawn = drawn_revision(&headers);
    let answer = state.read_log(key, drawn, region, |lines, revision| {
        state.json(&LogBody {
            region,
            lines,
            revision,
        })
    });

    match answer {
        ServerLogResult::NotFound => Err(StatusCode::NOT_FOUND),
        ServerLogResult::NotModified => Err(StatusCode::NOT_MODIFIED),
        ServerLogResult::Error(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
        ServerLogResult::Read(bytes, revision) => json_response(&state, Some(revision), bytes),
    }
}

/// Everything that can be asked of a unit right now.
async fn choices(
    State(state): State<Arc<ServerState>>,
    Path(unit): Path<String>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let key = state.key(&unit).ok_or(StatusCode::NOT_FOUND)?;
    let drawn = drawn_revision(&headers);
    let res = state.choices(key, drawn)?;
    json_response(&state, None, res)
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

/// The revision a client says it already has, out of `If-None-Match`.
///
/// One reading of the header for both routes that carry one: two copies of
/// this is two chances for a quoted number to be parsed one way here and
/// another way there, and the answer to that mismatch is a client that never
/// gets a `304`.
fn drawn_revision(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::IF_NONE_MATCH)?
        .to_str()
        .ok()?
        .trim_matches('"')
        .parse::<u64>()
        .ok()
}

fn json_response(
    state: &Arc<ServerState>,
    revision: Option<u64>,
    body: Bytes,
) -> Result<Response, StatusCode> {
    let mut builder = Builder::new();
    builder = builder.header(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Some(revision) = revision {
        builder = builder.header(
            header::ETAG,
            HeaderValue::from_maybe_shared(state.write(format_args!("\"{revision}\"")))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
        );
    }
    builder
        .body(Body::from(body))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
