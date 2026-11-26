//! `/v1/networks/{chain}/{network}/tip/search`: the block a network had at
//! a given time, searched on its node and streamed as the search narrows.

use std::{convert::Infallible, sync::Arc};

use axum::{
    extract::{Path, Query, State, rejection::QueryRejection},
    response::sse::{Event, Sse},
};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::models::Session;

use super::networks::NetworkPath;
use crate::error::Error;
use crate::{
    api::AppState,
    usecase::networks::{self, Search},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(search))
}

/// `at` as Unix seconds: RFC 3339 or Unix seconds.
///
/// # Errors
///
/// A 400 for anything else.
fn seconds(at: &str) -> Result<u64, Error> {
    at.parse::<u64>()
        .ok()
        .or_else(|| {
            at.parse::<jiff::Timestamp>()
                .ok()
                .and_then(|time| u64::try_from(time.as_second()).ok())
        })
        .ok_or_else(|| Error::invalid_argument("at must be an RFC 3339 time or Unix seconds"))
}

/// When, for the search.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct SearchQuery {
    /// RFC 3339 time (`2026-10-01T00:00:00Z`) or Unix seconds: find the last
    /// block at or before it.
    at: String,
    /// The last event `id` read (a step's number): the same search's steps
    /// after it.
    cursor: Option<String>,
}

/// A produced block in a search's events.
#[derive(Debug, Serialize)]
struct Stamp {
    /// Native position (height or slot).
    position: u64,
    /// Unix seconds.
    timestamp: u64,
}

impl From<base::BlockStamp> for Stamp {
    fn from(stamp: base::BlockStamp) -> Self {
        Self {
            position: stamp.position,
            timestamp: stamp.timestamp,
        }
    }
}

/// A `probe` event: the block searched for is at or after `low`, before
/// `high`.
#[derive(Debug, Serialize)]
struct Probe {
    low: Stamp,
    high: Stamp,
}

/// An `error` event.
#[derive(Debug, Serialize)]
struct Failed {
    message: String,
}

#[scope::utoipa(
    get,
    path = "/v1/networks/{chain}/{network}/tip/search",
    operation_id = "network_tip_search",
    summary = "Find a network's last block at or before a time, streaming the search",
    description = "`text/event-stream`, read from the node: `event: probe` with \
        `{\"low\":{\"position\",\"timestamp\"},\"high\":{…}}` after each step of the binary \
        search (the range narrows: the block is at or after `low`, before `high`); then \
        `event: found` with `{\"position\",\"timestamp\"}`, or `event: error` with \
        `{\"message\"}` (cannot find blocks by time, before the first block, the node \
        failed). The stream then ends. Each event's `id` is its step's number; a search \
        longer than the stream window (15 s) ends its answer with `event: close`: the same \
        search, asked again with the last `id` as `cursor`, goes on from the step after (it \
        runs on meanwhile, and is kept a minute after it ends).",
    params(NetworkPath, SearchQuery),
    responses(
        (status = 200, description = "The search's events", content_type = "text/event-stream",
            body = String),
        (status = 400, description = "Malformed `at` or `cursor`", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 404, description = "The network is not configured", body = Error)
    ),
    tag = "networks"
)]
async fn search(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
    query: Result<Query<SearchQuery>, QueryRejection>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Error> {
    scope::require!(session, "com.hashinbox.networks.$read");
    let Query(query) = query?;
    let time = seconds(&query.at)?;
    let seen = query.cursor.as_deref().map(str::parse::<usize>).transpose();
    let seen = seen.map_err(|_| Error::invalid_argument("cursor is not a step number"))?;
    let steps = networks::search_tip(&state.container, &state.searches, &path.network(), time)?;
    let events = after(steps, seen.unwrap_or(0));
    Ok(http_server::sse::windowed(events, state.stream_window))
}

/// The steps of a search after its `seen`th, each as its event, as they
/// come; they end with the search.
fn after(
    steps: watch::Receiver<Vec<Search>>,
    seen: usize,
) -> impl Stream<Item = Event> + Send + 'static {
    futures_util::stream::unfold((steps, seen), |(mut steps, seen)| async move {
        loop {
            let step = steps.borrow_and_update().get(seen).cloned();
            if let Some(step) = step {
                return Some((event(step).id((seen + 1).to_string()), (steps, seen + 1)));
            }
            steps.changed().await.ok()?;
        }
    })
}

/// A search's step as its event: `probe`, `found` or `error`, carrying its
/// data as JSON.
fn event(step: Search) -> Event {
    let (name, data) = match step {
        Search::Probe(probe) => (
            "probe",
            serde_json::to_string(&Probe {
                low: probe.low.into(),
                high: probe.high.into(),
            }),
        ),
        Search::Found(stamp) => ("found", serde_json::to_string(&Stamp::from(stamp))),
        Search::Failed(message) => ("error", serde_json::to_string(&Failed { message })),
    };
    Event::default().event(name).data(data.unwrap_or_default())
}
