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

/// Every stored network.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = NetworkList)]
struct ListResponse {
    /// Networks in chain, then name, order.
    networks: Vec<Network>,
}

#[utoipa::path(
    get,
    path = "/v1/networks",
    operation_id = "list_networks",
    summary = "List every network, its assets, its RPC nodes, its indexing and its live tip",
    description = "Every stored network: its row, its assets, its nodes (each URL with what \
        the running server knows of it: last tip, failure streak, calls) and \
        its `index` row (null: not indexed), and why its index job last failed \
        (`index_job_reason`). `tip_live` asks every running network's node at \
        once, each for at most 2 s; one that fails keeps the stored `tip` and says why in \
        `tip_error`.",
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
