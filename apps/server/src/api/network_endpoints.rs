//! `/v1/networks/{chain}/{network}/endpoints`: a network's RPC nodes, and
//! changing them while the server runs: add, replace the list (which also
//! reorders it), or remove one by id; `POST /v1/networks/endpoints` adds
//! nodes to several networks at once. A node is one URL; what a provider
//! needs (keys, headers) is written into it. A change is stored, then every
//! later call of the network goes to the new list and its routes. A node
//! kept by URL keeps its id and routes; a removed one leaves its routes.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, networks::NetworkPath};
use crate::{
    api::AppState,
    models::{Endpoint, NetworkName},
    usecase::networks::{self, Nodes},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list, add, replace))
        .routes(routes!(add_many))
        .routes(routes!(remove))
}

/// A network's nodes.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = NetworkEndpoints)]
struct EndpointsResponse {
    network: NetworkName,
    /// Its nodes as stored, in failover order.
    endpoints: Vec<Endpoint>,
}

impl From<Nodes> for EndpointsResponse {
    fn from(nodes: Nodes) -> Self {
        Self {
            network: nodes.network.into(),
            endpoints: nodes.endpoints,
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/endpoints",
    operation_id = "list_network_endpoints",
    summary = "List a network's RPC nodes",
    description = "Its stored nodes in stored order: each one's id and URL, as `PUT` takes \
        them, and what the running server knows of it (`state`: last tip, failure streak, \
        calls).",
    params(NetworkPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The network's nodes", body = EndpointsResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network is not configured", body = ErrorBody),
        (status = 503, description = "The nodes could not be read", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn list(
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
) -> Result<Json<EndpointsResponse>, Error> {
    let nodes = networks::endpoints(&state.container, &state.db, &path.network()).await?;
    Ok(Json(nodes.into()))
}

/// `POST` and `PUT /v1/networks/{chain}/{network}/endpoints` body.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[schema(as = RpcNodes)]
#[serde(deny_unknown_fields)]
struct NodesRequest {
    /// One to 16 distinct node URLs, in failover order. A URL carries everything:
    /// userinfo (HTTP Basic), query keys, a key in the path,
    /// `header.<Name>=value` parameters sent as headers, and `rpc.` settings
    /// (`timeout_ms`, `max_request_bytes`, `max_response_bytes`, `max_batch`,
    /// `rps`: most calls a second, e.g. `3.5`; a request waits for its turn).
    #[schema(min_items = 1, max_items = 16)]
    endpoints: Vec<String>,
}

impl NodesRequest {
    /// The nodes, in order.
    fn nodes(self) -> Vec<rpc::Endpoint> {
        self.endpoints.into_iter().map(rpc::Endpoint::new).collect()
    }
}

#[utoipa::path(
    post,
    path = "/v1/networks/{chain}/{network}/endpoints",
    operation_id = "add_network_endpoints",
    summary = "Add RPC nodes to a network",
    description = "Appends the nodes after the network's current ones, stores the list, then \
        sends every later call of the network to it.",
    params(NetworkPath),
    request_body = NodesRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Nodes stored and in use", body = EndpointsResponse),
        (status = 400, description = "Malformed node, a list above 16 nodes, or a node that \
            fails or serves another chain", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network is not configured", body = ErrorBody),
        (status = 503, description = "The nodes could not be stored", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn add(
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
    request: Result<Json<NodesRequest>, JsonRejection>,
) -> Result<Json<EndpointsResponse>, Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let (container, db, network) = (&state.container, &state.db, path.network());
    let nodes = networks::add_endpoints(container, db, &network, request.nodes()).await?;
    Ok(Json(nodes.into()))
}

#[utoipa::path(
    put,
    path = "/v1/networks/{chain}/{network}/endpoints",
    operation_id = "replace_network_endpoints",
    summary = "Replace a network's RPC nodes",
    description = "Stores the nodes, then sends every later call of the network (indexing, \
        wallets, broadcast) to them: the healthy node with the highest tip first, then the \
        others in order; calls already sent finish on the node they reached. Each node is asked \
        its chain and tip once, before storing. A URL already stored keeps its id, \
        routes and state; one left out is removed with its routes.",
    params(NetworkPath),
    request_body = NodesRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Nodes stored and in use", body = EndpointsResponse),
        (status = 400, description = "Malformed node list, or a node that fails or serves \
            another chain", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network is not configured", body = ErrorBody),
        (status = 503, description = "The nodes could not be stored", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn replace(
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
    request: Result<Json<NodesRequest>, JsonRejection>,
) -> Result<Json<EndpointsResponse>, Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let (container, db, network) = (&state.container, &state.db, path.network());
    let nodes = networks::replace_endpoints(container, db, &network, request.nodes()).await?;
    Ok(Json(nodes.into()))
}

/// One node to add, and its network.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct NetworkNode {
    chain: String,
    network: String,
    /// The node's URL, written as `endpoints` writes one.
    endpoint: String,
}

/// `POST /v1/networks/endpoints` body.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[schema(as = RpcAdditions)]
#[serde(deny_unknown_fields)]
struct AddManyRequest {
    /// Nodes to append, each after its network's current ones, in order.
    #[schema(min_items = 1, max_items = 64)]
    nodes: Vec<NetworkNode>,
}

impl AddManyRequest {
    /// The nodes, each with its network, in order: 1 to 64.
    fn nodes(self) -> Result<Vec<(base::Network, rpc::Endpoint)>, Error> {
        if self.nodes.is_empty() || self.nodes.len() > 64 {
            return Err(Error::invalid_request("add 1 to 64 nodes at once"));
        }
        let nodes = self.nodes.into_iter().map(|node| {
            let network = base::Network::new(base::Chain::new(node.chain), node.network);
            (network, rpc::Endpoint::new(node.endpoint))
        });
        Ok(nodes.collect())
    }
}

/// The networks a change touched.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = NetworkEndpointsList)]
struct AddManyResponse {
    /// Networks in chain, then name, order.
    networks: Vec<EndpointsResponse>,
}

#[utoipa::path(
    post,
    path = "/v1/networks/endpoints",
    operation_id = "add_endpoints",
    summary = "Add RPC nodes to one or more networks",
    description = "Appends each node after its network's current ones, stores every changed \
        list in one write (all or none), then sends every later call of those networks to \
        their new lists. A provider's nodes for several networks are added at once this way.",
    request_body = AddManyRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Nodes stored and in use", body = AddManyResponse),
        (status = 400, description = "Malformed node, a list above 16 nodes, or a node that \
            fails or serves another chain", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "A network is not configured", body = ErrorBody),
        (status = 503, description = "The nodes could not be stored", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn add_many(
    State(state): State<Arc<AppState>>,
    request: Result<Json<AddManyRequest>, JsonRejection>,
) -> Result<Json<AddManyResponse>, Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let changed = networks::add_endpoints_each(&state.container, &state.db, request.nodes()?);
    let networks = changed.await?.into_iter().map(Into::into).collect();
    Ok(Json(AddManyResponse { networks }))
}

/// One node of a network.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct NodePath {
    chain: String,
    network: String,
    /// The node's `id`, as `endpoints` lists it.
    id: i64,
}

#[utoipa::path(
    delete,
    path = "/v1/networks/{chain}/{network}/endpoints/{id}",
    operation_id = "remove_network_endpoint",
    summary = "Remove one RPC node of a network",
    description = "Removes the node `id` and its routes, stores the rest and uses them from \
        now on. A network's last node cannot be removed.",
    params(NodePath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Node removed", body = EndpointsResponse),
        (status = 400, description = "It is the network's last node", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "No such node, or network not configured",
            body = ErrorBody),
        (status = 503, description = "The nodes could not be stored", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn remove(
    State(state): State<Arc<AppState>>,
    Path(path): Path<NodePath>,
) -> Result<Json<EndpointsResponse>, Error> {
    let network = base::Network::new(base::Chain::new(path.chain), path.network);
    let nodes = networks::remove_endpoint(&state.container, &state.db, &network, path.id);
    Ok(Json(nodes.await?.into()))
}
