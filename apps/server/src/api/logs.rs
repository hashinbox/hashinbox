//! The kept log entries (the `log` table), newest first, a page at a
//! time: every entry (`/v1/logs`), a network's
//! (`/v1/networks/{chain}/{network}/logs`) and a job's
//! (`/v1/jobs/{id}/logs`); and each new one, live (`/v1/logs/stream`).

use std::{convert::Infallible, sync::Arc};

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::{Stream, StreamExt as _};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, IndexParams, networks::NetworkPath};
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
        .routes(routes!(list_network))
        .routes(routes!(list_job))
}

/// A page of log entries.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = LogList)]
struct ListResponse {
    /// The entries, newest first.
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

/// Which entries a log list reads.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct LogQuery {
    /// Only entries at this level or more severe: `error`, `warn`, `info`,
    /// `debug` or `trace`.
    level: Option<String>,
    /// Only entries of this event (`rpc.http`, `index.step`, ...).
    target: Option<String>,
}

impl LogQuery {
    /// As a filter; a 400 for a level that is none.
    fn filter(self) -> Result<LogFilter, Error> {
        let level = self
            .level
            .map(|level| level.parse())
            .transpose()
            .map_err(|_| {
                Error::invalid_request("log level is not error, warn, info, debug or trace")
            })?;
        Ok(LogFilter {
            level,
            target: self.target,
            ..LogFilter::default()
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
    description = "Every kept entry, newest first, a page at a time: its level, event \
        (`target`), message and fields, and its network and job when it has them. Kept 7 \
        days, at most 100 000 per network and as many of no network.",
    params(LogQuery, IndexParams),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The entries", body = ListResponse),
        (status = 400, description = "Malformed query, level, cursor, or limit", body = ErrorBody),
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

/// Whose entries the stream sends, beside [`LogQuery`]'s filters.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct StreamScope {
    /// Only entries of this network, `chain/name` (`bitcoin/regtest`).
    network: Option<String>,
    /// Only entries of this job, its `job_id`.
    job_id: Option<i64>,
}

impl StreamScope {
    /// `filter` narrowed to it; a 400 for a network that is not
    /// `chain/name`.
    fn filter(self, filter: LogFilter) -> Result<LogFilter, Error> {
        let network = match self.network {
            None => None,
            Some(label) => {
                let (chain, name) = label
                    .split_once('/')
                    .ok_or_else(|| Error::invalid_request("network is not chain/name"))?;
                Some(base::Network::new(base::Chain::new(chain), name.to_owned()))
            }
        };
        Ok(LogFilter {
            network,
            job_id: self.job_id,
            ..filter
        })
    }
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
        read: one event per entry, its `data` a `LogEntry` (its `id` 0: not yet stored). A \
        reader too slow for the feed gets `event: missed` with `{\"missed\": n}`, then the \
        entries after. Comments keep the connection alive; the stream does not end.",
    params(LogQuery, StreamScope),
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
    scope: Result<Query<StreamScope>, QueryRejection>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let Query(scope) = scope.map_err(Error::invalid_query)?;
    let filter = scope.filter(query.filter()?)?;
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

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/logs",
    operation_id = "list_network_logs",
    summary = "List a network's kept log entries",
    description = "The network's kept entries (those naming it: its nodes' calls, its index \
        steps, its payments), newest first, a page at a time.",
    params(NetworkPath, LogQuery, IndexParams),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The network's entries", body = ListResponse),
        (status = 400, description = "Malformed query, level, cursor, or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "The network is not configured", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn list_network(
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
    query: Result<Query<LogQuery>, QueryRejection>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let (filter, page) = (query.filter()?, page(index)?);
    let limit = page.limit;
    let logs = logs::network_page(&state.container, &state.db, path.network(), filter, page);
    Ok(Json(ListResponse::new(logs.await?, limit)))
}

/// The job whose entries are read.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct JobPath {
    /// The job's id.
    id: String,
}

#[utoipa::path(
    get,
    path = "/v1/jobs/{id}/logs",
    operation_id = "list_job_logs",
    summary = "List a job's kept log entries",
    description = "Every kept entry of the job's runs (its steps, its nodes' calls), newest \
        first, a page at a time; deleted with the job.",
    params(JobPath, IndexParams),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The job's entries", body = ListResponse),
        (status = 400, description = "Malformed query, cursor, or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Job is unknown", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "jobs"
)]
async fn list_job(
    State(state): State<Arc<AppState>>,
    Path(path): Path<JobPath>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    let page = page(index)?;
    let limit = page.limit;
    let logs = logs::job_page(&state.db, &path.id, page).await?;
    Ok(Json(ListResponse::new(logs, limit)))
}
