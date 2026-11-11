//! `/v1/triggers`: triggers (a selector, an optional filter, a check and an
//! action on one network) and their runs. The JSON forms of selector,
//! filter, check and action are `crate::models::trigger`'s.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
};
use diesel::OptionalExtension as _;
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, IndexParams, jobs::JobState};
use crate::{
    api::AppState,
    db::Paged as _,
    models::{
        self, Trigger,
        trigger::{self as triggers, Action, Filter, Gate, Logic, Stats},
        trigger_run,
        wallet_index_selector::Selector,
    },
    usecase::workers::trigger::actions::SendInput,
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
    let triggers = state
        .db
        .read(move |connection| Ok(triggers::db::find_page(connection, page)?))
        .await?;
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
    check: Logic<Gate>,
    #[schema(value_type = Object)]
    action: Action,
    /// Absent means `true`.
    #[serde(default = "enabled")]
    enabled: bool,
}

fn enabled() -> bool {
    true
}

impl Definition {
    /// The trigger this definition describes as trigger `id`; a 400 for one
    /// the model refuses ([`models::Trigger::validate`]) or a send from `This`
    /// to `This`, a 404 for an unconfigured network or sent asset.
    fn trigger(
        self,
        payments: &payments::Payments,
        id: String,
        group_id: String,
        created_at: i64,
        updated_at: i64,
    ) -> Result<models::Trigger, Error> {
        let network = base::Network::new(base::Chain::new(&self.chain), self.network.clone());
        payments.service(&network)?;
        if self.action.name != "send" {
            return Err(Error::invalid_request(format!(
                "unknown action {}",
                self.action.name
            )));
        }
        let send = SendInput::deserialize(&self.action.input)
            .map_err(|error| Error::invalid_request(format!("send input: {error}")))?;
        super::asset_info(payments, &network, &send.asset)?;
        send.validate()
            .map_err(|invalid| Error::invalid_request(invalid.to_string()))?;
        let trigger = models::Trigger {
            id,
            chain: self.chain,
            network: self.network,
            group_id,
            selector: models::Json(self.selector),
            filter: self.filter.map(models::Json),
            check: models::Json(self.check),
            action: models::Json(self.action),
            enabled: self.enabled,
            created_at,
            updated_at,
            last_round: None,
        };
        // The model's own rules, before it is ever kept.
        trigger
            .validate()
            .map_err(|invalid| Error::invalid_request(invalid.to_string()))?;
        Ok(trigger)
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

impl BatchRequest {
    /// The batch's triggers in group `group_id`: 1 to 16, each with its own
    /// `id`, all on one network.
    fn triggers(
        self,
        payments: &payments::Payments,
        group_id: &str,
        now: i64,
    ) -> Result<Vec<models::Trigger>, Error> {
        if self.triggers.is_empty() || self.triggers.len() > 16 {
            return Err(Error::invalid_request("a batch holds 1 to 16 triggers"));
        }
        let mut rows = Vec::with_capacity(self.triggers.len());
        for definition in self.triggers {
            let id = definition
                .id
                .clone()
                .ok_or_else(|| Error::invalid_request("every trigger of a batch needs an id"))?;
            if rows.iter().any(|row: &models::Trigger| row.id == id) {
                return Err(Error::unprocessable("a batch repeats a trigger id"));
            }
            if rows.first().is_some_and(|first: &models::Trigger| {
                (first.chain.as_str(), first.network.as_str())
                    != (definition.chain.as_str(), definition.network.as_str())
            }) {
                return Err(Error::invalid_request(
                    "every trigger of a batch is on one network",
                ));
            }
            rows.push(definition.trigger(payments, id, group_id.to_owned(), now, now)?);
        }
        Ok(rows)
    }
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
    let now = now()?;
    // One group for the whole batch, so it can be found (and removed) together.
    let group = uuid::Uuid::now_v7().to_string();
    let rows = request.triggers(&state.payments, &group, now)?;
    let kept = rows.clone();
    let taken = state
        .db
        .write(move |connection| {
            // Every id is checked before any is written: all or none.
            for row in &kept {
                if triggers::db::get_by_id(connection, &row.id)
                    .optional()?
                    .is_some()
                {
                    return Ok(Some(row.id.clone()));
                }
            }
            for row in &kept {
                triggers::db::insert(connection, row)?;
            }
            Ok(None)
        })
        .await?;
    if let Some(taken) = taken {
        return Err(Error::unprocessable(format!(
            "a trigger with id {taken} exists"
        )));
    }
    Ok((StatusCode::CREATED, Json(CreateResponse { triggers: rows })))
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
    let id = path.id;
    let row = state
        .db
        .read(move |connection| Ok(triggers::db::get_by_id(connection, &id).optional()?))
        .await?
        .ok_or_else(|| Error::not_found("no such trigger"))?;
    Ok(Json(row))
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
    if definition.id.is_some() {
        return Err(Error::invalid_request("a trigger's id never changes"));
    }
    let id = path.id.clone();
    let kept = state
        .db
        .read(move |connection| Ok(triggers::db::get_by_id(connection, &id).optional()?))
        .await?
        .ok_or_else(|| Error::not_found("no such trigger"))?;
    // A replace keeps its id, creation time and group.
    let row = definition.trigger(
        &state.payments,
        path.id,
        kept.group_id,
        kept.created_at,
        now()?,
    )?;
    let replaced = row.clone();
    let changed = state
        .db
        .write(move |connection| Ok(triggers::db::update(connection, &replaced)?))
        .await?;
    if changed == 0 {
        return Err(Error::not_found("no such trigger"));
    }
    Ok(Json(row))
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
    let id = path.id;
    let deleted = state
        .db
        .write(move |connection| Ok(triggers::db::delete(connection, &id)?))
        .await?;
    if deleted == 0 {
        return Err(Error::not_found("no such trigger"));
    }
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

/// Where a run's jobs are together.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum RunOutcome {
    /// One still waits or runs.
    Pending,
    /// Every one succeeded.
    Succeeded,
    /// None succeeded.
    Failed,
    /// Some succeeded, some did not.
    Mixed,
}

impl RunOutcome {
    /// Where `states` are together; `None` for none.
    fn of(states: &[JobState]) -> Option<Self> {
        let succeeded = states.iter().filter(|state| **state == JobState::Succeeded);
        let unfinished = states
            .iter()
            .any(|state| matches!(state, JobState::Pending | JobState::Running));
        Some(match succeeded.count() {
            _ if states.is_empty() => return None,
            _ if unfinished => Self::Pending,
            all if all == states.len() => Self::Succeeded,
            0 => Self::Failed,
            _ => Self::Mixed,
        })
    }
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
    let id = path.id;
    let runs = state
        .db
        .read(move |connection| Ok(trigger_run::db::find_page(connection, &id, page)?))
        .await?;
    Ok(Json(RunsResponse {
        next_cursor: runs.cursor(limit),
        runs: runs.into_iter().map(TriggerRun::from).collect(),
    }))
}

impl From<trigger_run::Run> for TriggerRun {
    fn from(run: trigger_run::Run) -> Self {
        let jobs: Vec<RunJob> = run
            .jobs
            .into_iter()
            .map(|job| RunJob {
                id: job.id.to_string(),
                state: job.status.into(),
                reason: job.reason,
            })
            .collect();
        let states: Vec<JobState> = jobs.iter().map(|job| job.state).collect();
        Self {
            started_at: run.started_at,
            stats: run.stats,
            outcome: RunOutcome::of(&states),
            jobs,
        }
    }
}

/// Unix ms.
fn now() -> Result<i64, Error> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::invalid_response(error.to_string()))?;
    i64::try_from(elapsed.as_millis()).map_err(|error| Error::invalid_response(error.to_string()))
}
