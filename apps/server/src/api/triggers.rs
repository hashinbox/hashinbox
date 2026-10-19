//! `/v1/triggers`: triggers (a selector, an optional filter, a check and an
//! action on one network), their runs, and a preview of what a trigger
//! would see now. The JSON forms of selector, filter, check and action are
//! `crate::models::trigger`'s.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use base::{Decimal, Networked as _};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{
    models::{
        self,
        trigger::{Action, Filter, Gate, Logic},
        wallet_index::Selector,
    },
    state::AppState,
    storage::triggers::PreviewSources,
    trigger::Decimals,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(read, replace, delete))
        .routes(routes!(runs))
        .routes(routes!(preview))
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
    /// Which wallet indexes it acts on: `{"ids": […]}`, `{"asset": "…"}`,
    /// `{"tag": "…"}`, `{"all": […]}`, `{"any": […]}`, `{"not": …}`.
    #[schema(value_type = Object)]
    selector: Selector,
    /// Which of them it acts on: compares under `all`/`any`/`not`; null:
    /// every one.
    #[schema(value_type = Option<Object>)]
    filter: Option<Logic<Filter>>,
    /// When it acts: `"idle"`, `{"since": {"seconds": …}}` and compares under
    /// `all`/`any`/`not`.
    #[schema(value_type = Object)]
    check: Logic<Gate>,
    /// What it does: `{"sweep": {"to": …}}` or
    /// `{"transfer": {"to": …, "amount": …}}`.
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

/// A trigger as written: `POST` (with `id`) and `PUT` bodies.
#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = TriggerDefinition)]
#[serde(deny_unknown_fields)]
struct Definition {
    /// Its slug; `POST` only (`PUT` takes it from the path).
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
    /// Unix ms; null while a send is in flight. Always present.
    #[schema(required)]
    settled_at: Option<i64>,
}

/// `GET /v1/triggers/{id}/runs`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = TriggerRunList)]
struct RunsResponse {
    /// The latest runs, latest first.
    runs: Vec<TriggerRun>,
}

/// `POST /v1/triggers/preview` body: a trigger's targeting, unsaved.
#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = TriggerPreviewRequest)]
#[serde(deny_unknown_fields)]
struct PreviewRequest {
    chain: String,
    network: String,
    #[schema(value_type = Object)]
    selector: Selector,
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    filter: Option<Logic<Filter>>,
    #[schema(value_type = Object)]
    check: Logic<Gate>,
}

/// One wallet index a selector picked.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct Selected {
    /// `<chain>:<network>:<address>`, as the wallet routes take it.
    wallet_id: String,
    /// Address text.
    address: String,
    /// Asset id.
    asset: String,
    /// Confirmed balance in display units.
    confirmed: String,
    /// Not yet confirmed change in display units; negative when leaving.
    pending: String,
}

