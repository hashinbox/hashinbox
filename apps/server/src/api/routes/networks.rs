//! `GET /v1/networks`: every stored network (its row, assets, nodes and
//! index), its tip as last told, and the chains this server runs;
//! `POST /v1/networks`: a new network, registered at once.

use std::sync::Arc;

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::models::{Caller, NetworkAsset, Session, network_asset::NetworkAssetKind};

use crate::error::Error;
use crate::{api::AppState, models::Network, usecase::networks};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(list, create))
}

/// The network's tip as its nodes last told it.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct LiveTip {
    /// The highest tip its nodes last told, kept in memory (asked at most
    /// once per block time, by selection and indexing); the stored
    /// `index.tip` before any node told one. Always present, null: neither.
    #[schema(required)]
    live: Option<u64>,
}

/// One stored network, and its tip as its nodes last told it.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = ConfiguredNetwork)]
struct NetworkResponse {
    #[serde(flatten)]
    network: Network,
    tip: LiveTip,
}

impl From<(Network, Option<u64>)> for NetworkResponse {
    fn from((network, known): (Network, Option<u64>)) -> Self {
        let stored = network.index.as_ref().and_then(|index| index.tip);
        let tip = LiveTip {
            live: known.or(stored),
        };
        Self { network, tip }
    }
}

/// Every stored network.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = NetworkList)]
struct ListResponse {
    /// Networks in chain, then name, order.
    networks: Vec<NetworkResponse>,
    /// Every chain this server runs, which `POST /v1/networks` takes.
    chains: Vec<String>,
}

#[scope::utoipa(
    get,
    path = "/v1/networks",
    operation_id = "networks_list",
    summary = "List every network, its assets, its RPC nodes, its indexing and its tip",
    description = "Every stored network: its row, its assets, its routes, its nodes (each \
        URL with its calls and, while disabled, the record that disabled it: `disabled`) \
        and its `index` (its `network_index` row and why its index job last failed, \
        `job_reason`; null: neither). `tip.live` is the highest tip its nodes last told, \
        kept in memory: no node is asked.",
    responses(
        (status = 200, description = "Every network", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 503, description = "The networks could not be read", body = Error)
    ),
    tag = "networks"
)]
async fn list(
    session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ListResponse>, Error> {
    scope::require!(session, "com.hashinbox.networks.$read");
    let networks = networks::overview(&state.container, &state.db).await?;
    let networks = networks.into_iter().map(Into::into).collect();
    let chains = state.chains.names();
    Ok(Json(ListResponse { networks, chains }))
}

/// A new network's native currency.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct NativeAsset {
    /// Display symbol, such as `ETH`.
    ticker: String,
    /// Fractional digits between the display unit and the atomic unit.
    decimals: u32,
}

/// `POST /v1/networks` body.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[schema(as = NewNetwork)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// A chain of `GET /v1/networks`' `chains`.
    chain: String,
    /// The network's name: 1 to 64 of `a-z`, `0-9` and `-` (`my-devnet`).
    /// Bitcoin's names its network (`mainnet`, `testnet`, `signet`,
    /// `regtest`).
    network: String,
    /// The chain's own id of the network, as its nodes report it: the EVM
    /// chain id, the Solana or Tron genesis hash; null or empty for
    /// Bitcoin, which the name identifies.
    chain_id: Option<String>,
    /// Whether its coins are test coins.
    testnet: bool,
    /// The chain's block (slot) time, in milliseconds.
    block_time_ms: u64,
    /// Positions below the tip before a block counts as confirmed.
    confirmations: u64,
    native: NativeAsset,
}

impl From<CreateRequest> for Network {
    /// Enabled, no node, route or index yet; the chain's default settings.
    fn from(request: CreateRequest) -> Self {
        let chain_id = request.chain_id.map(|id| id.trim().to_owned());
        Self {
            chain: request.chain,
            network: request.network,
            chain_id: chain_id.filter(|id| !id.is_empty()),
            testnet: request.testnet,
            enabled: true,
            block_time_ms: request.block_time_ms,
            confirmations: request.confirmations,
            max_depth: None,
            explorer_address: None,
            explorer_transaction: None,
            settings: serde_json::json!({}),
            assets: vec![NetworkAsset {
                asset: "native".to_owned(),
                kind: NetworkAssetKind::Native,
                contract: None,
                ticker: request.native.ticker,
                decimals: request.native.decimals,
                display_decimals: None,
            }],
            routes: Vec::new(),
            index: None,
            endpoints: Default::default(),
        }
    }
}

#[scope::utoipa(
    post,
    path = "/v1/networks",
    operation_id = "networks_create",
    summary = "Add a network",
    description = "Stores a network and its native currency in one write, checked by its \
        chain as a start reads it, then registers it at once, with no node: add its nodes \
        with `POST /v1/networks/{chain}/{network}/endpoints`, and it indexes from then on.",
    request_body = CreateRequest,
    responses(
        (status = 201, description = "Stored and registered", body = NetworkResponse),
        (status = 400, description = "Malformed body, a name that is not a slug, an unknown \
            chain, or a network its chain refuses", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 409, description = "The network exists already", body = Error),
        (status = 503, description = "The network could not be stored", body = Error)
    ),
    tag = "networks"
)]
async fn create(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    request: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<NetworkResponse>), Error> {
    scope::require!(caller, "com.hashinbox.networks.$write");
    let Json(request) = request?;
    let (container, db, chains) = (&state.container, &state.db, &state.chains);
    let network = networks::create(container, db, chains, request.into()).await?;
    Ok((StatusCode::CREATED, Json((network, None).into())))
}

/// The network a route acts on.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
pub(super) struct NetworkPath {
    /// Chain name, as `GET /v1/networks` lists it.
    chain: String,
    /// Network name, as `GET /v1/networks` lists it.
    network: String,
}

impl NetworkPath {
    /// The network it names.
    pub(super) fn network(&self) -> base::Network {
        base::Network::new(base::Chain::new(&self.chain), self.network.clone())
    }
}
