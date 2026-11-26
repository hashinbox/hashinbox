//! `GET /v1/version`: the server's version, what setup still misses, and
//! its public settings.

use std::{collections::BTreeMap, sync::Arc};

use axum::{Json, extract::State};
use serde::Serialize;
use serde_json::Value;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{AppState, Error},
    config::VERSION,
    usecase::{settings, users},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(read))
}

/// The server's version, and its public settings.
#[derive(Clone, Debug, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = Version)]
struct ReadResponse {
    /// The build's version: its git tag description, or the crate version.
    version: String,
    /// True on a fresh install (no user yet); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    missing_root: Option<bool>,
    /// Every public setting's value (its default when never written), by
    /// key: `{"instance": "hashinbox"}`.
    settings: BTreeMap<String, Value>,
}

#[utoipa::path(
    get,
    path = "/v1/version",
    operation_id = "version_get",
    summary = "Read the server's version",
    description = "Public: the version this server was built as, `missing_root` while no \
        user exists (`POST /v1/users` creates the root) and every public setting's value. \
        A 200 means the server is up and serving.",
    responses(
        (status = 200, description = "The version", body = ReadResponse),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "health"
)]
async fn read(State(state): State<Arc<AppState>>) -> Result<Json<ReadResponse>, Error> {
    let root_exists = users::root_exists(&state.db, &state.root_exists).await?;
    Ok(Json(ReadResponse {
        version: VERSION.to_owned(),
        missing_root: (!root_exists).then_some(true),
        settings: settings::public(&state.db).await?,
    }))
}
