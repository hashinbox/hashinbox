//! `GET /v1/networks`: every stored network (its row, assets, nodes and
//! index), and the node's tip asked now.

use std::sync::Arc;

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{api::AppState, models::Network, usecase::networks};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(list))
}

/// The node's tip, asked now.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct LiveTip {
    /// The node's newest block; the stored `index.tip` when asking failed.
    /// Always present.
    #[schema(required)]
    live: Option<u64>,
    /// Why asking the node failed; always present, null when it answered.
    #[schema(required)]
    error: Option<String>,
}

/// One stored network, and its node's tip asked now.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = ConfiguredNetwork)]
struct NetworkResponse {
    #[serde(flatten)]
    network: Network,
    tip: LiveTip,
}

impl From<(Network, Result<u64, String>)> for NetworkResponse {
    fn from((network, asked): (Network, Result<u64, String>)) -> Self {
        let stored = network.index.as_ref().and_then(|index| index.tip);
        let tip = match asked {
            Ok(tip) => LiveTip {
                live: Some(tip),
                error: None,
            },
            Err(error) => LiveTip {
                live: stored,
                error: Some(error),
            },
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

#[utoipa::path(
    get,
    path = "/v1/networks",
    operation_id = "list_networks",
    summary = "List every network, its assets, its RPC nodes, its indexing and its live tip",
    description = "Every stored network: its row, its assets, its routes, its nodes (each \
        URL with its calls and, while disabled, the record that disabled it: `disabled`) \
        and its `index` (its `network_index` row and why its index job last failed, \
        `job_reason`; null: neither). `tip.live` asks every running network's node at \
        once, each for at most 2 s; one that fails keeps the stored `index.tip` and says \
        why in `tip.error`.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Every network", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The networks could not be read", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn list(State(state): State<Arc<AppState>>) -> Result<Json<ListResponse>, Error> {
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
