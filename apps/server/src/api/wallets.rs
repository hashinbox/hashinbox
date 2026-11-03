//! `/v1/wallets` and `/v1/wallets/{id}`: generate, import or watch, list and
//! read wallets (`wallet_indexes` indexes them). A wallet is an address:
//! keyed (its key kept in its `wallet` row) or watch-only, with the assets it
//! is indexed in and how far each is indexed.

use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use base::{Chain, WalletIndexState};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, asset_info, balance::amount, wallet_address};
use crate::{
    api::AppState,
    models::{self, Network, wallet::Indexed},
};

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
    /// How far each asset is indexed, in asset id order.
    indexes: Vec<Progress>,
}

impl Wallet {
    /// `indexed`, against its network's indexer `stats`.
    fn new(indexed: Indexed, stats: Option<&base::Stats>) -> Self {
        let address = base::Address::from(&indexed.wallet);
        let indexes: Vec<Progress> = indexed
            .indexes
            .into_iter()
            .map(|index| Progress::new(index, stats))
            .collect();
        Self {
            id: format!(
                "{}:{}:{}",
                address.network.chain, address.network.name, address.value
            ),
            network: address.network.into(),
            address: address.value,
            keyed: indexed.wallet.keyed(),
            assets: indexes.iter().map(|index| index.asset.clone()).collect(),
            indexes,
        }
    }
}

/// How far one asset of a wallet is indexed. Complete when `index_state`
/// is `indexing` and `index_behind_tip` is 0; permanent up to
/// `min(indexed_at, finality_at)`; history shortened when `indexed_from` is
/// above `index_requested_from`. Positions are native (height or slot).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = WalletIndexProgress)]
pub(super) struct Progress {
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
    pub(super) fn new(index: base::WalletIndex, stats: Option<&base::Stats>) -> Self {
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
pub(super) enum IndexState {
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

/// `POST /v1/wallets` body: a wallet in one network, created without
/// indexes. With `secret`, its key is imported and kept; else with
/// `address`, the address is watched without a key; with neither, a new key
/// is generated and kept.
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = CreateWallet)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// Chain name, as `GET /v1/assets` lists it.
    chain: String,
    /// Network name, as `GET /v1/assets` lists it.
    network: String,
    /// A raw private key in the chain's import format: Ethereum hex, Solana
    /// Base58 secret or keypair, Bitcoin WIF.
    #[serde(default)]
    #[schema(nullable = false)]
    secret: Option<String>,
    /// Address text in any form the chain accepts; with `secret`, the
    /// address the key must derive.
    #[serde(default)]
    #[schema(nullable = false)]
    address: Option<String>,
}

#[utoipa::path(
    post,
    path = "/v1/wallets",
    operation_id = "create_wallet",
    summary = "Import a wallet's key, watch an address, or generate a wallet, in one configured network",
    description = "With `secret`, imports a raw private key and keeps it; with `address` too, \
        the key must derive that address. With `address` alone, watches it without its key. \
        With neither, generates a key and keeps it; retrying makes another wallet. The wallet \
        is created without indexes (`POST /v1/wallets/{id}/index`). An address already kept \
        answers its wallet as is (200), except that importing the key of a watch-only wallet \
        keeps the key; its tags and indexes stay.",
    request_body = CreateRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "Wallet created", body = Wallet),
        (status = 200, description = "The address was already kept: its wallet", body = Wallet),
        (status = 400, description = "Malformed body, address or key, or a key that derives \
            another address", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network is not configured", body = ErrorBody),
        (status = 503, description = "Generation or the store is unavailable",
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
    let factory = &state.payments.service(&network)?.wallets.factory;
    let given = request
        .address
        .map(|text| factory.parse_address(&text))
        .transpose()?;
    let row = if let Some(secret) = request.secret {
        let wallet = factory.from_pk_text(&secret).await?;
        if let Some(given) = given.filter(|given| given != wallet.address()) {
            return Err(Error::invalid_request(format!(
                "the key derives {}, not {}",
                wallet.address().value,
                given.value
            )));
        }
        models::Wallet::try_from(&*wallet)
    } else if let Some(address) = given {
        models::Wallet::from_address(&address)
    } else {
        let wallet = factory.from_pk(factory.generate().await?).await?;
        models::Wallet::try_from(&*wallet)
    }
    .map_err(|error| Error::invalid_request(format!("{error:#}")))?;
    let address = base::Address::from(&row);
    let created = state
        .db
        .write(move |connection| {
            let kept = models::wallet::db::find_by_id(connection, &row.id)?.is_some();
            models::wallet::db::insert(connection, &row)?;
            Ok(!kept)
        })
        .await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(wallet(&state, &address).await?)))
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
    let entries = crate::usecase::wallets::list(&state.db).await?;
    let stats = state.payments.stats();
    let mut wallets = Vec::with_capacity(entries.len());
    for entry in entries {
        let indexed = entry.wallet;
        // An asset the server no longer configures has no display units.
        let balances = indexed
            .indexes
            .iter()
            .zip(entry.balances)
            .filter_map(|(index, balance)| {
                let info = asset_info(&state.payments, &index.asset.network, &index.asset.id);
                Some(amount(&info.ok()?, balance?).map(|text| (index.asset.id.clone(), text)))
            })
            .collect::<Result<_, _>>()?;
        let stats = stats.get(&base::Address::from(&indexed.wallet).network);
        wallets.push(Listed {
            tags: indexed.wallet.tags.0.iter().cloned().collect(),
            wallet: Wallet::new(indexed, stats),
            balances,
        });
    }
    Ok(Json(ListResponse { wallets }))
}

/// The wallet a route reads.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
pub(super) struct WalletPath {
    /// `<chain>:<network>:<address>`, as a create call returned it.
    pub(super) id: String,
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
    Ok(Json(wallet(&state, &address).await?))
}

/// The wallet of `address`, with how far each of its assets is indexed.
///
/// # Errors
///
/// Not found when it is not kept; the store's failure.
async fn wallet(state: &AppState, address: &base::Address) -> Result<Wallet, Error> {
    let id = models::Wallet::id(address)
        .map_err(|error| Error::invalid_request(format!("{error:#}")))?;
    let indexed = state
        .db
        .read(move |connection| models::wallet::db::find_with_indexes(connection, Some(&id)))
        .await?
        .pop()
        .ok_or_else(|| Error::not_found("wallet is unknown"))?;
    let stats = state
        .payments
        .service(&address.network)
        .ok()
        .map(|service| service.indexer.stats());
    Ok(Wallet::new(indexed, stats.as_ref()))
}
