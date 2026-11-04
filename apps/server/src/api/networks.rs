//! `GET /v1/networks`: every configured network, its stored row (its
//! indexer's last read of the node) and RPC nodes, the node's tip asked now,
//! and its configuration.

use std::{sync::Arc, time::Duration};

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{
    api::AppState,
    models::{Explorer, Network, NetworkName, RpcNode, network},
};

/// Longest the list waits for one network's node to tell its tip.
const LIVE: Duration = Duration::from_secs(2);

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(list))
}

/// One configured network: its stored row and nodes, the node's tip now,
/// and its configuration.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct ConfiguredNetwork {
    network: NetworkName,
    /// The node's newest block at the indexer's last read; always present,
    /// null before the first.
    #[schema(required)]
    tip: Option<u64>,
    /// `tip - confirmations`; always present.
    #[schema(required)]
    confirmed: Option<u64>,
    /// The last kept block; always present, null: none yet.
    #[schema(required)]
    indexed: Option<u64>,
    /// Unix ms of the indexer's last read whose tip moved; always present.
    #[schema(required)]
    checked_at: Option<u64>,
    /// The node's newest block, asked now; `tip` when that failed. Always
    /// present.
    #[schema(required)]
    tip_live: Option<u64>,
    /// Why asking the node failed; always present, null when it answered.
    #[schema(required)]
    tip_error: Option<String>,
    /// The indexer's last failure since the process started, cleared by the
    /// next kept block; always present.
    #[schema(required)]
    index_error: Option<String>,
    /// Its nodes in failover order.
    endpoints: Vec<RpcNode>,
    /// Its block explorer links; always present, null when none is configured.
    #[schema(required)]
    explorer: Option<Explorer>,
    /// How old, in days, a wallet's start may be; null: no limit. Always
    /// present.
    #[schema(required)]
    max_history_days: Option<u64>,
}

/// Every configured network.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = NetworkList)]
struct ListResponse {
    /// Networks in chain, then name, order.
    networks: Vec<ConfiguredNetwork>,
}

#[utoipa::path(
    get,
    path = "/v1/networks",
    operation_id = "list_networks",
    summary = "List every configured network, its indexing, its live tip and its RPC nodes",
    description = "Every network the configuration names: its stored row (the indexer's last \
        read of the node), its nodes, each URL parsed, and its configuration. `tip_live` asks \
        every network's node at once, each for at most 2 s; one that fails keeps the stored \
        `tip` and says why in `tip_error`.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Every configured network", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The networks could not be read", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn list(State(state): State<Arc<AppState>>) -> Result<Json<ListResponse>, Error> {
    let mut rows = state.db.read(network::db::find_all).await?;
    let networks = state.payments.services().iter().map(|(name, service)| {
        let row = rows.remove(name).unwrap_or_else(|| Network::new(name));
        let state = &state;
        async move {
            let live = match &service.dated {
                None => Err("this network cannot read its tip".to_owned()),
                Some(dated) => match tokio::time::timeout(LIVE, dated.tips()).await {
                    Err(_) => Err("the node did not answer in time".to_owned()),
                    Ok(tips) => tips.map(|tips| tips.produced).map_err(|e| format!("{e:#}")),
                },
            };
            ConfiguredNetwork {
                network: NetworkName {
                    chain: row.chain,
                    name: row.network,
                },
                tip: row.tip,
                confirmed: row.confirmed,
                indexed: row.indexed,
                checked_at: row.checked_at,
                tip_live: live.as_ref().ok().copied().or(row.tip_live),
                tip_error: live.err(),
                index_error: service.indexer.stats().error,
                endpoints: row.endpoints,
                explorer: state.explorers.get(name),
                max_history_days: service.max_history.map(|limit| limit.as_secs() / 86_400),
            }
        }
    });
    let networks = futures_util::future::join_all(networks.collect::<Vec<_>>()).await;
    Ok(Json(ListResponse { networks }))
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
    /// The network, when configured; a 404 when not.
    pub(super) fn network(&self, payments: &payments::Payments) -> Result<base::Network, Error> {
        let network = base::Network::new(base::Chain::new(&self.chain), self.network.clone());
        payments.service(&network)?;
        Ok(network)
    }
}
