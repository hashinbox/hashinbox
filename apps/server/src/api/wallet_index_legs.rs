//! `/v1/wallets/{id}/indexes/{asset}/legs`: an index's mined legs, latest
//! first, a page at a time (its pending legs are on the index itself).

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, IndexParams, wallet_indexes::IndexPath};
use crate::{
    api::AppState,
    db::Paged as _,
    models::leg::{self, Movement},
    usecase::wallets,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(legs))
}

/// `GET /v1/wallets/{id}/indexes/{asset}/legs`: a page of mined legs.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = LegPage)]
struct LegsResponse {
    /// Live mined legs, latest first, each with the balance after it
    /// (`total`) and its transaction's txid and state (null for a reward).
    legs: Vec<Movement>,
    /// Pass as `cursor` for the next page; null on the last page. Always
    /// present.
    #[schema(required)]
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}/indexes/{asset}/legs",
    operation_id = "list_wallet_index_legs",
    summary = "List an index's mined legs",
    description = "The index's legs in live blocks, latest first, each with the balance right \
        after it. Its pending legs are on the index itself.",
    params(IndexPath, IndexParams),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "One page of legs", body = LegsResponse),
        (status = 400, description = "Malformed wallet ID, query, cursor, or limit",
            body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn legs(
    State(state): State<Arc<AppState>>,
    Path(path): Path<IndexPath>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<LegsResponse>, Error> {
    let Query(index) = index.map_err(Error::invalid_query)?;
    let page = index.page("legs", leg::parse_key)?;
    let limit = page.limit;
    let id = path.wallet()?;
    let legs = wallets::legs_page(&state.container, &state.db, &id, &path.asset, page).await?;
    Ok(Json(LegsResponse {
        next_cursor: legs.cursor(limit),
        legs,
    }))
}
