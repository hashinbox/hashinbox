//! `/v1/networks/{chain}/{network}/endpoints`: a network's RPC nodes, and
//! changing them while the server runs: add, replace the list, or remove
//! one; `POST /v1/networks/endpoints` adds nodes to several networks at
//! once. A node is one URL; what a provider needs (keys, headers) is written
//! into it. A change is stored, then every later call of the network goes to
//! the new list.

use std::{collections::BTreeMap, sync::Arc};

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
    models::{NetworkName, RpcEndpoints, RpcNode, endpoint},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list, add, replace))
        .routes(routes!(add_many))
        .routes(routes!(remove))
}

/// One RPC node, written as the configuration file's `endpoints` writes it.
/// The URL carries everything: userinfo (HTTP Basic), query keys, a key in
/// the path, or `header.<Name>=value` parameters sent as headers.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
enum RpcEndpoint {
    /// The node's URL, with default bounds.
    Url(String),
    /// The node's URL and bounds.
    Detailed(DetailedEndpoint),
}

/// A node with its own bounds.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct DetailedEndpoint {
    /// `http` or `https` URL naming a host, without a fragment.
    url: String,
    /// Per-call limits: `timeout_ms`, `max_request_bytes`,
    /// `max_response_bytes`; each defaults when absent.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    bounds: rpc::Bounds,
}

impl From<RpcEndpoint> for rpc::Endpoint {
    fn from(endpoint: RpcEndpoint) -> Self {
        match endpoint {
            RpcEndpoint::Url(url) => Self::new(url),
            RpcEndpoint::Detailed(detailed) => Self::new(detailed.url).with_bounds(detailed.bounds),
        }
    }
}

/// A network's nodes.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = NetworkEndpoints)]
struct EndpointsResponse {
    network: NetworkName,
    /// Its nodes in failover order.
    endpoints: Vec<RpcNode>,
}

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/endpoints",
    operation_id = "list_network_endpoints",
    summary = "List a network's RPC nodes",
    description = "Its stored nodes in failover order, each URL parsed.",
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
    let network = path.network(&state.payments)?;
    let stored = state.db.read(endpoint::db::find_all).await?;
    Ok(Json(EndpointsResponse {
        endpoints: stored
            .rows()
            .get(&network)
            .map(RpcEndpoints::shown)
            .unwrap_or_default(),
        network: network.into(),
    }))
}

/// `POST` and `PUT /v1/networks/{chain}/{network}/endpoints` body.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[schema(as = RpcNodes)]
#[serde(deny_unknown_fields)]
struct NodesRequest {
    /// One to 16 nodes, in failover order.
    #[schema(min_items = 1, max_items = 16)]
    endpoints: Vec<RpcEndpoint>,
}

impl NodesRequest {
    /// The nodes, in order.
    fn nodes(self) -> Vec<rpc::Endpoint> {
        self.endpoints.into_iter().map(Into::into).collect()
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
    let network = path.network(&state.payments)?;
    if request.endpoints.is_empty() {
        return Err(Error::invalid_request("add 1 to 16 nodes at once"));
    }
    let mut nodes = state.db.read(endpoint::db::find_all).await?.nodes(&network);
    let added = request.nodes();
    nodes.extend(added.clone());
    let nodes = RpcEndpoints::new(nodes).map_err(Error::invalid_request)?;
    check(&state, &network, &added).await?;
    store(&state, &network, &nodes).await?;
    Ok(Json(install(&state, network, &nodes)?))
}

#[utoipa::path(
    put,
    path = "/v1/networks/{chain}/{network}/endpoints",
    operation_id = "replace_network_endpoints",
    summary = "Replace a network's RPC nodes",
    description = "Stores the nodes, then sends every later call of the network (indexing, \
        wallets, broadcast) to them, failing over in order; calls already sent finish on the \
        node they reached. Each node is asked its chain and tip once, before storing.",
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
    let network = path.network(&state.payments)?;
    let nodes = RpcEndpoints::new(request.nodes()).map_err(Error::invalid_request)?;
    check(&state, &network, nodes.nodes()).await?;
    store(&state, &network, &nodes).await?;
    Ok(Json(install(&state, network, &nodes)?))
}

