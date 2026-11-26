//! `/v1/networks/{chain}/{network}/blocks`: a network's last kept blocks.

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
    models::{Block, int},
    usecase::networks,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(read))
}

/// `GET /v1/networks/{chain}/{network}/blocks`: a page of blocks.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = BlockList)]
struct ListResponse {
    /// Live kept blocks, latest first.
    blocks: Vec<Block>,
    /// Pass as `cursor` for the next page; null on the last page. Always
    /// present.
    #[schema(required)]
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/blocks",
    operation_id = "list_blocks",
    summary = "List a network's last kept blocks",
    description = "Live kept blocks, latest first, a page at a time.",
    params(NetworkPath, IndexParams),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The last blocks", body = ListResponse),
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
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    let Query(index) = index.map_err(Error::invalid_query)?;
    let page = index.page("blocks", |cursor| cursor.parse().ok())?;
    let limit = page.limit;
    let blocks = networks::blocks_page(&state.container, &state.db, path.network(), page);
    let blocks = blocks.await?;
    Ok(Json(ListResponse {
        next_cursor: blocks.cursor(limit),
        blocks,
    }))
}

/// One block of a network.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
pub(super) struct BlockPath {
    /// Chain name, as `GET /v1/networks` lists it.
    chain: String,
    /// Network name, as `GET /v1/networks` lists it.
    network: String,
    /// Native position (height or slot).
    position: u64,
}

impl BlockPath {
    /// The network, when configured (a 404 when not), and the position.
    pub(super) fn at(&self) -> Result<(base::Network, i64), Error> {
        let network = base::Network::new(base::Chain::new(&self.chain), self.network.clone());
        let position =
            int(self.position).map_err(|error| Error::invalid_request(error.to_string()))?;
        Ok((network, position))
    }
}

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/blocks/{position}",
    operation_id = "read_block",
    summary = "Read one kept block of a network",
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
    let (network, position) = path.at()?;
    Ok(Json(networks::block(&state.db, network, position).await?))
}
