//! `/v1/jobs`: the job queue — every job, where it is, why it failed, what
//! it was given and what its worker has done so far.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, IndexParams};
use crate::{api::AppState, db::Paged as _, models::task, usecase::workers::broadcast};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(read))
}

/// Which jobs `GET /v1/jobs` lists.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct ListQuery {
    /// Only jobs of this chain; with `network`.
    chain: Option<String>,
    /// Only jobs of this network of `chain`.
    network: Option<String>,
    /// Only this kind (`index`, `expire`, `triggers`, `broadcast`, `prune`).
    kind: Option<String>,
    /// Only jobs in this state.
    state: Option<JobState>,
}

impl ListQuery {
    /// The network of `chain` and `network`, both or neither.
    fn network(&self) -> Result<Option<base::Network>, Error> {
        match (&self.chain, &self.network) {
            (Some(chain), Some(network)) => Ok(Some(base::Network::new(
                base::Chain::new(chain),
                network.clone(),
            ))),
            (None, None) => Ok(None),
            _ => Err(Error::invalid_request("chain and network go together")),
        }
    }
}

/// Where a job is.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum JobState {
    /// Waiting for its turn.
    Pending,
    /// Executing.
    Running,
    /// Every transaction was accepted by the network.
    Succeeded,
    /// Failed for good; `reason` says why.
    Failed,
}

impl JobState {
    const fn status(self) -> i32 {
        match self {
            Self::Pending => 0,
            Self::Running => 1,
            Self::Succeeded => 2,
            Self::Failed => 3,
        }
    }
}

/// One job of the queue.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, utoipa::ToSchema)]
struct Job {
    /// The job id.
    id: String,
    /// The job kind: its worker (`index`, `expire`, `triggers`, `broadcast`, `prune`).
    kind: String,
    /// What it locks: jobs whose topics conflict run one at a time, in id order.
    topic: String,
    /// Where it is.
    state: JobState,
    /// Not started before this, Unix ms: the next run of a pending job.
    run_at: u64,
    /// Not started after this, Unix ms; always present.
    #[schema(required)]
    deadline: Option<u64>,
    /// Executions started since it was last fresh.
    attempts: u32,
    /// Why it failed, or why its last attempt is being retried; always
    /// present.
    #[schema(required)]
    reason: Option<String>,
    /// What it was given.
    #[schema(value_type = Object)]
    input: Value,
    /// What its worker has done so far; always present.
    #[schema(value_type = Option<Object>, required)]
    progress: Option<Value>,
    /// A broadcast's transactions signed (0 for other kinds).
    signed: usize,
    /// How many of them, in broadcast order, the nodes accepted.
    accepted: usize,
}

impl From<tasks::Job> for Job {
    fn from(job: tasks::Job) -> Self {
        let (signed, accepted) = broadcast::progress(job.state.as_ref());
        Self {
            id: job.id.to_string(),
            kind: job.kind,
            topic: job.topic.to_string(),
            state: match job.status {
                tasks::Status::Pending => JobState::Pending,
                tasks::Status::Running => JobState::Running,
                tasks::Status::Done => JobState::Succeeded,
                tasks::Status::Failed => JobState::Failed,
            },
            run_at: job.run_at,
            deadline: job.deadline,
            attempts: job.attempts,
            reason: job.reason,
            input: job.input,
            progress: job.state,
            signed,
            accepted,
        }
    }
}

/// `GET /v1/jobs`: a page of jobs.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = JobList)]
struct ListResponse {
    /// The jobs last written, latest first.
    jobs: Vec<Job>,
    /// Pass as `cursor` for the next page; null on the last page. Always
    /// present.
    #[schema(required)]
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/jobs",
    operation_id = "list_jobs",
    summary = "List queued jobs",
    params(ListQuery, IndexParams),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The latest jobs", body = ListResponse),
        (status = 400, description = "Malformed query, cursor, or limit, or a chain without its network", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The queue is unavailable", body = ErrorBody)
    ),
    tag = "jobs"
)]
async fn list(
    State(state): State<Arc<AppState>>,
    query: Result<Query<ListQuery>, QueryRejection>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let Query(index) = index.map_err(Error::invalid_query)?;
    let page = index.page("jobs", |cursor| {
        let (updated_at, id) = cursor.split_once(':')?;
        Some((updated_at.parse().ok()?, id.to_owned()))
    })?;
    let limit = page.limit;
    let status = query.state.map(JobState::status);
    let network = query.network()?;
    let rows = state
        .db
        .read(move |connection| {
            Ok(task::db::find_page(
                connection,
                network.as_ref(),
                query.kind.as_deref(),
                status,
                page,
            )?)
        })
        .await?;
    let next_cursor = rows.cursor(limit);
    let jobs = rows
        .into_iter()
        .map(|(row, _)| tasks::Job::try_from(row).map(Job::from))
        .collect::<anyhow::Result<Vec<_>>>()
        .map_err(|error| Error::unavailable(format!("{error:#}")))?;
    Ok(Json(ListResponse { jobs, next_cursor }))
}

/// The job read.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
struct JobPath {
    /// The id a send returned.
    id: String,
}

#[utoipa::path(
    get,
    path = "/v1/jobs/{id}",
    operation_id = "read_job",
    summary = "Read a queued job",
    description = "The job's outcome, what it was given and what its worker has done; for a \
        broadcast, how many transactions it signed and the nodes accepted.",
    params(JobPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The job", body = Job),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Job is unknown", body = ErrorBody),
        (status = 503, description = "The queue is unavailable", body = ErrorBody)
    ),
    tag = "jobs"
)]
async fn read(
    State(state): State<Arc<AppState>>,
    Path(path): Path<JobPath>,
) -> Result<Json<Job>, Error> {
    let id = tasks::Id::from(path.id);
    let job = tasks::Store::get(&state.queue, &id)
        .await
        .map_err(|error| Error::unavailable(error.to_string()))?
        .ok_or_else(|| Error::not_found("job does not exist"))?;
    Ok(Json(Job::from(job)))
}
