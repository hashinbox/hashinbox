//! `/v1/networks/{chain}/{network}/transactions`: a network's kept
//! transactions, pending (the mempool as this server knows it) and mined.
//! Only transactions of indexed addresses are kept.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, IndexParams, networks::NetworkPath};
use crate::{
    api::AppState,
    db::Paged as _,
    models::transaction::{self, Transaction},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(list))
}

/// Query of `GET /v1/networks/{chain}/{network}/transactions`.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct ListQuery {
    /// Only pending ones (true), oldest sent first; otherwise (the default)
    /// pending and mined in live blocks, newest first.
    pending: Option<bool>,
}

/// `GET /v1/networks/{chain}/{network}/transactions`: a page of transactions.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = NetworkTransactions)]
struct ListResponse {
    /// The transactions, with their legs.
    transactions: Vec<Transaction>,
    /// Pass as `cursor` for the next page; null on the last page. Always
    /// present.
    #[schema(required)]
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/transactions",
    operation_id = "list_network_transactions",
    summary = "List a network's transactions, or only its pending ones",
    params(NetworkPath, ListQuery, IndexParams),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The transactions", body = ListResponse),
        (status = 400, description = "Malformed query, cursor, or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "The network is not configured", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn list(
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
    query: Result<Query<ListQuery>, QueryRejection>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let Query(index) = index.map_err(Error::invalid_query)?;
    let pending = query.pending == Some(true);
    let page = index.page("transactions", transaction::parse_key)?;
    let limit = page.limit;
    let network = path.network(&state.container)?;
    let transactions = state
        .db
        .read(move |connection| {
            if pending {
                return Ok(transaction::db::find_pending_page(
                    connection, &network, page,
                )?);
            }
            Ok(transaction::db::find_page(connection, &network, page)?)
        })
        .await?;
    Ok(Json(ListResponse {
        next_cursor: transactions.cursor(limit),
        transactions,
    }))
}
