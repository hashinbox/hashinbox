//! `/v1/networks/{chain}/{network}/blocks`: a network's last kept blocks with
//! the kept transactions in them. Only transactions of indexed addresses are
//! kept.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{
    api::AppState,
    models::{Block, block, int},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(read))
}

/// How many blocks `GET .../blocks` answers unless `limit` says otherwise.
const LIMIT: i64 = 20;

/// The network read.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
pub(super) struct BlocksPath {
    /// Chain name, as `GET /v1/networks` lists it.
    pub(super) chain: String,
    /// Network name, as `GET /v1/networks` lists it.
    pub(super) network: String,
}

/// The block read.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct BlockPath {
    /// Chain name, as `GET /v1/networks` lists it.
    chain: String,
    /// Network name, as `GET /v1/networks` lists it.
    network: String,
    /// Native position (height or slot).
    position: u64,
}

/// How many blocks.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct BlocksQuery {
    /// At most this many, 1 to 100; 20 by default.
    limit: Option<i64>,
}

/// `GET /v1/networks/{chain}/{network}/blocks`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = BlockList)]
struct ListResponse {
    /// The last kept blocks, latest first.
    blocks: Vec<Block>,
}

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/blocks",
    operation_id = "list_blocks",
    summary = "List a network's last kept blocks",
    params(BlocksPath, BlocksQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The last blocks", body = ListResponse),
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
    query: Result<Query<BlocksQuery>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let limit = query.limit.unwrap_or(LIMIT);
    if !(1..=100).contains(&limit) {
        return Err(Error::invalid_request(
            "blocks limit must be between 1 and 100",
        ));
    }
    let network = base::Network::new(base::Chain::new(path.chain), path.network);
    state.payments.service(&network)?;
    let blocks = state
        .db
        .read(move |connection| Ok(block::db::find_latest(connection, &network, limit)?))
        .await?;
    Ok(Json(ListResponse { blocks }))
}

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/blocks/{position}",
    operation_id = "read_block",
    summary = "Read one kept block of a network, with its kept transactions",
    params(BlockPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The block", body = Block),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "The network is not configured, or no live block is kept at this position", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn read(
    State(state): State<Arc<AppState>>,
    Path(path): Path<BlockPath>,
) -> Result<Json<Block>, Error> {
    let network = base::Network::new(base::Chain::new(path.chain), path.network);
    state.payments.service(&network)?;
    let position = int(path.position).map_err(|error| Error::invalid_request(error.to_string()))?;
    let found = state
        .db
        .read(move |connection| Ok(block::db::find_at(connection, &network, position)?))
        .await?;
    found
        .map(Json)
        .ok_or_else(|| Error::not_found("no live block is kept at this position"))
}
