//! `/v1/networks/{chain}/{network}/tip/search`: the block a network had at
//! a given time, searched on its node and streamed as the search narrows.

use std::{convert::Infallible, sync::Arc};

use axum::{
    extract::{Path, Query, State, rejection::QueryRejection},
    response::sse::{Event, Sse},
};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, networks::NetworkPath};
use crate::api::AppState;

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
        .ok_or_else(|| Error::invalid_request("at must be an RFC 3339 time or Unix seconds"))
}

/// When, for the search.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct SearchQuery {
    /// RFC 3339 time (`2026-10-01T00:00:00Z`) or Unix seconds: find the last
    /// block at or before it.
    at: String,
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

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/tip/search",
    operation_id = "search_tip",
    summary = "Find a network's last block at or before a time, streaming the search",
    description = "`text/event-stream`, read from the node: `event: probe` with \
        `{\"low\":{\"position\",\"timestamp\"},\"high\":{…}}` after each step of the binary \
        search (the range narrows: the block is at or after `low`, before `high`); then \
        `event: found` with `{\"position\",\"timestamp\"}`, or `event: error` with \
        `{\"message\"}` (cannot find blocks by time, before the first block, the node \
        failed). The stream then ends.",
    params(NetworkPath, SearchQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The search's events", content_type = "text/event-stream",
            body = String),
        (status = 400, description = "Malformed `at`", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "The network is not configured", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn search(
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
    query: Result<Query<SearchQuery>, QueryRejection>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let time = seconds(&query.at)?;
    let network = path.network(&state.payments)?;
    let dated = state.payments.service(&network)?.dated.clone();
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        // A client gone mid-search only stops hearing it.
        let probe = |probe: base::BlockProbe| {
            let body = Probe {
                low: probe.low.into(),
                high: probe.high.into(),
            };
            let _ = sender.send(event("probe", &body));
        };
        let found = match &dated {
            None => Err(anyhow::anyhow!("this network cannot find blocks by time")),
            Some(dated) => dated.search(time, &probe).await,
        };
        let last = match found {
            Ok(Some(stamp)) => event("found", &Stamp::from(stamp)),
            Ok(None) => event(
                "error",
                &Failed {
                    message: "the time is before the network's first block".to_owned(),
                },
            ),
            Err(error) => event(
                "error",
                &Failed {
                    message: format!("{error:#}"),
                },
            ),
        };
        let _ = sender.send(last);
    });
    Ok(Sse::new(futures_util::stream::unfold(
        receiver,
        |mut receiver| async move {
            let event = receiver.recv().await?;
            Some((Ok(event), receiver))
        },
    )))
}

/// The event `name` carrying `data` as JSON.
fn event(name: &str, data: &impl Serialize) -> Event {
    Event::default()
        .event(name)
        .data(serde_json::to_string(data).unwrap_or_default())
}
