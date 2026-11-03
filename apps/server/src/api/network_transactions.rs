//! `/v1/networks/{chain}/{network}/transactions`: a network's kept
//! transactions, pending (the mempool as this server knows it) or mined.
//! Only transactions of indexed addresses are kept.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, network_blocks::BlocksPath};
use crate::{
    api::AppState,
    models::{transaction, transaction::Transaction},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(list))
}

/// Which transactions.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct ListQuery {
    /// Pending ones (true, the default), oldest sent first; or mined ones in
    /// live blocks (false), latest first.
    pending: Option<bool>,
    /// At most this many, 1 to 500; 100 by default.
    limit: Option<i64>,
}

/// `GET /v1/networks/{chain}/{network}/transactions`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = NetworkTransactions)]
struct ListResponse {
    /// The transactions, with their legs.
    transactions: Vec<Transaction>,
}

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/transactions",
    operation_id = "list_network_transactions",
    summary = "List a network's pending or mined transactions",
    params(BlocksPath, ListQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The transactions", body = ListResponse),
        (status = 400, description = "Malformed query or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "The network is not configured", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn list(
    State(state): State<Arc<AppState>>,
    Path(path): Path<BlocksPath>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=500).contains(&limit) {
        return Err(Error::invalid_request(
            "transactions limit must be between 1 and 500",
        ));
    }
    let pending = query.pending.unwrap_or(true);
    let network = base::Network::new(base::Chain::new(path.chain), path.network);
    state.payments.service(&network)?;
    let transactions = state
        .db
        .read(move |connection| {
            Ok(transaction::db::find_by_network(
                connection, &network, pending, limit,
            )?)
        })
        .await?;
    Ok(Json(ListResponse { transactions }))
}
