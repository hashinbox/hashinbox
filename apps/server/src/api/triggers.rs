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
use diesel::OptionalExtension as _;
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{
    models::{
        self,
        trigger::{self as triggers, Action, Filter, Gate, Logic, Target},
        trigger_run,
        wallet_index::Selector,
    },
    state::AppState,
    trigger::{self, Definition as Unsaved, Sources},
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
    /// What it does: `{"send": {"source": target, "asset": id,
    /// "destination": target, "amount": amount}}`.
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

/// `POST /v1/triggers/preview` body: a trigger, unsaved.
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
    #[schema(value_type = Object)]
    action: Action,
}

/// One transfer a run would make.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct PlannedTransfer {
    /// Sender address text.
    from: String,
    /// Recipient address text.
    to: String,
    /// Display units of the send's asset.
    amount: String,
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
    /// Whether the filter keeps it (it is then `This` for check and action).
    kept: bool,
}

/// What a trigger would see now.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = TriggerPreview)]
struct PreviewResponse {
    /// The block position balances are read at; null before the first
    /// block. Always present.
    #[schema(required)]
    confirmed: Option<i64>,
    /// What the selector picks, fully indexed, with balances, each marked
    /// kept or not by the filter.
    selected: Vec<Selected>,
    /// Whether the check holds over the kept ones, for a trigger that never ran.
    check: bool,
    /// The transfers the action would make now; empty when nothing is due.
    transfers: Vec<PlannedTransfer>,
    /// Why the action cannot be planned; null when it can. Always present.
    #[schema(required)]
    plan_error: Option<String>,
    /// Senders of `transfers` this server holds no key for; a run refuses them.
    unkeyed: Vec<String>,
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
        .storage
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
        rows.push(row(&state, id, definition, now, now)?);
    }
    let kept = rows.clone();
    let taken = state
        .storage
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
        .storage
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
    let created_at = state
        .storage
        .read(move |connection| Ok(triggers::db::get_by_id(connection, &id).optional()?))
        .await?
        .ok_or_else(|| Error::not_found("no such trigger"))?
        .created_at;
    let row = row(&state, path.id, definition, created_at, now()?)?;
    let replaced = row.clone();
    let changed = state
        .storage
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
        .storage
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
        .storage
        .read(move |connection| Ok(trigger_run::db::find_recent(connection, &id, RUNS)?))
        .await?;
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
    description = "Without storing anything, at the last kept block: the wallet indexes the \
        selector picks (fully indexed, with balances), which of them the filter keeps, whether \
        the check holds over those for a trigger that never ran, and the transfers the action \
        would make. Fees used are estimated.",
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
    let sources = Sources::new(
        service.assets.clone(),
        service.wallets.factory.fees(),
        now()?,
    );
    let send_asset = {
        let Action::Send(send) = &request.action;
        send.asset.clone()
    };
    let definition = Unsaved {
        selector: request.selector,
        filter: request.filter,
        check: request.check,
        action: request.action,
    };
    let preview = state
        .storage
        .read(move |connection| trigger::preview(connection, &network, &definition, &sources))
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
                kept: preview.kept.contains_key(&one.index.id),
            }
        })
        .collect();
    let (transfers, plan_error) = match preview.plan {
        Ok(plan) => (plan.transfers, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let sent = infos.get(&send_asset);
    Ok(Json(PreviewResponse {
        confirmed: preview.confirmed,
        selected,
        check: preview.check,
        transfers: transfers
            .into_iter()
            .map(|transfer| PlannedTransfer {
                from: transfer.from.value,
                to: transfer.to.value,
                amount: display(sent, &transfer.amount),
            })
            .collect(),
        plan_error,
        unkeyed: preview
            .unkeyed
            .into_iter()
            .map(|address| address.value)
            .collect(),
    }))
}

/// The row `definition` describes as trigger `id`.
///
/// # Errors
///
/// A 400 for a malformed id or a send from `This` to `This`; a 404 for an
/// unconfigured network or sent asset.
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
        return Err(Error::invalid_request(
            "a trigger id is 1 to 64 of [a-z0-9-]",
        ));
    }
    let network = base::Network::new(
        base::Chain::new(&definition.chain),
        definition.network.clone(),
    );
    state.payments.service(&network)?;
    let Action::Send(send) = &definition.action;
    super::asset_info(&state.payments, &network, &send.asset)?;
    if matches!(
        (&send.source, &send.destination),
        (Target::This(_), Target::This(_))
    ) {
        return Err(Error::invalid_request(
            "a send from This to This is many to many",
        ));
    }
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
