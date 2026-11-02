//! `/v1/networks/{chain}/{network}/blocks`: a network's last kept blocks with
//! the kept transactions in them, and its pending ones (the mempool as this
//! server knows it). Only transactions of indexed addresses are kept.

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
    models::{Block, Leg, block, int, leg, transaction, uint},
    state::AppState,
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
struct BlocksPath {
    /// Chain name, as `GET /v1/networks` lists it.
    chain: String,
    /// Network name, as `GET /v1/networks` lists it.
    network: String,
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

/// Where a transaction is.
#[derive(Clone, Copy, Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum ChainState {
    /// Sent, not mined yet.
    Pending,
    /// Mined and executed.
    Succeeded,
    /// Mined but failed; only its fee moved.
    Reverted,
}

/// What a leg did to its address.
#[derive(Clone, Copy, Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum ChainLegKind {
    Received,
    Spent,
    Burned,
    Minted,
    Fee,
}

/// One kept leg: an indexed address's movement in a transaction.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct ChainLeg {
    address: String,
    /// Asset id of the network.
    asset: String,
    kind: ChainLegKind,
    /// Exact atomic decimal.
    amount: String,
}

impl TryFrom<Leg> for ChainLeg {
    type Error = anyhow::Error;

    fn try_from(leg: Leg) -> anyhow::Result<Self> {
        let kind = match leg.kind {
            0 => ChainLegKind::Received,
            1 => ChainLegKind::Spent,
            2 => ChainLegKind::Burned,
            3 => ChainLegKind::Minted,
            4 => ChainLegKind::Fee,
            _ => anyhow::bail!("a stored leg kind is not a transaction leg"),
        };
        Ok(Self {
            address: leg.address,
            asset: leg.asset,
            kind,
            amount: leg.amount,
        })
    }
}

/// One kept transaction.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct ChainTransaction {
    /// Chain-native transaction id.
    txid: String,
    /// Position in its block; always present, null while pending.
    #[schema(required)]
    order: Option<i64>,
    state: ChainState,
    /// Unix ms it stops holding funds; always present, null once mined.
    #[schema(required)]
    expires_at: Option<i64>,
    /// The kept legs, in order.
    legs: Vec<ChainLeg>,
}

impl ChainTransaction {
    /// The kept transaction of `row`, with its kept legs.
    fn read(
        connection: &mut diesel::SqliteConnection,
        row: transaction::Transaction,
    ) -> anyhow::Result<Self> {
        let legs = leg::db::find_for(connection, &row.id)?
            .into_iter()
            .map(ChainLeg::try_from)
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            state: match row.state {
                0 => ChainState::Pending,
                1 => ChainState::Succeeded,
                _ => ChainState::Reverted,
            },
            txid: row.txid,
            order: row.order,
            expires_at: row.expires_at,
            legs,
        })
    }
}

/// One kept block.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct ChainBlock {
    /// Native position (height or slot).
    position: u64,
    /// Block hash, hexadecimal.
    hash: String,
    /// Unix seconds, when reported; always present.
    #[schema(required)]
    timestamp: Option<u64>,
    /// Its kept transactions, in block order.
    transactions: Vec<ChainTransaction>,
}

impl ChainBlock {
    /// The kept block of `row`, with its kept transactions.
    fn read(connection: &mut diesel::SqliteConnection, row: &Block) -> anyhow::Result<Self> {
        let transactions = transaction::db::find_in(connection, &[row.id.as_str()])?
            .into_iter()
            .map(|tx| ChainTransaction::read(connection, tx))
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            position: uint(row.position)?,
            hash: hex::encode(&row.hash),
            timestamp: row.timestamp.map(uint).transpose()?,
            transactions,
        })
    }
}

/// `GET /v1/networks/{chain}/{network}/blocks`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = ChainView)]
struct ListResponse {
    /// The last kept blocks, latest first.
    blocks: Vec<ChainBlock>,
    /// The pending transactions, oldest sent first.
    mempool: Vec<ChainTransaction>,
}

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/blocks",
    operation_id = "list_blocks",
    summary = "List a network's last kept blocks and its pending transactions",
    params(BlocksPath, BlocksQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The last blocks and the mempool", body = ListResponse),
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
    let response = state
        .storage
        .read(move |connection| {
            let blocks = block::db::find_latest(connection, &network, limit)?
                .iter()
                .map(|row| ChainBlock::read(connection, row))
                .collect::<anyhow::Result<_>>()?;
            let mempool = transaction::db::find_all_pending(connection, &network)?
                .into_iter()
                .map(|tx| ChainTransaction::read(connection, tx))
                .collect::<anyhow::Result<_>>()?;
            Ok(ListResponse { blocks, mempool })
        })
        .await?;
    Ok(Json(response))
}

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/blocks/{position}",
    operation_id = "read_block",
    summary = "Read one kept block of a network, with its kept transactions",
    params(BlockPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The block", body = ChainBlock),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "The network is not configured, or no live block is kept at this position", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn read(
    State(state): State<Arc<AppState>>,
    Path(path): Path<BlockPath>,
) -> Result<Json<ChainBlock>, Error> {
    let network = base::Network::new(base::Chain::new(path.chain), path.network);
    state.payments.service(&network)?;
    let position = int(path.position).map_err(|error| Error::invalid_request(error.to_string()))?;
    let found = state
        .storage
        .read(move |connection| {
            block::db::find_live(connection, &network, position)?
                .map(|row| ChainBlock::read(connection, &row))
                .transpose()
        })
        .await?;
    found
        .map(Json)
        .ok_or_else(|| Error::not_found("no live block is kept at this position"))
}
