//! `/v1/wallets` and `/v1/wallets/{id}`: generate or watch, list and read
//! wallets. A wallet is an address: keyed (generated here, its key kept in
//! the `wallet` table) or watch-only, with the assets it is indexed in.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use base::{Birthday, Chain};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, asset_info, wallet_address};
use crate::{models::Network, state::AppState};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(create, list))
        .routes(routes!(read))
}

/// One wallet's public metadata; never its key.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct Wallet {
    /// `<chain>:<network>:<address>`: what every wallet route takes.
    id: String,
    /// The wallet's chain and network.
    network: Network,
    /// Canonical address text.
    address: String,
    /// Whether this server holds its key and can send from it.
    keyed: bool,
    /// The asset ids it is indexed in.
    assets: Vec<String>,
}

/// `POST /v1/wallets` body: generate a wallet in one network, or watch an
/// address in it without its key.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = CreateWallet)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// Chain name, as `GET /v1/assets` lists it.
    chain: String,
    /// Network name, as `GET /v1/assets` lists it.
    network: String,
    /// Address text in any form the chain accepts. Present: watch this
    /// address without its key. Absent: generate a new wallet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    address: Option<String>,
    /// The asset ids to index it in; absent means `["native"]`. Empty
    /// generates a wallet without watching it yet (watch it later by posting
    /// its address); an address needs at least one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    assets: Option<Vec<String>>,
    /// Where indexing begins: `"fresh"` for the chain's tip, or the native
    /// position to index from. Absent means `"fresh"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    start: Option<Start>,
}

/// A wallet's birthday on the wire: `"fresh"` or a native position.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(untagged)]
enum Start {
    /// Index from the chain's tip.
    Fresh(Fresh),
    /// Index from this native position.
    At(u64),
}

/// The `"fresh"` literal of a `Start`.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
enum Fresh {
    /// The address has never been used.
    Fresh,
}

#[utoipa::path(
    post,
    path = "/v1/wallets",
    operation_id = "create_wallet",
    summary = "Generate a wallet, or watch an address, in one configured network",
    description = "Without `address`, generates a wallet and stores it with its key; keys never \
        leave the server and HTTP accepts none. Retrying creates another wallet. With \
        `address`, watches that address without its key. Either way the address is then \
        indexed in every asset of `assets` from `start`; watching an asset already watched \
        keeps its first start. A generated wallet with empty `assets` is kept unwatched.",
    request_body = CreateRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "Wallet generated or address watched", body = Wallet),
        (status = 400, description = "Malformed body or address text", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network or asset is not configured", body = ErrorBody),
        (status = 503, description = "Generation, the node or the store is unavailable",
            body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn create(
    State(state): State<Arc<AppState>>,
    request: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Wallet>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let network = base::Network::new(Chain::new(request.chain), request.network);
    let ids = request.assets.unwrap_or_else(|| vec!["native".to_owned()]);
    // A generated wallet may wait unwatched; a watched address needs an asset.
    if ids.is_empty() && request.address.is_some() {
        return Err(Error::invalid_request("watching an address needs an asset"));
    }
    let assets = ids
        .iter()
        .map(|id| asset_info(&state.payments, &network, id).map(|info| info.asset))
        .collect::<Result<Vec<_>, _>>()?;
    // The key is kept before anything is watched: an interrupted creation
    // leaves an unwatched wallet, never a watched address without its key.
    let (address, keyed) = match &request.address {
        Some(text) => (state.payments.parse(&network, text)?, false),
        None => {
            let (wallet, _) = state.payments.generate(&network).await?;
            (wallet.address().clone(), true)
        }
    };
    let birthday = match request.start {
        None | Some(Start::Fresh(Fresh::Fresh)) => Birthday::Fresh,
        Some(Start::At(position)) => Birthday::At(position),
    };
    // `Indexer::watch` borrows the address as `&dyn Addressed`, which is not
    // `Sync`, so its future is not `Send`: it runs on a blocking thread.
    let (payments, watched, runtime) = (
        state.payments.clone(),
        address.clone(),
        tokio::runtime::Handle::current(),
    );
    tokio::task::spawn_blocking(move || {
        runtime.block_on(async {
            for asset in &assets {
                payments.watch(&watched, asset, birthday).await?;
            }
            anyhow::Ok(())
        })
    })
    .await
    .map_err(anyhow::Error::from)??;
    let keyed = keyed || state.storage.keyed().await?.contains(&address);
    let watched = state.storage.watched().await?;
    let assets = watched
        .into_iter()
        .filter(|(watched, _)| *watched == address)
        .map(|(_, asset)| asset.id)
        .collect();
    Ok((
        StatusCode::CREATED,
        Json(Wallet {
            id: id(&address),
            network: address.network.clone().into(),
            address: address.value,
            keyed,
            assets,
        }),
    ))
}

/// `GET /v1/wallets`: the wallets.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = WalletList)]
struct ListResponse {
    /// Wallets in ID order.
    wallets: Vec<Wallet>,
}

#[utoipa::path(
    get,
    path = "/v1/wallets",
    operation_id = "list_wallets",
    summary = "List the wallets",
    description = "Every keyed wallet and watched address, in wallet ID order.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Wallets in ID order", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The wallet store is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn list(State(state): State<Arc<AppState>>) -> Result<Json<ListResponse>, Error> {
    let keyed: BTreeSet<_> = state.storage.keyed().await?.into_iter().collect();
    let mut wallets: BTreeMap<base::Address, Vec<String>> = keyed
        .iter()
        .map(|address| (address.clone(), Vec::new()))
        .collect();
    for (address, asset) in state.storage.watched().await? {
        wallets.entry(address).or_default().push(asset.id);
    }
    let wallets = wallets
        .into_iter()
        .map(|(address, assets)| Wallet {
            id: id(&address),
            network: address.network.clone().into(),
            keyed: keyed.contains(&address),
            address: address.value,
            assets,
        })
        .collect();
    Ok(Json(ListResponse { wallets }))
}

/// The wallet a route reads.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
struct WalletPath {
    /// `<chain>:<network>:<address>`, as a create call returned it.
    id: String,
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}",
    operation_id = "read_wallet",
    summary = "Read one wallet's public metadata",
    description = "The wallet's network, canonical address, whether it is keyed, and the assets \
        it is indexed in; never its key.",
    params(WalletPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The wallet's public metadata", body = Wallet),
        (status = 400, description = "Malformed wallet ID", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody),
        (status = 503, description = "The wallet store is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn read(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Wallet>, Error> {
    let address = wallet_address(&state.payments, &path.id)?;
    let keyed = state.storage.keyed().await?.contains(&address);
    let assets: Vec<_> = state
        .storage
        .watched()
        .await?
        .into_iter()
        .filter(|(watched, _)| *watched == address)
        .map(|(_, asset)| asset.id)
        .collect();
    if !keyed && assets.is_empty() {
        return Err(Error::not_found("wallet is unknown"));
    }
    Ok(Json(Wallet {
        id: id(&address),
        network: address.network.clone().into(),
        address: address.value,
        keyed,
        assets,
    }))
}

/// The wallet ID of `address`.
fn id(address: &base::Address) -> String {
    format!(
        "{}:{}:{}",
        address.network.chain, address.network.name, address.value
    )
}
