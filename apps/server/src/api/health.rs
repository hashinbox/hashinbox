//! `GET /health`: readiness and every indexed network's `network_index` row.

use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::AppState,
    models::{NetworkIndex, NetworkName},
    usecase::stats,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(read))
}

/// One network's indexing.
#[derive(Clone, Debug, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct NetworkHealth {
    /// The indexed chain and network.
    network: NetworkName,
    /// What its indexer last wrote.
    indexing: NetworkIndex,
}

/// The health of every indexed network.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = Report)]
struct ReadResponse {
    /// At least one network, and no index job failing.
    ready: bool,
    /// Every indexed network.
    networks: Vec<NetworkHealth>,
}

#[utoipa::path(
    get,
    path = "/health",
    operation_id = "read_health",
    summary = "Read readiness and every indexed network's indexing",
    description = "Public readiness: ready when at least one network is indexed and no \
        index job is failing (failed, or going again with a `reason`). Reads the \
        `network_index` rows and the job queue; no RPC.",
    responses(
        (status = 200, description = "Ready", body = ReadResponse),
        (status = 503, description = "Not ready", body = ReadResponse)
    ),
    tag = "health"
)]
async fn read(State(state): State<Arc<AppState>>) -> (StatusCode, Json<ReadResponse>) {
    let health = stats::health(&state.db).await;
    let status = if health.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(health.into()))
}

impl From<stats::Health> for ReadResponse {
    fn from(health: stats::Health) -> Self {
        let networks = health.networks.into_iter();
        Self {
            ready: health.ready,
            networks: networks
                .map(|(network, indexing)| NetworkHealth {
                    network: network.into(),
                    indexing,
                })
                .collect(),
        }
    }
}
