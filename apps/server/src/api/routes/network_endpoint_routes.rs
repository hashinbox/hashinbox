//! `/v1/networks/{chain}/{network}/routes`: which methods of a network take
//! turns over which of its RPC nodes, and replacing them while the server
//! runs. Every other method goes to all the network's nodes, highest tip
//! first (`usecase::Transport`).

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::models::Caller;
use crate::models::Session;

use super::networks::NetworkPath;
use crate::error::Error;
use crate::{
    api::AppState,
    models::{NetworkName, Route},
    usecase::networks,
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

#[scope::utoipa(
    get,
    path = "/v1/networks/{chain}/{network}/routes",
    operation_id = "list_network_routes",
    summary = "List a network's routed methods",
    description = "Each routed method and the nodes taking turns serving it, round robin. \
        Every other method goes to the network's healthy node with the highest tip.",
    params(NetworkPath),
    responses(
        (status = 200, description = "The network's routes", body = RoutesResponse),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 404, description = "Network is not configured", body = Error),
        (status = 503, description = "The routes could not be read", body = Error)
    ),
    tag = "networks"
)]
async fn list(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
) -> Result<Json<RoutesResponse>, Error> {
    scope::require!(session, "com.hashinbox.endpoints.$read");
    let network = path.network();
    let routes = networks::routes(&state.container, &state.db, &network).await?;
    Ok(Json(RoutesResponse {
        routes,
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

#[scope::utoipa(
    put,
    path = "/v1/networks/{chain}/{network}/routes",
    operation_id = "replace_network_routes",
    summary = "Replace a network's routed methods",
    description = "Stores the routes, then sends each routed method round robin over its \
        nodes, and every other method to the healthy node with the highest tip. A routed \
        call failing retryably moves to the next node, then to the network's other nodes. \
        No node is asked.",
    params(NetworkPath),
    request_body = RoutesRequest,
    responses(
        (status = 200, description = "Routes stored and in use", body = RoutesResponse),
        (status = 400, description = "An empty method, a method routed twice, or a node that \
            is missing, repeated or of another network", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 404, description = "Network is not configured", body = Error),
        (status = 503, description = "The routes could not be stored", body = Error)
    ),
    tag = "networks"
)]
async fn replace(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
    request: Result<Json<RoutesRequest>, JsonRejection>,
) -> Result<Json<RoutesResponse>, Error> {
    scope::require!(caller, "com.hashinbox.endpoints.$write");
    let Json(request) = request?;
    let network = path.network();
    let routes = networks::replace_routes(&state.container, &state.db, &network, request.routes);
    Ok(Json(RoutesResponse {
        routes: routes.await?,
        network: network.into(),
    }))
}
