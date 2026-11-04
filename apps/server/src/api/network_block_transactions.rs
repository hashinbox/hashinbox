//! `/v1/networks/{chain}/{network}/blocks/{position}/transactions`: a live
//! block's kept transactions, in block order. Only transactions of indexed
//! addresses are kept.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, IndexParams, network_blocks::BlockPath};
use crate::{
    api::AppState,
    db::Paged as _,
    models::{
        block,
        transaction::{self, Transaction},
    },
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(list))
}

/// `GET /v1/networks/{chain}/{network}/blocks/{position}/transactions`: a
/// page of a block's transactions.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = BlockTransactions)]
struct ListResponse {
    /// The transactions, with their legs, in block order.
    transactions: Vec<Transaction>,
    /// Pass as `cursor` for the next page; null on the last page. Always
    /// present.
    #[schema(required)]
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/blocks/{position}/transactions",
    operation_id = "list_block_transactions",
    summary = "List a live block's kept transactions",
    params(BlockPath, IndexParams),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The transactions", body = ListResponse),
        (status = 400, description = "Malformed query, cursor, or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "The network is not configured, or no live block is kept at this position", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn list(
    State(state): State<Arc<AppState>>,
    Path(path): Path<BlockPath>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    let Query(index) = index.map_err(Error::invalid_query)?;
    let (network, position) = path.at()?;
    let page = index.page("transactions", transaction::parse_key)?;
    let limit = page.limit;
    let transactions = state
        .db
        .read(move |connection| {
            let Some(block) = block::db::find_by_position(connection, &network, position)? else {
                return Ok(None);
            };
            Ok(Some(transaction::db::find_by_block_page(
                connection, &block.id, page,
            )?))
        })
        .await?
        .ok_or_else(|| Error::not_found("no live block is kept at this position"))?;
    Ok(Json(ListResponse {
        next_cursor: transactions.cursor(limit),
        transactions,
    }))
}
