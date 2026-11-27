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

use crate::models::Caller;
use crate::models::Session;

use ::trigger::{Stats, actions::Call, conditions::Logic, selection::Selector};

use super::jobs::JobState;
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
use crate::{api::IndexParams, error::Error};

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

#[scope::utoipa(
    get,
    path = "/v1/triggers",
    operation_id = "triggers_list",
    summary = "List triggers",
    params(IndexParams),
    responses(
        (status = 200, description = "Triggers in id order", body = ListResponse),
        (status = 400, description = "Malformed query or limit", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "triggers"
)]
async fn list(
    session: Session,
    State(state): State<Arc<AppState>>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    scope::require!(session, "com.hashinbox.triggers:read");
    let Query(index) = index?;
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

#[scope::utoipa(
    post,
    path = "/v1/triggers",
    operation_id = "triggers_create",
    summary = "Create triggers",
    description = "Stores every trigger or none, all on one configured network, each with an \
        `id` chosen here that never changes: a template of several triggers (gas funding and \
        sweep) never lands half.",
    request_body = BatchRequest,
    responses(
        (status = 201, description = "Triggers created", body = CreateResponse),
        (status = 400, description = "Malformed body or id, a repeated id, several networks, \
            or a send from This to This", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 404, description = "Network or sent asset is not configured",
            body = Error),
        (status = 409, description = "An id is taken", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "triggers"
)]
async fn create(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    request: Result<Json<BatchRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CreateResponse>), Error> {
    scope::require!(caller, "com.hashinbox.triggers");
    let Json(request) = request?;
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

#[scope::utoipa(
    get,
    path = "/v1/triggers/{id}",
    operation_id = "trigger_get",
    summary = "Read a trigger",
    params(TriggerPath),
    responses(
        (status = 200, description = "The trigger", body = Trigger),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 404, description = "No such trigger", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "triggers"
)]
async fn read(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(path): Path<TriggerPath>,
) -> Result<Json<Trigger>, Error> {
    scope::require!(session, "com.hashinbox.triggers:read");
    Ok(Json(triggers::read(&state.db, path.id).await?))
}

#[scope::utoipa(
    put,
    path = "/v1/triggers/{id}",
    operation_id = "trigger_replace",
    summary = "Replace a trigger",
    description = "Replaces everything but its id and creation time; a body `id` is refused. \
        Setting `enabled` to false pauses it.",
    params(TriggerPath),
    request_body = Definition,
    responses(
        (status = 200, description = "Trigger replaced", body = Trigger),
        (status = 400, description = "Malformed body, or a send from This to This",
            body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 404, description = "No such trigger, or network not configured",
            body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "triggers"
)]
async fn replace(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    Path(path): Path<TriggerPath>,
    request: Result<Json<Definition>, JsonRejection>,
) -> Result<Json<Trigger>, Error> {
    scope::require!(caller, "com.hashinbox.triggers");
    let Json(definition) = request?;
    let (container, db, actions) = (&state.container, &state.db, &state.actions);
    let trigger = triggers::replace(container, db, actions, path.id, definition.into());
    Ok(Json(trigger.await?))
}

#[scope::utoipa(
    delete,
    path = "/v1/triggers/{id}",
    operation_id = "trigger_delete",
    summary = "Delete a trigger and its runs",
    params(TriggerPath),
    responses(
        (status = 204, description = "Trigger deleted"),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 404, description = "No such trigger", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "triggers"
)]
async fn delete(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    Path(path): Path<TriggerPath>,
) -> Result<StatusCode, Error> {
    scope::require!(caller, "com.hashinbox.triggers");
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

#[scope::utoipa(
    get,
    path = "/v1/triggers/{id}/runs",
    operation_id = "trigger_runs_list",
    summary = "List a trigger's latest runs",
    params(TriggerPath, IndexParams),
    responses(
        (status = 200, description = "Its runs, latest first", body = RunsResponse),
        (status = 400, description = "Malformed query, cursor, or limit", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "triggers"
)]
async fn runs(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(path): Path<TriggerPath>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<RunsResponse>, Error> {
    scope::require!(session, "com.hashinbox.triggers.runs:read");
    let Query(index) = index?;
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