/// One node to add, and its network.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct NetworkNode {
    chain: String,
    network: String,
    endpoint: RpcEndpoint,
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
    if request.nodes.is_empty() || request.nodes.len() > 64 {
        return Err(Error::invalid_request("add 1 to 64 nodes at once"));
    }
    let stored = state.db.read(endpoint::db::find_all).await?;
    let mut lists: BTreeMap<base::Network, Vec<rpc::Endpoint>> = BTreeMap::new();
    let mut added = Vec::new();
    for node in request.nodes {
        let network = base::Network::new(base::Chain::new(node.chain), node.network);
        state.payments.service(&network)?;
        let endpoint = rpc::Endpoint::from(node.endpoint);
        added.push((network.clone(), endpoint.clone()));
        lists
            .entry(network.clone())
            .or_insert_with(|| stored.nodes(&network))
            .push(endpoint);
    }
    let lists = lists
        .into_iter()
        .map(|(network, nodes)| {
            RpcEndpoints::new(nodes)
                .map(|nodes| (network, nodes))
                .map_err(Error::invalid_request)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for (network, endpoint) in added {
        check(&state, &network, &[endpoint]).await?;
    }
    let kept = lists.clone();
    // Stored before installed: a restart never forgets a change in use.
    state
        .db
        .write(move |connection| {
            for (network, nodes) in &kept {
                endpoint::db::upsert(connection, network, nodes)?;
            }
            Ok(())
        })
        .await?;
    let networks = lists
        .into_iter()
        .map(|(network, nodes)| install(&state, network, &nodes))
        .collect::<Result<_, _>>()?;
    Ok(Json(AddManyResponse { networks }))
}

/// One node of a network.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct NodePath {
    chain: String,
    network: String,
    /// The node's `index` in its network's `endpoints`.
    index: usize,
}

#[utoipa::path(
    delete,
    path = "/v1/networks/{chain}/{network}/endpoints/{index}",
    operation_id = "remove_network_endpoint",
    summary = "Remove one RPC node of a network",
    description = "Removes the node at `index`, stores the rest and uses them from now on. A \
        network's last node cannot be removed.",
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
    state.payments.service(&network)?;
    let mut nodes = state.db.read(endpoint::db::find_all).await?.nodes(&network);
    if path.index >= nodes.len() {
        return Err(Error::not_found("no such node"));
    }
    nodes.remove(path.index);
    let nodes = RpcEndpoints::new(nodes)
        .map_err(|_| Error::invalid_request("a network keeps at least one node"))?;
    store(&state, &network, &nodes).await?;
    Ok(Json(install(&state, network, &nodes)?))
}

/// Asks each of `endpoints` which chain it serves and its tip, once, before
/// anything is stored: a node that fails or serves another chain is refused
/// with why. Nothing asks again while the server runs.
async fn check(
    state: &AppState,
    network: &base::Network,
    endpoints: &[rpc::Endpoint],
) -> Result<(), Error> {
    for (index, endpoint) in endpoints.iter().enumerate() {
        state
            .payments
            .info(network, endpoint)
            .await
            .map_err(|error| Error::invalid_request(format!("node {index}: {error:#}")))?;
    }
    Ok(())
}

/// Stores `nodes` as `network`'s list.
async fn store(
    state: &AppState,
    network: &base::Network,
    nodes: &RpcEndpoints,
) -> Result<(), Error> {
    let (network, nodes) = (network.clone(), nodes.clone());
    // Stored before installed: a restart never forgets a change in use.
    state
        .db
        .write(move |connection| endpoint::db::upsert(connection, &network, &nodes))
        .await?;
    Ok(())
}

/// Sends every later call of `network` to a cluster of `nodes`.
fn install(
    state: &AppState,
    network: base::Network,
    nodes: &RpcEndpoints,
) -> Result<EndpointsResponse, Error> {
    state.payments.set_endpoints(&network, nodes.nodes())?;
    Ok(EndpointsResponse {
        endpoints: nodes.shown(),
        network: network.into(),
    })
}
