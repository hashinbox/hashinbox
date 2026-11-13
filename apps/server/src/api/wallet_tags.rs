//! `/v1/wallets/tags` and `/v1/wallets/{id}/tags/{tag}`: the labels on
//! wallets (kept on the wallet row) that trigger selectors pick by
//! (`{"tag": "…"}`).

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, IndexParams};
use crate::{api::AppState, db::Paged as _, models::wallet::WalletId, usecase::wallets};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(add, remove))
}

/// `GET /v1/wallets/tags`: a page of tags.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = TagList)]
struct ListResponse {
    /// Tags in use on any wallet, sorted, each once. A wallet's own tags are
    /// on it (`GET /v1/wallets`).
    tags: Vec<String>,
    /// Pass as `cursor` for the next page; null on the last page. Always
    /// present.
    #[schema(required)]
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/wallets/tags",
    operation_id = "list_tags",
    summary = "List the tags in use",
    params(IndexParams),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Tags in order", body = ListResponse),
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
    let page = index.page("tags", |cursor| Some(cursor.to_owned()))?;
    let limit = page.limit;
    let tags = wallets::tags_page(&state.db, page).await?;
    Ok(Json(ListResponse {
        next_cursor: tags.cursor(limit),
        tags,
    }))
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
    let id = WalletId::parse(&path.id)?;
    wallets::set_tag(&state.container, &state.db, &id, path.tag, true).await?;
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
    let id = WalletId::parse(&path.id)?;
    wallets::set_tag(&state.container, &state.db, &id, path.tag, false).await?;
    Ok(StatusCode::NO_CONTENT)
}
