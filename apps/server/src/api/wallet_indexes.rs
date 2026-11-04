//! `/v1/wallets/{id}/indexes` and `/v1/wallets/{id}/indexes/{asset}`: the
//! indexes of a kept wallet, one per asset. Create one from a start (070's
//! rules: `max_history_days`; a start the node no longer serves moves
//! forward), at once; pause or resume it; delete it.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use base::{Birthday, WalletIndexState};
use serde::Deserialize;
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, asset_info, wallet_address, wallets::WalletPath};
use crate::{
    api::AppState,
    models::{
        self, wallet_index,
        wallet_index::{IndexState, WalletIndex},
    },
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(create))
        .routes(routes!(update, remove))
}

/// `POST /v1/wallets/{id}/indexes` body.
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = CreateWalletIndex)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// The asset id to index the wallet in.
    asset: String,
    /// The native position to index from, no older than the network's
    /// `max_history_days`. Absent: from the chain's tip, as a generated
    /// wallet (it has no past) must be.
    #[serde(default)]
    #[schema(nullable = false)]
    start: Option<u64>,
}

impl CreateRequest {
    /// Where to index from: `start`, refused when older than the network's
    /// history limit (accepted when it has no limit or cannot find blocks by
    /// time); absent, the chain's tip.
    async fn birthday(&self, service: &base::Service) -> Result<Birthday, Error> {
        let Some(position) = self.start else {
            return Ok(Birthday::Fresh);
        };
        // No limit, or no way to find blocks by time: any start goes.
        let (Some(dated), Some(limit)) = (&service.dated, service.max_history) else {
            return Ok(Birthday::At(position));
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let Some(earliest) = dated
            .position_at(now.saturating_sub(limit).as_secs())
            .await?
        else {
            return Ok(Birthday::At(position));
        };
        if position < earliest.position {
            return Err(Error::invalid_request(format!(
                "start {position} is older than this network's history limit of {} \
                 days: start at {} or later",
                limit.as_secs() / 86_400,
                earliest.position
            )));
        }
        Ok(Birthday::At(position))
    }
}

#[utoipa::path(
    post,
    path = "/v1/wallets/{id}/indexes",
    operation_id = "create_wallet_index",
    summary = "Index a wallet in an asset",
    description = "Indexes the wallet in `asset` from `start` (absent: the chain's tip), at once (the network's next \
        indexing step picks it). An asset already indexed keeps its first start (200). A \
        `start` older than the network's `max_history_days` is refused; one the node no \
        longer serves moves forward to the first block it serves before indexing begins.",
    params(WalletPath),
    request_body = CreateRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "The index", body = WalletIndex),
        (status = 200, description = "The asset was already indexed: its index",
            body = WalletIndex),
        (status = 400, description = "Malformed body or wallet ID, or a start older than the \
            network's history limit", body = ErrorBody),
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
    let address = wallet_address(&state.payments, &path.id)?;
    let asset = asset_info(&state.payments, &address.network, &request.asset)?.asset;
    let (wallet_id, index_id) = models::Wallet::id(&address)
        .and_then(|wallet| Ok((wallet.clone(), models::child(&wallet, &[&asset.id])?)))
        .map_err(|error| Error::invalid_request(format!("{error:#}")))?;
    let Some(mut wallet) = state
        .db
        .read(move |connection| models::wallet::db::find(connection, &wallet_id))
        .await?
    else {
        return Err(Error::not_found("wallet is unknown"));
    };
    if let Some(index) = wallet.indexes.remove(&asset.id) {
        return Ok((StatusCode::OK, Json(index)));
    }
    let birthday = request
        .birthday(state.payments.service(&address.network)?)
        .await?;
    // `Indexer::watch` borrows the address as `&dyn Addressed`, which is not
    // `Sync`, so its future is not `Send`: it runs on a blocking thread.
    let (payments, watched, runtime) = (
        state.payments.clone(),
        address.clone(),
        tokio::runtime::Handle::current(),
    );
    tokio::task::spawn_blocking(move || {
        runtime.block_on(payments.watch(&watched, &asset, birthday))
    })
    .await
    .map_err(anyhow::Error::from)??;
    let network = address.network;
    let created = state
        .db
        .read(move |connection| wallet_index::db::find_at_tip(connection, &network, &index_id))
        .await?
        .ok_or_else(|| Error::not_found("no such index"))?;
    Ok((StatusCode::CREATED, Json(created)))
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
    /// The wallet's address and the index's model id.
    pub(super) fn index(
        &self,
        payments: &payments::Payments,
    ) -> Result<(base::Address, String), Error> {
        let address = wallet_address(payments, &self.id)?;
        let id = models::Wallet::id(&address)
            .and_then(|wallet| models::child(&wallet, &[&self.asset]))
            .map_err(|error| Error::invalid_request(format!("{error:#}")))?;
        Ok((address, id))
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
    /// The state asked for; `no_data` is the indexer's to set.
    fn index_state(&self) -> Result<WalletIndexState, Error> {
        match self.index_state {
            IndexState::Indexing => Ok(WalletIndexState::Indexing),
            IndexState::Paused => Ok(WalletIndexState::Paused),
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
        from its last indexed block, backfilling what it missed.",
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
    let (address, id) = path.index(&state.payments)?;
    state
        .db
        .write(move |connection| {
            wallet_index::db::set_state(
                connection,
                &address,
                Some(&path.asset),
                index_state,
                None,
            )?;
            wallet_index::db::find_at_tip(connection, &address.network, &id)
        })
        .await?
        .map(Json)
        .ok_or_else(|| Error::not_found("no such index"))
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
    let (_, id) = path.index(&state.payments)?;
    let removed = state
        .db
        .write(move |connection| Ok(wallet_index::db::delete(connection, &id)?))
        .await?;
    if removed == 0 {
        return Err(Error::not_found("no such index"));
    }
    Ok(StatusCode::NO_CONTENT)
}
