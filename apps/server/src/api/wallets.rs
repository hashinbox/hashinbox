//! `/v1/wallets` and `/v1/wallets/{id}`: generate or watch, list and read
//! wallets. A wallet is an address: keyed (generated here, its key kept in
//! the `wallet` table) or watch-only, with the assets it is indexed in and
//! how far each is indexed; pause and resume its indexing.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use base::{Birthday, Chain, WalletIndexState};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, asset_info, balance::amount, wallet_address};
use crate::{
    models::{Network, balance, tag, wallet_index},
    state::AppState,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(create, list))
        .routes(routes!(read))
        .routes(routes!(pause))
        .routes(routes!(resume))
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
    /// How far each asset is indexed, in asset id order.
    indexes: Vec<Progress>,
}

/// How far one asset of a wallet is indexed. Complete when `index_state`
/// is `indexing` and `index_behind_tip` is 0; permanent up to
/// `min(indexed_at, finality_at)`; history shortened when `indexed_from` is
/// above `index_requested_from`. Positions are native (height or slot).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = WalletIndexProgress)]
struct Progress {
    /// The asset id.
    asset: String,
    index_state: IndexState,
    /// Why it is `no_data`; null otherwise. Always present.
    #[schema(required)]
    index_reason: Option<String>,
    /// The start asked for.
    index_requested_from: u64,
    /// The start indexed from: the one asked for, or the first block the
    /// node served when it no longer served that one.
    indexed_from: u64,
    /// The last block indexed; null before the first. Always present.
    #[schema(required)]
    indexed_at: Option<u64>,
    /// The network's final (confirmed) block at its indexer's last step;
    /// null before its first. Always present.
    #[schema(required)]
    finality_at: Option<u64>,
    /// Blocks between the last indexed and the network's produced tip at
    /// its indexer's last step; null before its first. Always present.
    #[schema(required)]
    index_behind_tip: Option<u64>,
}

impl Progress {
    /// `index`, against its network's indexer `stats`.
    fn new(index: base::WalletIndex, stats: Option<&base::Stats>) -> Self {
        let tip = stats.and_then(|stats| stats.tip);
        let next = index.next().unwrap_or(u64::MAX);
        Self {
            index_state: index.state.into(),
            indexed_at: index.indexed_at.as_ref().map(|header| header.position),
            finality_at: stats.and_then(|stats| stats.confirmed),
            index_behind_tip: tip.map(|tip| tip.saturating_add(1).saturating_sub(next)),
            asset: index.asset.id,
            index_reason: index.reason,
            index_requested_from: index.requested_from,
            indexed_from: index.birthday,
        }
    }
}

/// Whether an asset of a wallet is indexed.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum IndexState {
    /// Indexed.
    Indexing,
    /// Skipped until resumed; resumed, it backfills what it missed.
    Paused,
    /// Skipped: the node no longer serves a block it needs (see
    /// `index_reason`); resume once a node serves it.
    NoData,
}

impl From<WalletIndexState> for IndexState {
    fn from(state: WalletIndexState) -> Self {
        match state {
            WalletIndexState::Indexing => Self::Indexing,
            WalletIndexState::Paused => Self::Paused,
            WalletIndexState::NoData => Self::NoData,
        }
    }
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
    /// position to index from, no older than the network's
    /// `max_history_days`. Absent means `"fresh"`.
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
        keeps its first start. A generated wallet with empty `assets` is kept unwatched. A \
        `start` older than the network's `max_history_days` is refused.",
    request_body = CreateRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "Wallet generated or address watched", body = Wallet),
        (status = 400, description = "Malformed body or address text, or a start older than \
            the network's history limit", body = ErrorBody),
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
    if let Some(Start::At(position)) = request.start {
        within_history(&state, &network, position).await?;
    }
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
    let mut wallet = wallet(&state, address).await?;
    wallet.keyed |= keyed;
    Ok((StatusCode::CREATED, Json(wallet)))
}

