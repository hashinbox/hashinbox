//! `/v1/networks/{chain}/{network}/routes`: which methods of a network take
//! turns over which of its RPC nodes (`rpc::Router`), and replacing them
//! while the server runs. Every other method goes to the cluster of all the
//! network's nodes.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, networks::NetworkPath};
use crate::{
    api::AppState,
    models::{NetworkName, Route, Routes, endpoint, endpoint_route},
    usecase::endpoints::connect,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(list, replace))
}

/// A network's routed methods.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = NetworkRoutes)]
struct RoutesResponse {
    network: NetworkName,
    /// By method; each one's nodes in failover order.
    routes: Vec<Route>,
}

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/routes",
    operation_id = "list_network_routes",
    summary = "List a network's routed methods",
    description = "Each routed method and the nodes taking turns serving it, round robin. \
        Every other method goes to the network's cluster.",
    params(NetworkPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The network's routes", body = RoutesResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network is not configured", body = ErrorBody),
        (status = 503, description = "The routes could not be read", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn list(
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
) -> Result<Json<RoutesResponse>, Error> {
    let network = path.network(&state.payments)?;
    let stored = state.db.read(endpoint_route::db::find_all).await?;
    Ok(Json(RoutesResponse {
        routes: stored.routes(&network),
        network: network.into(),
    }))
}

/// `PUT /v1/networks/{chain}/{network}/routes` body.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[schema(as = RpcRoutes)]
#[serde(deny_unknown_fields)]
struct RoutesRequest {
    /// Every route of the network; empty: none. A method appears once, over
    /// one or more of the network's node ids, each once.
    routes: Vec<Route>,
}

#[utoipa::path(
    put,
    path = "/v1/networks/{chain}/{network}/routes",
    operation_id = "replace_network_routes",
    summary = "Replace a network's routed methods",
    description = "Stores the routes, then sends each routed method round robin over its \
        nodes, and every other method to the cluster. A routed call failing retryably moves \
        to the next node, then to the cluster. No node is asked.",
    params(NetworkPath),
    request_body = RoutesRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Routes stored and in use", body = RoutesResponse),
        (status = 400, description = "An empty method, a method routed twice, or a node that \
            is missing, repeated or of another network", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network is not configured", body = ErrorBody),
        (status = 503, description = "The routes could not be stored", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn replace(
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
    request: Result<Json<RoutesRequest>, JsonRejection>,
) -> Result<Json<RoutesResponse>, Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let network = path.network(&state.payments)?;
    let endpoints = state.db.read(endpoint::db::find_all).await?.nodes(&network);
    let routes = Routes::new(request.routes, &endpoints).map_err(Error::invalid_request)?;
    let key = network.clone();
    // Stored before connected: a restart never forgets a change in use.
    state
        .db
        .write(move |connection| endpoint_route::db::replace(connection, &key, &routes))
        .await?;
    let (_, routes) = connect(&state.db, &state.payments, &network).await?;
    Ok(Json(RoutesResponse {
        network: network.into(),
        routes,
    }))
}
