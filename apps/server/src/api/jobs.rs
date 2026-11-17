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
use crate::{api::AppState, db::Paged as _, usecase::jobs};

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
    /// Only this kind (`index`, `triggers`, `broadcast`, `prune`).
    kind: Option<String>,
    /// Only jobs in this state; absent, every state but `idle`.
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
pub(super) enum JobState {
    /// Waiting for its turn.
    Pending,
    /// Executing.
    Running,
    /// Done; for a broadcast, its transaction is mined in a final block.
    Succeeded,
    /// Failed for good; `reason` says why.
    Failed,
    /// Finished without running; `reason` names the job it gave way to.
    Skipped,
    /// Succeeded, and had nothing to do; listed only when asked for.
    Idle,
}

impl From<tasks::Status> for JobState {
    fn from(status: tasks::Status) -> Self {
        match status {
            tasks::Status::Pending => Self::Pending,
            tasks::Status::Running => Self::Running,
            tasks::Status::Done => Self::Succeeded,
            tasks::Status::Failed => Self::Failed,
            tasks::Status::Skipped => Self::Skipped,
            tasks::Status::Idle => Self::Idle,
        }
    }
}

impl From<JobState> for tasks::Status {
    fn from(state: JobState) -> Self {
        match state {
            JobState::Pending => Self::Pending,
            JobState::Running => Self::Running,
            JobState::Succeeded => Self::Done,
            JobState::Failed => Self::Failed,
            JobState::Skipped => Self::Skipped,
            JobState::Idle => Self::Idle,
        }
    }
}

/// One job of the queue.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, utoipa::ToSchema)]
struct Job {
    /// The job id.
    id: String,
    /// The job kind: its worker (`index`, `triggers`, `broadcast`, `prune`).
    kind: String,
    /// What it locks: jobs whose topics conflict run one at a time, in line order.
    topic: String,
    /// Where it is.
    state: JobState,
    /// Not started before this, Unix ms: the next run of a pending job.
    run_at: u64,
    /// When it was queued, Unix ms.
    created_at: i64,
    /// When it was last written, Unix ms: started, retried or finished.
    updated_at: i64,
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
    /// A broadcast's transaction (txid), once recorded; always present,
    /// null before and for other kinds.
    #[schema(required)]
    txid: Option<String>,
    /// Whether it was sent: a node accepted it, or may hold it (false for other kinds).
    accepted: bool,
}

impl From<jobs::Job> for Job {
    fn from(kept: jobs::Job) -> Self {
        let job = kept.job;
        Self {
            id: job.id.to_string(),
            kind: job.kind,
            topic: job.topic.to_string(),
            state: job.status.into(),
            run_at: job.run_at,
            created_at: kept.created_at,
            updated_at: kept.updated_at,
            deadline: job.deadline,
            attempts: job.attempts,
            reason: job.reason,
            input: job.input,
            progress: job.state,
            txid: kept.txid,
            accepted: kept.accepted,
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
        Some((updated_at.parse().ok()?, id.parse().ok()?))
    })?;
    let limit = page.limit;
    let (network, status) = (query.network()?, query.state.map(Into::into));
    let jobs = jobs::list_page(&state.db, network, query.kind, status, page).await?;
    let next_cursor = jobs.cursor(limit);
    let jobs = jobs.into_iter().map(Job::from).collect();
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
        broadcast, the transaction it recorded and whether it was sent.",
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
    Ok(Json(jobs::read(&state.db, &path.id).await?.into()))
}
