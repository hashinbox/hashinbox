//! `/v1/triggers`: triggers (a selector, an optional filter, a check and an
//! action on one network) and their runs. The JSON forms of selector,
//! filter, check and action are `crate::models::trigger`'s.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use diesel::OptionalExtension as _;
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{
    api::AppState,
    models::{
        self,
        trigger::{self as triggers, Action, Filter, Gate, Logic},
        trigger_run,
        wallet_index::Selector,
    },
    usecase::workers::trigger::actions::SendInput,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(read, replace, delete))
        .routes(routes!(runs))
}

/// How many runs `GET /v1/triggers/{id}/runs` answers.
const RUNS: i64 = 50;

/// One trigger.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct Trigger {
    /// Its slug: 1 to 64 of `[a-z0-9-]`.
    id: String,
    /// Chain name, as `GET /v1/assets` lists it.
    chain: String,
    /// Network name, as `GET /v1/assets` lists it.
    network: String,
    /// The batch it was created in: every trigger of one create shares it.
    group: String,
    /// Which wallet indexes it acts on: `{"ids": […]}`, `{"asset": "…"}`,
    /// `{"tag": "…"}`, `{"untagged": {}}`, `{"all": […]}`, `{"any": […]}`, `{"not": …}`.
    #[schema(value_type = Object)]
    selector: Selector,
    /// Which of them it acts on: compares under `all`/`any`/`not`; null:
    /// every one.
    #[schema(value_type = Option<Object>)]
    filter: Option<Logic<Filter>>,
    /// When it acts: `{"since": {"seconds": …}}` and compares under
    /// `all`/`any`/`not`.
    #[schema(value_type = Object)]
    check: Logic<Gate>,
    /// What it does: `{"name": "send", "input": {"source": target, "asset":
    /// id, "destination": target, "amount": amount}}`.
    #[schema(value_type = Object)]
    action: Action,
    /// Whether it runs.
    enabled: bool,
    /// Unix ms.
    created_at: i64,
    /// Unix ms.
    updated_at: i64,
}

impl From<models::Trigger> for Trigger {
    fn from(row: models::Trigger) -> Self {
        Self {
            id: row.id,
            chain: row.chain,
            network: row.network,
            group: row.group_id,
            selector: row.selector.0,
            filter: row.filter.map(|filter| filter.0),
            check: row.check.0,
            action: row.action.0,
            enabled: row.enabled,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
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

/// `POST /v1/triggers` body: triggers of one network stored together, all or none.
#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = TriggerBatch)]
#[serde(deny_unknown_fields)]
struct BatchRequest {
    /// One to 16 triggers, each with its `id`.
    #[schema(min_items = 1, max_items = 16)]
    triggers: Vec<Definition>,
}

/// `GET /v1/triggers`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = TriggerList)]
struct ListResponse {
    /// Triggers in id order.
    triggers: Vec<Trigger>,
}

/// The trigger a route acts on.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct TriggerPath {
    /// The trigger's slug.
    id: String,
}

/// One run of a trigger.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct TriggerRun {
    /// Unix ms.
    started_at: i64,
}

/// `GET /v1/triggers/{id}/runs`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = TriggerRunList)]
struct RunsResponse {
    /// The latest runs, latest first.
    runs: Vec<TriggerRun>,
}

#[utoipa::path(
    get,
    path = "/v1/triggers",
    operation_id = "list_triggers",
    summary = "List triggers",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Every trigger", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn list(State(state): State<Arc<AppState>>) -> Result<Json<ListResponse>, Error> {
    let triggers = state
        .db
        .read(|connection| Ok(triggers::db::find_all(connection)?))
        .await?;
    Ok(Json(ListResponse {
        triggers: triggers.into_iter().map(Trigger::from).collect(),
    }))
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
        (status = 201, description = "Triggers created", body = ListResponse),
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
) -> Result<(StatusCode, Json<ListResponse>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    if request.triggers.is_empty() || request.triggers.len() > 16 {
        return Err(Error::invalid_request("a batch holds 1 to 16 triggers"));
    }
    let now = now()?;
    // One group for the whole batch, so it can be found (and removed) together.
    let group = uuid::Uuid::now_v7().to_string();
    let mut rows = Vec::with_capacity(request.triggers.len());
    for definition in request.triggers {
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
        rows.push(row(&state, id, group.clone(), definition, now, now)?);
    }
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
    Ok((
        StatusCode::CREATED,
        Json(ListResponse {
            triggers: rows.into_iter().map(Trigger::from).collect(),
        }),
    ))
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
    Ok(Json(row.into()))
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
    let row = row(
        &state,
        path.id,
        kept.group_id,
        definition,
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
    Ok(Json(row.into()))
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

#[utoipa::path(
    get,
    path = "/v1/triggers/{id}/runs",
    operation_id = "list_trigger_runs",
    summary = "List a trigger's latest runs",
    params(TriggerPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Its latest 50 runs, latest first", body = RunsResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn runs(
    State(state): State<Arc<AppState>>,
    Path(path): Path<TriggerPath>,
) -> Result<Json<RunsResponse>, Error> {
    let id = path.id;
    let runs = state
        .db
        .read(move |connection| Ok(trigger_run::db::find_recent(connection, &id, RUNS)?))
        .await?;
    Ok(Json(RunsResponse {
        runs: runs
            .into_iter()
            .map(|run| TriggerRun {
                started_at: run.started_at,
            })
            .collect(),
    }))
}

/// The row `definition` describes as trigger `id`.
///
/// # Errors
///
/// A 400 for a trigger the model refuses ([`models::Trigger::validate`]) or a
/// send from `This` to `This`; a 404 for an
/// unconfigured network or sent asset.
fn row(
    state: &AppState,
    id: String,
    group_id: String,
    definition: Definition,
    created_at: i64,
    updated_at: i64,
) -> Result<models::Trigger, Error> {
    let network = base::Network::new(
        base::Chain::new(&definition.chain),
        definition.network.clone(),
    );
    state.payments.service(&network)?;
    if definition.action.name != "send" {
        return Err(Error::invalid_request(format!(
            "unknown action {}",
            definition.action.name
        )));
    }
    let send = SendInput::deserialize(&definition.action.input)
        .map_err(|error| Error::invalid_request(format!("send input: {error}")))?;
    super::asset_info(&state.payments, &network, &send.asset)?;
    send.validate()
        .map_err(|invalid| Error::invalid_request(invalid.to_string()))?;
    let trigger = models::Trigger {
        id,
        chain: definition.chain,
        network: definition.network,
        group_id,
        selector: models::Json(definition.selector),
        filter: definition.filter.map(models::Json),
        check: models::Json(definition.check),
        action: models::Json(definition.action),
        enabled: definition.enabled,
        created_at,
        updated_at,
    };
    // The model's own rules, before it is ever kept.
    trigger
        .validate()
        .map_err(|invalid| Error::invalid_request(invalid.to_string()))?;
    Ok(trigger)
}

/// Unix ms.
fn now() -> Result<i64, Error> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::invalid_response(error.to_string()))?;
    i64::try_from(elapsed.as_millis()).map_err(|error| Error::invalid_response(error.to_string()))
}
