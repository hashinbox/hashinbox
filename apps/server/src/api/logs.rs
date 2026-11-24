//! The kept log entries (the `log` table), a page at a time (`/v1/logs`),
//! and each new one, live (`/v1/logs/stream`), both read by the same
//! filters: network, job, wallet, transaction, trigger, level, event and
//! time.

use std::{convert::Infallible, sync::Arc};

use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::{Stream, StreamExt as _};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, IndexParams};
use crate::{
    api::AppState,
    db::{Page, Paged as _},
    models::{Log, LogFilter},
    usecase::logs::{self, Heard},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(stream))
        .routes(routes!(list_targets))
}

/// A page of log entries.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = LogList)]
struct ListResponse {
    /// The entries, newest first; oldest first with `after`.
    logs: Vec<Log>,
    /// Pass as `cursor` for the next page; null on the last page. Always
    /// present.
    #[schema(required)]
    next_cursor: Option<String>,
}

impl ListResponse {
    /// `logs`, a page of at most `limit`.
    fn new(logs: Vec<Log>, limit: i64) -> Self {
        Self {
            next_cursor: logs.cursor(limit),
            logs,
        }
    }
}

/// Which entries a log list or the stream reads; each unset reads all.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct LogQuery {
    /// Only entries of this network, `chain/name` (`bitcoin/regtest`).
    network: Option<String>,
    /// Only entries of this job.
    job_id: Option<i64>,
    /// Only entries of this wallet, its id (`{chain}.{network}.{address}`).
    wallet_id: Option<String>,
    /// Only entries of this transaction, its id (`{chain}.{network}.{txid}`).
    transaction_id: Option<String>,
    /// Only entries of this trigger, its id.
    trigger_id: Option<String>,
    /// Only entries at this level or more severe: `error`, `warn`, `info`,
    /// `debug` or `trace`.
    level: Option<String>,
    /// With `level`: only entries at exactly that level.
    only: Option<bool>,
    /// Only entries of these events, comma-separated (`rpc.http,index.leg`).
    target: Option<String>,
    /// Only entries from before this Unix ms.
    before: Option<i64>,
    /// Only entries from this Unix ms on; a page then lists oldest first.
    after: Option<i64>,
}

impl LogQuery {
    /// As a filter; a 400 for a level that is none, or a network that is
    /// not `chain/name`.
    fn filter(self) -> Result<LogFilter, Error> {
        let level = self
            .level
            .map(|level| level.parse())
            .transpose()
            .map_err(|_| {
                Error::invalid_request("log level is not error, warn, info, debug or trace")
            })?;
        let network = match self.network {
            None => None,
            Some(label) => {
                let (chain, name) = label
                    .split_once('/')
                    .ok_or_else(|| Error::invalid_request("network is not chain/name"))?;
                Some(base::Network::new(base::Chain::new(chain), name.to_owned()))
            }
        };
        let targets = self.target.iter().flat_map(|targets| targets.split(','));
        Ok(LogFilter {
            network,
            job_id: self.job_id,
            wallet_id: self.wallet_id,
            transaction_id: self.transaction_id,
            trigger_id: self.trigger_id,
            level,
            only: self.only.unwrap_or(false),
            targets: targets.map(str::to_owned).collect(),
            before: self.before,
            after: self.after,
        })
    }
}

/// The page `index` asks for.
fn page(index: Result<Query<IndexParams>, QueryRejection>) -> Result<Page<i64>, Error> {
    let Query(index) = index.map_err(Error::invalid_query)?;
    index.page("logs", |cursor| cursor.parse().ok())
}

#[utoipa::path(
    get,
    path = "/v1/logs",
    operation_id = "list_logs",
    summary = "List the kept log entries",
    description = "The kept entries the filters read, newest first, a page at a time: its \
        level, event (`target`), message and fields, and its network, job, wallet, \
        transaction and trigger when it has them. With `after`, oldest first from that \
        moment, so a reader can page forward from a time. Debug and trace entries are kept \
        24 hours, the rest 30 days; at most the newest 10 000 of a job, and of a network's \
        (or no network's) entries outside a job.",
    params(LogQuery, IndexParams),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The entries", body = ListResponse),
        (status = 400, description = "Malformed query, level, network, cursor, or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "logs"
)]
async fn list(
    State(state): State<Arc<AppState>>,
    query: Result<Query<LogQuery>, QueryRejection>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let (filter, page) = (query.filter()?, page(index)?);
    let limit = page.limit;
    let logs = logs::find_page(&state.db, filter, page).await?;
    Ok(Json(ListResponse::new(logs, limit)))
}

/// The events log entries are kept of.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = LogTargets)]
struct TargetsResponse {
    /// Each event (`target`) once, in order: what a log list's `target`
    /// filter takes.
    targets: Vec<String>,
}

#[utoipa::path(
    get,
    path = "/v1/logs/targets",
    operation_id = "list_log_targets",
    summary = "List the logged events",
    description = "Every event (`target`) a kept entry has, each once, in order: the values \
        the `target` filter of the log lists and the stream takes.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The events", body = TargetsResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "logs"
)]
async fn list_targets(State(state): State<Arc<AppState>>) -> Result<Json<TargetsResponse>, Error> {
    let targets = logs::targets(&state.db).await?;
    Ok(Json(TargetsResponse { targets }))
}

/// A `missed` event.
#[derive(Debug, Serialize)]
struct Missed {
    /// How many entries were not sent.
    missed: u64,
}

#[utoipa::path(
    get,
    path = "/v1/logs/stream",
    operation_id = "stream_logs",
    summary = "Stream new log entries",
    description = "`text/event-stream` of every entry kept from now on that the filters \
        read (those of the list): one event per entry, its `data` a `LogEntry` (its `id` 0: \
        not yet stored). A reader too slow for the feed gets `event: missed` with \
        `{\"missed\": n}`, then the entries after. Comments keep the connection alive; the \
        stream does not end.",
    params(LogQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The entries, as they are kept",
            content_type = "text/event-stream", body = String),
        (status = 400, description = "Malformed query, level, or network", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody)
    ),
    tag = "logs"
)]
async fn stream(
    State(state): State<Arc<AppState>>,
    query: Result<Query<LogQuery>, QueryRejection>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let filter = query.filter()?;
    let events = logs::listen(&state.logs, filter).map(|heard| Ok(event(&heard)));
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

/// What a listener heard as its event: an entry, or `missed`.
fn event(heard: &Heard) -> Event {
    match heard {
        Heard::Entry(log) => Event::default().data(serde_json::to_string(log).unwrap_or_default()),
        Heard::Missed(missed) => Event::default()
            .event("missed")
            .data(serde_json::to_string(&Missed { missed: *missed }).unwrap_or_default()),
    }
}
