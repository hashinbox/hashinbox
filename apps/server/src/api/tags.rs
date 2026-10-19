//! `/v1/tags` and `/v1/wallets/{id}/tags/{tag}`: the labels on addresses
//! that trigger selectors pick by (`{"tag": "…"}`).

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
use crate::state::AppState;

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(add, remove))
}

/// One tag on one address.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct Tag {
    /// `<chain>:<network>:<address>`, as the wallet routes take it.
    wallet_id: String,
    /// The tag: 1 to 64 of `[a-z0-9-]`.
    tag: String,
}

/// `GET /v1/tags`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = TagList)]
struct ListResponse {
    /// Every tag, by network, address, then tag.
    tags: Vec<Tag>,
}

/// The tag a route acts on.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct TagPath {
    /// `<chain>:<network>:<address>`.
    id: String,
    /// 1 to 64 of `[a-z0-9-]`.
    tag: String,
}

#[utoipa::path(
    get,
    path = "/v1/tags",
    operation_id = "list_tags",
    summary = "List every tag",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Every tag", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn list(State(state): State<Arc<AppState>>) -> Result<Json<ListResponse>, Error> {
    let tags = state.storage.tags().await?;
    Ok(Json(ListResponse {
        tags: tags
            .iter()
            .map(|tag| Tag {
                wallet_id: format!("{}:{}:{}", tag.chain, tag.network, tag.address),
                tag: tag.tag.clone(),
            })
            .collect(),
    }))
}

#[utoipa::path(
    put,
    path = "/v1/wallets/{id}/tags/{tag}",
    operation_id = "add_wallet_tag",
    summary = "Tag an address",
    description = "Tags the address; tagging twice keeps one tag. Any address of a configured \
        network can be tagged, keyed or not.",
    params(TagPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 204, description = "Tagged"),
        (status = 400, description = "Malformed wallet ID or tag", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network is not configured", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn add(
    State(state): State<Arc<AppState>>,
    Path(path): Path<TagPath>,
) -> Result<StatusCode, Error> {
    let address = wallet_address(&state.payments, &path.id)?;
    state
        .storage
        .add_tag(&address, &path.tag)
        .await
        .map_err(|error| Error::invalid_request(format!("{error:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/v1/wallets/{id}/tags/{tag}",
    operation_id = "remove_wallet_tag",
    summary = "Untag an address",
    params(TagPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 204, description = "Untagged"),
        (status = 400, description = "Malformed wallet ID or tag", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "No such tag, or network not configured", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "triggers"
)]
async fn remove(
    State(state): State<Arc<AppState>>,
    Path(path): Path<TagPath>,
) -> Result<StatusCode, Error> {
    let address = wallet_address(&state.payments, &path.id)?;
    if !state.storage.remove_tag(&address, &path.tag).await? {
        return Err(Error::not_found("no such tag"));
    }
    Ok(StatusCode::NO_CONTENT)
}
