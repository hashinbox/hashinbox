//! `GET /health`: readiness and every network's indexing, from memory.

use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::AppState,
    models::{Indexing, Network},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(read))
}

/// One network's indexing.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct NetworkHealth {
    /// The indexed chain and network.
    network: Network,
    /// What its indexer did since the process started.
    indexing: Indexing,
}

/// The memory-only health snapshot of this process.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = Report)]
struct ReadResponse {
    /// At least one network, and none whose last step failed.
    ready: bool,
    /// Every configured network.
    networks: Vec<NetworkHealth>,
}

#[utoipa::path(
    get,
    path = "/health",
    operation_id = "read_health",
    summary = "Read readiness and network health from memory",
    description = "Public memory-only readiness: ready when at least one network is configured \
        and no network's last indexing step failed. No RPC or storage reads.",
    responses(
        (status = 200, description = "Ready", body = ReadResponse),
        (status = 503, description = "Not ready", body = ReadResponse)
    ),
    tag = "health"
)]
async fn read(State(state): State<Arc<AppState>>) -> (StatusCode, Json<ReadResponse>) {
    let networks: Vec<_> = state
        .payments
        .stats()
        .into_iter()
        .map(|(network, stats)| NetworkHealth {
            network: network.into(),
            indexing: stats.into(),
        })
        .collect();
    let ready = !networks.is_empty()
        && networks
            .iter()
            .all(|network| network.indexing.error.is_none());
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(ReadResponse { ready, networks }))
}
