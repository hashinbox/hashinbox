//! `/v1/triggers`: triggers (a selector, an optional filter, a check and an
//! action on one network) and their runs. The JSON forms of selector,
//! filter, check and action are `crate::models::trigger`'s.

use std::sync::Arc;

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use ::trigger::{Stats, actions::Call, conditions::Logic, selection::Selector};

use super::{Error, ErrorBody, IndexParams, jobs::JobState};
use crate::{
    api::AppState,
    db::Paged as _,
    models::{
        Trigger,
        trigger::{Check, Filter},
        trigger_run::{Run, RunOutcome},
    },
    usecase::triggers,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(read, replace, delete))
        .routes(routes!(runs))
}

/// `GET /v1/triggers`: a page of triggers.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = TriggerList)]
struct ListResponse {
    /// Triggers in id order.
    triggers: Vec<Trigger>,
    /// Pass as `cursor` for the next page; null on the last page. Always
    /// present.
    #[schema(required)]
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/triggers",
    operation_id = "list_triggers",
    summary = "List triggers",
    params(IndexParams),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Triggers in id order", body = ListResponse),
        (status = 400, description = "Malformed query or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn list(
    State(state): State<Arc<AppState>>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    let Query(index) = index.map_err(Error::invalid_query)?;
    let page = index.page("triggers", |cursor| Some(cursor.to_owned()))?;
    let limit = page.limit;
    let triggers = triggers::list_page(&state.db, page).await?;
    Ok(Json(ListResponse {
        next_cursor: triggers.cursor(limit),
        triggers,
    }))
}

/// A trigger as written: in a `POST` (with `id`) or as a `PUT` body.
#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = TriggerDefinition)]
#[serde(deny_unknown_fields)]
struct Definition {
    /// Its slug; required in a `POST`, refused in a `PUT` (the path names it).
    #[serde(default)]
    #[schema(nullable = false)]
    id: Option<String>,
    chain: String,
    network: String,
    #[schema(value_type = Object)]
    selector: Selector,
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    filter: Option<Logic<Filter>>,
    #[schema(value_type = Object)]
    check: Logic<Check>,
    #[schema(value_type = Object)]
    action: Call,
    /// Absent means `true` in a `POST` and the stored value in a `PUT`.
    #[serde(default)]
    #[schema(nullable = false)]
    enabled: Option<bool>,
}

impl From<Definition> for triggers::Definition {
    fn from(definition: Definition) -> Self {
        Self {
            id: definition.id,
            chain: definition.chain,
            network: definition.network,
            selector: definition.selector,
            filter: definition.filter,
            check: definition.check,
            action: definition.action,
            enabled: definition.enabled,
        }
    }
}

/// `POST /v1/triggers` body: triggers of one network stored together, all or none.
#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = TriggerBatch)]
#[serde(deny_unknown_fields)]
struct BatchRequest {
    /// One to 16 triggers, each with its `id`.
    #[schema(min_items = 1, max_items = 16)]
    triggers: Vec<Definition>,
}

/// `POST /v1/triggers`: the triggers created.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = TriggerBatchCreated)]
struct CreateResponse {
    /// The batch's triggers, in its order.
    triggers: Vec<Trigger>,
}

#[utoipa::path(
    post,
    path = "/v1/triggers",
    operation_id = "create_triggers",
    summary = "Create triggers",
    description = "Stores every trigger or none, all on one configured network, each with an \
        `id` chosen here that never changes: a template of several triggers (gas funding and \
        sweep) never lands half.",
    request_body = BatchRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "Triggers created", body = CreateResponse),
        (status = 400, description = "Malformed body or id, several networks, or a send from \
            This to This", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network or sent asset is not configured",
            body = ErrorBody),
        (status = 422, description = "An id is taken or repeated", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn create(
    State(state): State<Arc<AppState>>,
    request: Result<Json<BatchRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CreateResponse>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let definitions = request.triggers.into_iter().map(Into::into).collect();
    let (container, db, actions) = (&state.container, &state.db, &state.actions);
    let triggers = triggers::create(container, db, actions, definitions).await?;
    Ok((StatusCode::CREATED, Json(CreateResponse { triggers })))
}

/// The trigger a route acts on.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct TriggerPath {
    /// The trigger's slug.
    id: String,
}

