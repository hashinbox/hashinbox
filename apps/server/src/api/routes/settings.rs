//! `/v1/settings`: the instance's settings, each declared in code
//! (`models::setting`). List them (a secret one only as set or not), write
//! one (checked by its declaration; a secret one never).

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::models::Caller;
use crate::{
    api::AppState,
    error::Error,
    models::{
        Session,
        setting::{Listed, Setting},
    },
    usecase::settings,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(write))
}

/// `GET /v1/settings`.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = SettingList)]
struct ListResponse {
    /// Every setting, in declaration order: a secret one as `{key, set,
    /// description}` only.
    settings: Vec<Listed>,
}

#[scope::utoipa(
    get,
    path = "/v1/settings",
    operation_id = "list_settings",
    summary = "List the settings",
    description = "Every setting the server declares: its key, value (its default when never \
        written), default, whether it is public, description and type. A secret setting \
        (`recovery_passphrase`) shows only its key, whether it is set and its description.",
    responses(
        (status = 200, description = "Every setting", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "settings"
)]
async fn list(
    session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ListResponse>, Error> {
    scope::require!(session, "com.hashinbox.settings.$read");
    Ok(Json(ListResponse {
        settings: settings::list(&state.db).await?,
    }))
}

/// The setting a route acts on.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct SettingPath {
    /// Its key, as `GET /v1/settings` lists it.
    key: String,
}

/// `PUT /v1/settings/{key}` body.
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = WriteSetting)]
#[serde(deny_unknown_fields)]
struct WriteRequest {
    /// Its new value: JSON of the setting's type.
    value: Value,
}

#[scope::utoipa(
    put,
    path = "/v1/settings/{key}",
    operation_id = "write_setting",
    summary = "Write a setting",
    description = "Stores the setting's value once it parses as the setting's type and passes \
        its check. Whether it is public is the declaration's, never the request's. A secret \
        setting is never written here: 403 `permission_denied`.",
    params(SettingPath),
    request_body = WriteRequest,
    responses(
        (status = 200, description = "Stored", body = Setting),
        (status = 400, description = "Malformed body, unknown key, wrong type or refused value",
            body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope, or a secret setting", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "settings"
)]
async fn write(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    Path(path): Path<SettingPath>,
    request: Result<Json<WriteRequest>, JsonRejection>,
) -> Result<Json<Setting>, Error> {
    scope::require!(caller, "com.hashinbox.settings.$write");
    let Json(request) = request?;
    Ok(Json(
        settings::write(&state.db, &path.key, request.value).await?,
    ))
}
