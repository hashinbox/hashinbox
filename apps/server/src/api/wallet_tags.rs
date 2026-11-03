//! `/v1/wallets/tags` and `/v1/wallets/{id}/tags/{tag}`: the labels on
//! wallets (kept on the wallet row) that trigger selectors pick by
//! (`{"tag": "…"}`).

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, wallet_address};
use crate::{
    api::AppState,
    models::{Wallet, wallet},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(add, remove))
}

/// `GET /v1/wallets/tags`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = TagList)]
struct ListResponse {
    /// Every tag in use on any wallet, sorted, each once. A wallet's own
    /// tags are on it (`GET /v1/wallets`).
    tags: Vec<String>,
}

/// The tag a route acts on.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct TagPath {
    /// `{chain}.{network}.{address}`.
    id: String,
    /// 1 to 64 of `[a-z0-9-]`.
    tag: String,
}

#[utoipa::path(
    get,
    path = "/v1/wallets/tags",
    operation_id = "list_tags",
    summary = "List the tags in use",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Every tag", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn list(State(state): State<Arc<AppState>>) -> Result<Json<ListResponse>, Error> {
    let tags = state
        .db
        .read(|connection| Ok(wallet::db::find_tags(connection)?))
        .await?;
    Ok(Json(ListResponse { tags }))
}

#[utoipa::path(
    put,
    path = "/v1/wallets/{id}/tags/{tag}",
    operation_id = "add_wallet_tag",
    summary = "Tag a wallet",
    description = "Tags the wallet, keyed or watch-only; tagging twice keeps one tag.",
    params(TagPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 204, description = "Tagged"),
        (status = 400, description = "Malformed wallet ID or tag", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown, or network not configured",
            body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn add(
    State(state): State<Arc<AppState>>,
    Path(path): Path<TagPath>,
) -> Result<StatusCode, Error> {
    set(&state, path, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/v1/wallets/{id}/tags/{tag}",
    operation_id = "remove_wallet_tag",
    summary = "Untag a wallet",
    description = "Removes the tag from the wallet.",
    params(TagPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 204, description = "Untagged"),
        (status = 400, description = "Malformed wallet ID or tag", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "No such wallet or tag, or network not configured",
            body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn remove(
    State(state): State<Arc<AppState>>,
    Path(path): Path<TagPath>,
) -> Result<StatusCode, Error> {
    if !set(&state, path, false).await? {
        return Err(Error::not_found("no such tag"));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Adds (`add`) or removes the tag of `path` on its wallet; whether its tags
/// changed.
///
/// # Errors
///
/// A 400 for a malformed ID or tag, a 404 for an unknown wallet.
async fn set(state: &AppState, path: TagPath, add: bool) -> Result<bool, Error> {
    let address = wallet_address(&state.payments, &path.id)?;
    let id = Wallet::id(&address).map_err(|error| Error::invalid_request(format!("{error:#}")))?;
    if !crate::models::wallet::is_tag(&path.tag) {
        return Err(Error::invalid_request(format!(
            "a tag is 1 to 64 of [a-z0-9-]: {:?}",
            path.tag
        )));
    }
    state
        .db
        .write(move |connection| wallet::db::set_tag(connection, &id, &path.tag, add))
        .await?
        .ok_or_else(|| Error::not_found("wallet is unknown"))
}