/// What a trigger would see now.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = TriggerPreview)]
struct PreviewResponse {
    /// The block position balances are read at; null before the first
    /// block. Always present.
    #[schema(required)]
    confirmed: Option<i64>,
    /// What the selector picks, fully indexed, with balances.
    selected: Vec<Selected>,
    /// Whether the filter holds; null without a filter. Always present.
    #[schema(required)]
    filter: Option<bool>,
    /// Whether the check holds for a trigger that never ran.
    check: bool,
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
    let triggers = state.storage.triggers().await?;
    Ok(Json(ListResponse {
        triggers: triggers.into_iter().map(Trigger::from).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/triggers",
    operation_id = "create_trigger",
    summary = "Create a trigger",
    description = "Stores a trigger on a configured network. Its `id` is chosen here and \
        never changes.",
    request_body = Definition,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "Trigger created", body = Trigger),
        (status = 400, description = "Malformed body, id or action address", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network is not configured", body = ErrorBody),
        (status = 422, description = "The id is taken", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn create(
    State(state): State<Arc<AppState>>,
    request: Result<Json<Definition>, JsonRejection>,
) -> Result<(StatusCode, Json<Trigger>), Error> {
    let Json(definition) = request.map_err(Error::invalid_json)?;
    let id = definition
        .id
        .clone()
        .ok_or_else(|| Error::invalid_request("a new trigger needs an id"))?;
    let now = now()?;
    let row = row(&state, id, definition, now, now)?;
    if !state.storage.insert_trigger(row.clone()).await? {
        return Err(Error::unprocessable("a trigger with this id exists"));
    }
    Ok((StatusCode::CREATED, Json(row.into())))
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
    let row = state
        .storage
        .trigger(&path.id)
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
        (status = 400, description = "Malformed body or action address", body = ErrorBody),
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
    let kept = state
        .storage
        .trigger(&path.id)
        .await?
        .ok_or_else(|| Error::not_found("no such trigger"))?;
    let row = row(&state, path.id, definition, kept.created_at, now()?)?;
    if !state.storage.update_trigger(row.clone()).await? {
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
    if !state.storage.delete_trigger(&path.id).await? {
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
    let runs = state.storage.trigger_runs(&path.id, RUNS).await?;
    Ok(Json(RunsResponse {
        runs: runs
            .into_iter()
            .map(|run| TriggerRun {
                started_at: run.started_at,
                settled_at: run.settled_at,
            })
            .collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/triggers/preview",
    operation_id = "preview_trigger",
    summary = "Preview what a trigger would see now",
    description = "Without storing anything: the wallet indexes the selector picks at the last \
        kept block, fully indexed, with balances, and whether the filter and check hold for \
        a trigger that never ran. A fee in the filter or check is estimated.",
    request_body = PreviewRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "What it would see", body = PreviewResponse),
        (status = 400, description = "Malformed body", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network is not configured", body = ErrorBody),
        (status = 503, description = "The store, node or estimator is unavailable",
            body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn preview(
    State(state): State<Arc<AppState>>,
    request: Result<Json<PreviewRequest>, JsonRejection>,
) -> Result<Json<PreviewResponse>, Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let network = base::Network::new(base::Chain::new(request.chain), request.network);
    let service = state.payments.service(&network)?;
    let infos: BTreeMap<String, base::AssetInfo> = service
        .assets
        .clone()
        .into_iter()
        .map(|info| (info.asset.id.clone(), info))
        .collect();
    let sources = PreviewSources {
        decimals: Decimals(
            infos
                .iter()
                .map(|(id, info)| (id.clone(), info.decimals))
                .collect(),
        ),
        estimator: service.wallets.factory.fees(),
        now: now()?,
    };
    let preview = state
        .storage
        .preview(
            network,
            request.selector,
            request.filter,
            request.check,
            sources,
        )
        .await?;
    let display = |info: Option<&base::AssetInfo>, atomic: &Decimal| -> String {
        let Some(info) = info else {
            return atomic.to_string();
        };
        let negative = *atomic < Decimal::zero();
        let magnitude = if negative {
            Decimal::zero() - atomic.clone()
        } else {
            atomic.clone()
        };
        let shown = info
            .display(&magnitude)
            .map_or_else(|_| atomic.to_string(), |value| value.to_string());
        if negative { format!("-{shown}") } else { shown }
    };
    let selected = preview
        .selected
        .values()
        .map(|one| {
            let info = infos.get(&one.index.asset.id);
            let address = &one.index.address;
            Selected {
                wallet_id: format!(
                    "{}:{}:{}",
                    address.network().chain,
                    address.network().name,
                    address.value
                ),
                address: address.value.clone(),
                asset: one.index.asset.id.clone(),
                confirmed: display(info, &one.balance.confirmed),
                pending: display(info, &one.balance.pending),
            }
        })
        .collect();
    Ok(Json(PreviewResponse {
        confirmed: preview.confirmed,
        selected,
        filter: preview.filter,
        check: preview.check,
    }))
}

/// The row `definition` describes as trigger `id`.
///
/// # Errors
///
/// A 400 for a malformed id or action address; a 404 for an unconfigured
/// network.
fn row(
    state: &AppState,
    id: String,
    definition: Definition,
    created_at: i64,
    updated_at: i64,
) -> Result<models::Trigger, Error> {
    if !(1..=64).contains(&id.len())
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(Error::invalid_request("a trigger id is 1 to 64 of [a-z0-9-]"));
    }
    let network = base::Network::new(
        base::Chain::new(&definition.chain),
        definition.network.clone(),
    );
    state.payments.service(&network)?;
    let (Action::Sweep { to } | Action::Transfer { to, .. }) = &definition.action;
    state.payments.parse(&network, to)?;
    Ok(models::Trigger {
        id,
        chain: definition.chain,
        network: definition.network,
        selector: models::Json(definition.selector),
        filter: definition.filter.map(models::Json),
        check: models::Json(definition.check),
        action: models::Json(definition.action),
        enabled: definition.enabled,
        created_at,
        updated_at,
    })
}

/// Unix ms.
fn now() -> Result<i64, Error> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::invalid_response(error.to_string()))?;
    i64::try_from(elapsed.as_millis()).map_err(|error| Error::invalid_response(error.to_string()))
}