/// Refuses `position` as a start in `network` when it is older than the
/// network's history limit; accepts it when the network has no limit or
/// cannot find blocks by time.
///
/// # Errors
///
/// Invalid request for a start too old; the node's failure.
async fn within_history(
    state: &AppState,
    network: &base::Network,
    position: u64,
) -> Result<(), Error> {
    let service = state.payments.service(network)?;
    let (Some(dated), Some(limit)) = (&service.dated, service.max_history) else {
        return Ok(());
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let Some(earliest) = dated
        .position_at(now.saturating_sub(limit).as_secs())
        .await?
    else {
        return Ok(());
    };
    if position < earliest.position {
        return Err(Error::invalid_request(format!(
            "start {position} is older than this network's history limit of {} days: start at \
             {} or later",
            limit.as_secs() / 86_400,
            earliest.position
        )));
    }
    Ok(())
}

/// One listed wallet: its public metadata, tags and balances.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = ListedWallet)]
struct Listed {
    #[serde(flatten)]
    wallet: Wallet,
    /// Its tags, by name.
    tags: Vec<String>,
    /// By asset id, its balance in each indexed asset this server configures:
    /// exact display units, as `GET /v1/wallets/{id}/balance` reports `amount`.
    balances: BTreeMap<String, String>,
}

/// `GET /v1/wallets`: the wallets.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = WalletList)]
struct ListResponse {
    /// Wallets in ID order.
    wallets: Vec<Listed>,
}

#[utoipa::path(
    get,
    path = "/v1/wallets",
    operation_id = "list_wallets",
    summary = "List the wallets",
    description = "Every keyed wallet and watched address, in wallet ID order, with its tags \
        and its balance in each indexed asset.",
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
    let networks: Vec<_> = state.payments.networks().cloned().collect();
    // Every index, every tag, and every index's balance in one query each;
    // every index's progress in one query per network.
    let (indexes, tags, balances, progress) = state
        .storage
        .read(move |connection| {
            let indexes = wallet_index::db::find_every(connection)?;
            let ids: Vec<&str> = indexes.iter().map(|index| index.id.as_str()).collect();
            // Confirmed through every position: every live block, as the
            // balance route sums them.
            let balances = balance::db::find_signed_by_ids(connection, &ids, i64::MAX)?;
            let mut progress = Vec::new();
            for network in &networks {
                progress.extend(wallet_index::db::find_all(connection, network)?);
            }
            Ok((indexes, tag::db::find_all(connection)?, balances, progress))
        })
        .await?;
    let stats = state.payments.stats();
    let listed = |address: &base::Address| Listed {
        wallet: Wallet {
            id: id(address),
            network: address.network.clone().into(),
            address: address.value.clone(),
            keyed: keyed.contains(address),
            assets: Vec::new(),
            indexes: Vec::new(),
        },
        tags: Vec::new(),
        balances: BTreeMap::new(),
    };
    let mut wallets: BTreeMap<base::Address, Listed> = keyed
        .iter()
        .map(|address| (address.clone(), listed(address)))
        .collect();
    for index in indexes {
        let wallet = wallets
            .entry(index.address.clone())
            .or_insert_with(|| listed(&index.address));
        // An asset the server no longer configures has no display units.
        let info = asset_info(&state.payments, &index.asset.network, &index.asset.id);
        if let (Ok(info), Some(balance)) = (info, balances.get(&index.id)) {
            let total = balance.confirmed.clone();
            wallet
                .balances
                .insert(index.asset.id.clone(), amount(&info, total)?);
        }
        wallet.wallet.assets.push(index.asset.id);
    }
    for index in progress {
        if let Some(wallet) = wallets.get_mut(&index.address) {
            let stats = stats.get(&index.address.network);
            wallet.wallet.indexes.push(Progress::new(index, stats));
        }
    }
    for tag in tags.iter() {
        let network = base::Network::new(Chain::new(&tag.chain), &tag.network);
        let address = base::Address::new(network, &tag.address);
        if let Some(wallet) = wallets.get_mut(&address) {
            wallet.tags.push(tag.tag.clone());
        }
    }
    Ok(Json(ListResponse {
        wallets: wallets.into_values().collect(),
    }))
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
    description = "The wallet's network, canonical address, whether it is keyed, the assets \
        it is indexed in and how far each is; never its key.",
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
    Ok(Json(wallet(&state, address).await?))
}

#[utoipa::path(
    post,
    path = "/v1/wallets/{id}/pause",
    operation_id = "pause_wallet",
    summary = "Pause indexing every asset of a wallet",
    description = "Its indexes are skipped: no facts are kept for blocks while paused. Resume \
        backfills what they missed.",
    params(WalletPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The wallet, paused", body = Wallet),
        (status = 400, description = "Malformed wallet ID", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody),
        (status = 503, description = "The wallet store is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn pause(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Wallet>, Error> {
    set_state(&state, &path.id, WalletIndexState::Paused).await
}

#[utoipa::path(
    post,
    path = "/v1/wallets/{id}/resume",
    operation_id = "resume_wallet",
    summary = "Resume indexing every asset of a wallet",
    description = "From `paused` or `no_data` (clearing its reason): each index continues from \
        its last indexed block, backfilling what it missed.",
    params(WalletPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The wallet, indexing", body = Wallet),
        (status = 400, description = "Malformed wallet ID", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody),
        (status = 503, description = "The wallet store is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn resume(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Wallet>, Error> {
    set_state(&state, &path.id, WalletIndexState::Indexing).await
}

/// Sets every index of wallet `id` to `index_state`, clearing its reason.
async fn set_state(
    state: &AppState,
    id: &str,
    index_state: WalletIndexState,
) -> Result<Json<Wallet>, Error> {
    let address = wallet_address(&state.payments, id)?;
    let target = address.clone();
    state
        .storage
        .write(move |connection| {
            wallet_index::db::set_state(connection, &target, None, index_state, None)?;
            Ok(())
        })
        .await?;
    Ok(Json(wallet(state, address).await?))
}

/// The wallet of `address`, with how far each of its assets is indexed.
///
/// # Errors
///
/// Not found when it is neither keyed nor watched; the store's failure.
async fn wallet(state: &AppState, address: base::Address) -> Result<Wallet, Error> {
    let keyed = state.storage.keyed().await?.contains(&address);
    let network = address.network.clone();
    let indexes = state
        .storage
        .read(move |connection| wallet_index::db::find_all(connection, &network))
        .await?;
    let stats = state
        .payments
        .service(&address.network)
        .ok()
        .map(|service| service.indexer.stats());
    let indexes: Vec<_> = indexes
        .into_iter()
        .filter(|index| index.address == address)
        .map(|index| Progress::new(index, stats.as_ref()))
        .collect();
    if !keyed && indexes.is_empty() {
        return Err(Error::not_found("wallet is unknown"));
    }
    Ok(Wallet {
        id: id(&address),
        network: address.network.clone().into(),
        address: address.value,
        keyed,
        assets: indexes.iter().map(|index| index.asset.clone()).collect(),
        indexes,
    })
}

/// The wallet ID of `address`.
fn id(address: &base::Address) -> String {
    format!(
        "{}:{}:{}",
        address.network.chain, address.network.name, address.value
    )
}
