//! `GET /health`: readiness and every indexed network's indexing.

use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::AppState,
    models::{Index, Network, NetworkName, network},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(read))
}

/// One network's indexing.
#[derive(Clone, Debug, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct NetworkHealth {
    /// The indexed chain and network.
    network: NetworkName,
    /// Its indexing; always present, null: no tip known and no index job
    /// reason yet.
    #[schema(required)]
    indexing: Option<Index>,
    /// How many of its RPC nodes are enabled; 0: no call can go out, its
    /// index job idles (its reason says so) and its payments wait.
    enabled_endpoints: usize,
}

impl From<Network> for NetworkHealth {
    fn from(network: Network) -> Self {
        Self {
            enabled_endpoints: network.endpoints.enabled(),
            network: network.key().into(),
            indexing: network.index,
        }
    }
}

/// The health of every indexed network.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = Report)]
struct ReadResponse {
    /// At least one network has an index, and no index job has a reason.
    ready: bool,
    /// Every indexed network.
    networks: Vec<NetworkHealth>,
}

impl From<Vec<Network>> for ReadResponse {
    fn from(networks: Vec<Network>) -> Self {
        let networks: Vec<NetworkHealth> = networks
            .into_iter()
            .filter(|network| network.enabled)
            .map(Into::into)
            .collect();
        let indexes = networks.iter().map(|network| network.indexing.as_ref());
        let indexed = indexes.clone().any(|index| index.is_some());
        let failing = indexes.flatten().any(|index| index.job_reason.is_some());
        let unserved = networks
            .iter()
            .any(|network| network.enabled_endpoints == 0);
        Self {
            ready: indexed && !failing && !unserved,
            networks,
        }
    }
}

#[utoipa::path(
    get,
    path = "/health",
    operation_id = "read_health",
    summary = "Read readiness and every indexed network's indexing",
    description = "Public readiness: ready when at least one network has an index, every \
        indexed network has an enabled RPC node, and no network's index job has a reason \
        (`job_reason`: it failed, or goes again because of it). Reads the stored networks; \
        no RPC.",
    responses(
        (status = 200, description = "Ready", body = ReadResponse),
        (status = 503, description = "Not ready", body = ReadResponse)
    ),
    tag = "health"
)]
async fn read(State(state): State<Arc<AppState>>) -> (StatusCode, Json<ReadResponse>) {
    // A failed read is not ready.
    let networks = state.db.read(network::db::find_all).await;
    let health = ReadResponse::from(networks.unwrap_or_default());
    let status = if health.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(health))
}
