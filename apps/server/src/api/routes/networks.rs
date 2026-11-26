//! `GET /v1/networks`: every stored network (its row, assets, nodes and
//! index), and the node's tip asked now.

use std::sync::Arc;

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::models::Session;

use crate::error::Error;
use crate::{api::AppState, models::Network, usecase::networks};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(list))
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
}

#[scope::utoipa(
    get,
    path = "/v1/networks",
    operation_id = "list_networks",
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
    /// The network it names.
    pub(super) fn network(&self) -> base::Network {
        base::Network::new(base::Chain::new(&self.chain), self.network.clone())
    }
}