#[utoipa::path(
    get,
    path = "/v1/triggers/{id}",
    operation_id = "read_trigger",
    summary = "Read a trigger",
    params(TriggerPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The trigger", body = Trigger),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "No such trigger", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn read(
    State(state): State<Arc<AppState>>,
    Path(path): Path<TriggerPath>,
) -> Result<Json<Trigger>, Error> {
    Ok(Json(triggers::read(&state.db, path.id).await?))
}

#[utoipa::path(
    put,
    path = "/v1/triggers/{id}",
    operation_id = "replace_trigger",
    summary = "Replace a trigger",
    description = "Replaces everything but its id and creation time; a body `id` is refused. \
        Setting `enabled` to false pauses it.",
    params(TriggerPath),
    request_body = Definition,
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Trigger replaced", body = Trigger),
        (status = 400, description = "Malformed body, or a send from This to This",
            body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "No such trigger, or network not configured",
            body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn replace(
    State(state): State<Arc<AppState>>,
    Path(path): Path<TriggerPath>,
    request: Result<Json<Definition>, JsonRejection>,
) -> Result<Json<Trigger>, Error> {
    let Json(definition) = request.map_err(Error::invalid_json)?;
    let (container, db, actions) = (&state.container, &state.db, &state.actions);
    let trigger = triggers::replace(container, db, actions, path.id, definition.into());
    Ok(Json(trigger.await?))
}

#[utoipa::path(
    delete,
    path = "/v1/triggers/{id}",
    operation_id = "delete_trigger",
    summary = "Delete a trigger and its runs",
    params(TriggerPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 204, description = "Trigger deleted"),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "No such trigger", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn delete(
    State(state): State<Arc<AppState>>,
    Path(path): Path<TriggerPath>,
) -> Result<StatusCode, Error> {
    triggers::delete(&state.db, path.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// One run of a trigger: when it started, the send jobs it queued and how
/// they ended.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct TriggerRun {
    /// Unix ms.
    started_at: i64,
    /// What its round saw: wallets selected, kept by the filter, whether
    /// the check held, and send jobs queued.
    stats: Stats,
    /// Where its jobs are together; null when none of them is kept.
    #[schema(required)]
    outcome: Option<RunOutcome>,
    /// Its send jobs, in the order queued; a job no longer kept is left out.
    jobs: Vec<RunJob>,
}

/// One send job of a run.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct RunJob {
    /// The job id: `GET /v1/jobs/{id}` reads it.
    id: String,
    state: JobState,
    /// Why it failed, or why its last attempt is being retried; always
    /// present.
    #[schema(required)]
    reason: Option<String>,
}

/// `GET /v1/triggers/{id}/runs`: a page of runs.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = TriggerRunList)]
struct RunsResponse {
    /// Runs, latest first.
    runs: Vec<TriggerRun>,
    /// Pass as `cursor` for the next page; null on the last page. Always
    /// present.
    #[schema(required)]
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/triggers/{id}/runs",
    operation_id = "list_trigger_runs",
    summary = "List a trigger's latest runs",
    params(TriggerPath, IndexParams),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Its runs, latest first", body = RunsResponse),
        (status = 400, description = "Malformed query, cursor, or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn runs(
    State(state): State<Arc<AppState>>,
    Path(path): Path<TriggerPath>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<RunsResponse>, Error> {
    let Query(index) = index.map_err(Error::invalid_query)?;
    let page = index.page("runs", |cursor| cursor.parse().ok())?;
    let limit = page.limit;
    let runs = triggers::runs_page(&state.db, path.id, page).await?;
    Ok(Json(RunsResponse {
        next_cursor: runs.cursor(limit),
        runs: runs.into_iter().map(TriggerRun::from).collect(),
    }))
}

impl From<Run> for TriggerRun {
    fn from(run: Run) -> Self {
        Self {
            started_at: run.started_at,
            outcome: run.outcome(),
            stats: run.stats,
            jobs: run
                .jobs
                .into_iter()
                .map(|job| RunJob {
                    id: job.id.to_string(),
                    state: job.status.into(),
                    reason: job.reason,
                })
                .collect(),
        }
    }
}
