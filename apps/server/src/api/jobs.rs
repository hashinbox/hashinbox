//! `GET /v1/jobs/{id}`: one queued job — where it is, why it failed, and how
//! many transactions it signed and the nodes accepted.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{state::AppState, workers::broadcast};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(read))
}

/// The job read.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
struct JobPath {
    /// The id a send returned.
    id: String,
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

/// `GET /v1/jobs/{id}`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = Job)]
struct ReadResponse {
    /// The job id.
    id: String,
    /// The job kind (`send`).
    kind: String,
    /// Where it is.
    state: JobState,
    /// Why it failed, or why its last attempt is being retried; always
    /// present.
    #[schema(required)]
    reason: Option<String>,
    /// How many transactions it signed.
    signed: usize,
    /// How many of them, in broadcast order, the nodes accepted.
    accepted: usize,
}

#[utoipa::path(
    get,
    path = "/v1/jobs/{id}",
    operation_id = "read_job",
    summary = "Read a queued job",
    description = "The job's outcome and how many transactions it signed and the nodes accepted.",
    params(JobPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The job", body = ReadResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Job is unknown", body = ErrorBody),
        (status = 503, description = "The queue is unavailable", body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn read(
    State(state): State<Arc<AppState>>,
    Path(path): Path<JobPath>,
) -> Result<Json<ReadResponse>, Error> {
    let id = tasks::Id::from(path.id);
    let job = tasks::Store::get(&state.storage, &id)
        .await
        .map_err(|error| Error::unavailable(error.to_string()))?
        .ok_or_else(|| Error::not_found("job does not exist"))?;
    let (signed, accepted) = broadcast::progress(job.state.as_ref());
    Ok(Json(ReadResponse {
        id: job.id.to_string(),
        kind: job.kind,
        state: match job.status {
            tasks::Status::Pending => JobState::Pending,
            tasks::Status::Running => JobState::Running,
            tasks::Status::Done => JobState::Succeeded,
            tasks::Status::Failed => JobState::Failed,
        },
        reason: job.reason,
        signed,
        accepted,
    }))
}
