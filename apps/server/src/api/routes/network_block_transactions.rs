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

use crate::models::Session;

use super::network_blocks::BlockPath;
use crate::{
    api::AppState,
    db::Paged as _,
    models::transaction::{self, Transaction},
    usecase::networks,
};
use crate::{api::IndexParams, error::Error};

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

#[scope::utoipa(
    get,
    path = "/v1/networks/{chain}/{network}/blocks/{position}/transactions",
    operation_id = "network_block_transactions_list",
    summary = "List a live block's kept transactions",
    params(BlockPath, IndexParams),
    responses(
        (status = 200, description = "The transactions", body = ListResponse),
        (status = 400, description = "Malformed query, cursor, or limit", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 404, description = "The network is not configured, or no live block is kept at this position", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "networks"
)]
async fn list(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(path): Path<BlockPath>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    scope::require!(session, "com.hashinbox.transactions.$read");
    let Query(index) = index?;
    let (network, position) = path.at()?;
    let page = index.page("transactions", transaction::parse_key)?;
    let limit = page.limit;
    let transactions = networks::block_transactions_page(&state.db, network, position, page);
    let transactions = transactions.await?;
    Ok(Json(ListResponse {
        next_cursor: transactions.cursor(limit),
        transactions,
    }))
}
