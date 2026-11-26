//! `/v1/wallets/{id}/indexes` and `/v1/wallets/{id}/indexes/{asset}`: the
//! indexes of a kept wallet, one per asset. Create one from a start (070's
//! rules: `max_depth`; a start the node no longer serves moves forward; a
//! chain that reads balances starts at its final block), at once; pause or
//! resume it; reconcile it with the chain's balance; delete it.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::Deserialize;
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, create_status, wallets::WalletPath};
use crate::{
    api::AppState,
    models::{
        wallet::WalletId,
        wallet_index::{IndexState, WalletIndex},
    },
    usecase::wallets,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(create))
        .routes(routes!(update, remove))
        .routes(routes!(reconcile))
}

/// `POST /v1/wallets/{id}/indexes` body.
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = CreateWalletIndex)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// The asset id to index the wallet in.
    asset: String,
    /// The native position to index from, at or below the chain's tip and
    /// at or above the tip less the network's `max_depth` (a chain that
    /// reads balances starts at its final block instead). Absent: from the chain's tip, as a generated
    /// wallet (it has no past) must be.
    #[serde(default)]
    #[schema(nullable = false)]
    start: Option<u64>,
}

#[utoipa::path(
    post,
    path = "/v1/wallets/{id}/indexes",
    operation_id = "create_wallet_index",
    summary = "Index a wallet in an asset",
    description = "Indexes the wallet in `asset` from `start` (absent: the chain's tip), at once (the network's next \
        indexing step picks it). An asset already indexed keeps its first start (200). A \
        `start` above the chain's tip, or below it less the network's `max_depth`, is refused \
        (400). On \
        a chain that reads balances (Solana, Ethereum) the index starts at the chain's \
        final block whatever `start` says (kept as `requested_from`): its balances are \
        null until the chain's balance read there opens it with an `adjustment` leg. A \
        start the node no longer serves moves forward to the first block it serves before \
        indexing begins.",
    params(WalletPath),
    request_body = CreateRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "The index", body = WalletIndex),
        (status = 200, description = "The asset was already indexed: its index",
            body = WalletIndex),
        (status = 400, description = "Malformed body or wallet ID, or a start above the \
            chain's tip or below it less the network's `max_depth`", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown, or the asset is not configured",
            body = ErrorBody),
        (status = 503, description = "The node or the store is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn create(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
    request: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<WalletIndex>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let id = WalletId::parse(&path.id)?;
    let (container, db) = (&state.container, &state.db);
    let index = wallets::create_index(container, db, &id, &request.asset, request.start);
    let (created, index) = index.await?;
    Ok((create_status(created), Json(index)))
}

/// The index a route acts on.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
pub(super) struct IndexPath {
    /// `{chain}.{network}.{address}`.
    pub(super) id: String,
    /// The asset id.
    pub(super) asset: String,
}

impl IndexPath {
    /// The wallet's id.
    pub(super) fn wallet(&self) -> Result<WalletId, Error> {
        Ok(WalletId::parse(&self.id)?)
    }
}

/// `PATCH /v1/wallets/{id}/indexes/{asset}` body.
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = UpdateWalletIndex)]
#[serde(deny_unknown_fields)]
struct UpdateRequest {
    /// `paused`: skipped, no facts kept for blocks while paused; `indexing`:
    /// resumed from `paused` or `no_data` (clearing its reason), backfilling
    /// what it missed. `no_data` is the indexer's to set.
    index_state: IndexState,
}

impl UpdateRequest {
    /// The state asked for; `no_data` is the indexer's.
    fn index_state(&self) -> Result<base::IndexState, Error> {
        match self.index_state {
            IndexState::Indexing => Ok(base::IndexState::Indexing),
            IndexState::Paused => Ok(base::IndexState::Paused),
            IndexState::NoData => Err(Error::invalid_request("no_data is the indexer's")),
        }
    }
}

#[utoipa::path(
    patch,
    path = "/v1/wallets/{id}/indexes/{asset}",
    operation_id = "update_wallet_index",
    summary = "Pause or resume a wallet's index",
    description = "`paused`: skipped, no facts kept for blocks while paused; what is kept \
        stays. `indexing`: from `paused` or `no_data` (clearing its reason), it continues \
        from its last indexed block, backfilling what it missed. On a chain that reads \
        balances (Solana, Ethereum) one left behind the chain's final block is not read \
        back: its balance is read there and one `adjustment` leg closes the gap.",
    params(IndexPath),
    request_body = UpdateRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The index", body = WalletIndex),
        (status = 400, description = "Malformed body or wallet ID, or `no_data` asked for",
            body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "No such index", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn update(
    State(state): State<Arc<AppState>>,
    Path(path): Path<IndexPath>,
    request: Result<Json<UpdateRequest>, JsonRejection>,
) -> Result<Json<WalletIndex>, Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let index_state = request.index_state()?;
    let id = path.wallet()?;
    let index = wallets::set_index_state(&state.container, &state.db, &id, path.asset, index_state);
    Ok(Json(index.await?))
}

#[utoipa::path(
    delete,
    path = "/v1/wallets/{id}/indexes/{asset}",
    operation_id = "delete_wallet_index",
    summary = "Stop indexing a wallet in an asset",
    description = "Forgets the index and its progress: its balance and history are no longer \
        read. Creating it again starts from its new start.",
    params(IndexPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "Malformed wallet ID", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "No such index", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn remove(
    State(state): State<Arc<AppState>>,
    Path(path): Path<IndexPath>,
) -> Result<StatusCode, Error> {
    let id = path.wallet()?;
    wallets::delete_index(&state.container, &state.db, &id, &path.asset).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/v1/wallets/{id}/indexes/{asset}/reconcile",
    operation_id = "reconcile_wallet_index",
    summary = "Reconcile a wallet's index with the chain's balance",
    description = "Deletes the index's mined legs above the chain's final block and \
        unsets its progress: its balances read null until the next indexing step reads \
        the chain's balance at a final block, appends one `adjustment` leg of the \
        difference and reads on from there. Pending legs stay. Corrects what indexing \
        cannot see, such as ether a contract sent. Only on chains that read balances \
        (Solana, Ethereum).",
    params(IndexPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 202, description = "The index, its progress unset", body = WalletIndex),
        (status = 400, description = "Malformed wallet ID", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "No such index", body = ErrorBody),
        (status = 422, description = "The chain reads no balance (Bitcoin), or the index is \
            paused or `no_data`", body = ErrorBody),
        (status = 503, description = "The indexer has not read the chain's final block \
            yet, or the store is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn reconcile(
    State(state): State<Arc<AppState>>,
    Path(path): Path<IndexPath>,
) -> Result<(StatusCode, Json<WalletIndex>), Error> {
    let id = path.wallet()?;
    let index = wallets::reconcile(&state.container, &state.db, &id, &path.asset).await?;
    Ok((StatusCode::ACCEPTED, Json(index)))
}
