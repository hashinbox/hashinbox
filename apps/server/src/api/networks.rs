//! `/v1/networks`: every configured network, its indexing and its RPC nodes,
//! and changing those nodes while the server runs: replace a network's list,
//! add nodes to several networks at once, or remove one. A node is one URL;
//! what a provider needs (keys, headers) is written into it.

use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{
    models::{Explorer, Indexing, Network, RpcEndpoints, endpoint},
    state::AppState,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(add))
        .routes(routes!(replace))
        .routes(routes!(remove))
}

/// One query parameter of a node's URL.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct RpcParameter {
    /// The parameter's name, or for a `header.<Name>` one, the header's.
    name: String,
    value: String,
    /// Whether every call sends it as a header instead of in the URL.
    header: bool,
}

/// One RPC node: its URL, parsed.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct RpcNode {
    /// Its position in failover order; what removing it takes.
    index: usize,
    /// The URL as stored.
    url: String,
    /// Scheme, host and port.
    origin: String,
    scheme: String,
    host: String,
    /// The port, the scheme's default when unwritten; always present.
    #[schema(required)]
    port: Option<u16>,
    path: String,
    /// The user of `user:password`, sent as HTTP Basic; empty when none.
    username: String,
    /// Always present.
    #[schema(required)]
    password: Option<String>,
    /// Its query parameters, in order.
    parameters: Vec<RpcParameter>,
    /// Longest one call may take, in milliseconds.
    timeout_ms: u64,
    /// Largest request body, in bytes.
    max_request_bytes: u32,
    /// Largest response body, in bytes.
    max_response_bytes: u32,
}

impl RpcNode {
    fn new(index: usize, (url, parts, bounds): (String, rpc::Parts, rpc::Bounds)) -> Self {
        Self {
            index,
            url,
            origin: parts.origin,
            scheme: parts.scheme,
            host: parts.host,
            port: parts.port,
            path: parts.path,
            username: parts.username,
            password: parts.password,
            parameters: parts
                .parameters
                .into_iter()
                .map(|parameter| RpcParameter {
                    name: parameter.name,
                    value: parameter.value,
                    header: parameter.header,
                })
                .collect(),
            timeout_ms: bounds.timeout_ms,
            max_request_bytes: bounds.max_request_bytes,
            max_response_bytes: bounds.max_response_bytes,
        }
    }
}

/// One configured network, its indexing, and its nodes.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct ConfiguredNetwork {
    network: Network,
    /// What its indexer did since the process started.
    indexing: Indexing,
    /// Its nodes in failover order.
    nodes: Vec<RpcNode>,
    /// Its block explorer links; always present, null when none is configured.
    #[schema(required)]
    explorer: Option<Explorer>,
    /// How old, in days, a wallet's start may be; null: no limit. Always
    /// present.
    #[schema(required)]
    max_history_days: Option<u64>,
}

/// Every configured network, or the ones a change touched.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = NetworkList)]
struct ListResponse {
    /// Networks in chain, then name, order.
    networks: Vec<ConfiguredNetwork>,
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
    bounds: Option<Value>,
}

impl From<RpcEndpoint> for Value {
    /// The document the configuration file would hold for the node.
    fn from(endpoint: RpcEndpoint) -> Self {
        match endpoint {
            RpcEndpoint::Url(url) => Self::String(url),
            RpcEndpoint::Detailed(detailed) => {
                let mut document = serde_json::Map::new();
                document.insert("url".to_owned(), Self::String(detailed.url));
                if let Some(bounds) = detailed.bounds {
                    document.insert("bounds".to_owned(), bounds);
                }
                Self::Object(document)
            }
        }
    }
}

/// `PUT /v1/networks/{chain}/{network}/rpc` body.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[schema(as = RpcNodes)]
#[serde(deny_unknown_fields)]
struct ReplaceRequest {
    /// One to 16 nodes, in failover order.
    #[schema(min_items = 1, max_items = 16)]
    endpoints: Vec<RpcEndpoint>,
}

/// One node to add, and its network.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct NetworkNode {
    chain: String,
    network: String,
    endpoint: RpcEndpoint,
}

/// `POST /v1/networks/rpc` body.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[schema(as = RpcAdditions)]
#[serde(deny_unknown_fields)]
struct AddRequest {
    /// Nodes to append, each after its network's current ones, in order.
    #[schema(min_items = 1, max_items = 64)]
    nodes: Vec<NetworkNode>,
}

/// The network a maintenance route acts on.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct NetworkPath {
    /// Chain name, as `GET /v1/networks` lists it.
    chain: String,
    /// Network name, as `GET /v1/networks` lists it.
    network: String,
}

/// One node of a network.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct NodePath {
    chain: String,
    network: String,
    /// The node's `index` in `GET /v1/networks`.
    index: usize,
}

#[utoipa::path(
    get,
    path = "/v1/networks",
    operation_id = "list_networks",
    summary = "List every configured network, its indexing and its RPC nodes",
    description = "Every network the configuration names, what its indexer did since the \
        process started, and its nodes, each URL parsed.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Every configured network", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The nodes could not be read", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn list(State(state): State<Arc<AppState>>) -> Result<Json<ListResponse>, Error> {
    let stored = state.storage.read(endpoint::db::find_all).await?;
    let stored = stored.rows();
    let networks = state
        .payments
        .services()
        .iter()
        .map(|(network, service)| ConfiguredNetwork {
            nodes: stored.get(network).map(nodes).unwrap_or_default(),
            explorer: state.explorers.get(network),
            network: network.clone().into(),
            indexing: service.indexer.stats().into(),
            max_history_days: max_history_days(service),
        })
        .collect();
    Ok(Json(ListResponse { networks }))
}

#[utoipa::path(
    post,
    path = "/v1/networks/rpc",
    operation_id = "add_network_rpc",
    summary = "Add RPC nodes to one or more networks",
    description = "Appends each node after its network's current ones, stores every changed \
        list in one write (all or none), then sends every later call of those networks to \
        their new lists. A provider's nodes for several networks are added at once this way.",
    request_body = AddRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Nodes stored and in use", body = ListResponse),
        (status = 400, description = "Malformed node, or a list above 16 nodes", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "A network is not configured", body = ErrorBody),
        (status = 503, description = "The nodes could not be stored", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn add(
    State(state): State<Arc<AppState>>,
    request: Result<Json<AddRequest>, JsonRejection>,
) -> Result<Json<ListResponse>, Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    if request.nodes.is_empty() || request.nodes.len() > 64 {
        return Err(Error::invalid_request("add 1 to 64 nodes at once"));
    }
    let stored = state
        .storage
        .read(endpoint::db::find_all)
        .await?
        .documents();
    let mut lists: BTreeMap<base::Network, Vec<Value>> = BTreeMap::new();
    for node in request.nodes {
        let network = base::Network::new(base::Chain::new(node.chain), node.network);
        state.payments.service(&network)?;
        lists
            .entry(network.clone())
            .or_insert_with(|| stored.get(&network).cloned().unwrap_or_default())
            .push(node.endpoint.into());
    }
    let lists = lists
        .into_iter()
        .map(|(network, documents)| {
            RpcEndpoints::new(documents)
                .map(|nodes| (network, nodes))
                .map_err(Error::invalid_request)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let kept = lists.clone();
    // Stored before installed: a restart never forgets a change in use.
    state
        .storage
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
    Ok(Json(ListResponse { networks }))
}

#[utoipa::path(
    put,
    path = "/v1/networks/{chain}/{network}/rpc",
    operation_id = "replace_network_rpc",
    summary = "Replace a network's RPC nodes",
    description = "Stores the nodes, then sends every later call of the network (indexing, \
        wallets, broadcast) to them, failing over in order; calls already sent finish on the \
        node they reached. The nodes are not verified here: the chain verifies them on the \
        next start, which uses the stored list whatever the configuration file says.",
    params(NetworkPath),
    request_body = ReplaceRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Nodes stored and in use", body = ConfiguredNetwork),
        (status = 400, description = "Malformed node list", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network is not configured", body = ErrorBody),
        (status = 503, description = "The nodes could not be stored", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn replace(
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
    request: Result<Json<ReplaceRequest>, JsonRejection>,
) -> Result<Json<ConfiguredNetwork>, Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let network = base::Network::new(base::Chain::new(path.chain), path.network);
    state.payments.service(&network)?;
    let documents = request.endpoints.into_iter().map(Value::from).collect();
    let nodes = RpcEndpoints::new(documents).map_err(Error::invalid_request)?;
    store(&state, &network, &nodes).await?;
    Ok(Json(install(&state, network, &nodes)?))
}

#[utoipa::path(
    delete,
    path = "/v1/networks/{chain}/{network}/rpc/{index}",
    operation_id = "remove_network_rpc",
    summary = "Remove one RPC node of a network",
    description = "Removes the node at `index`, stores the rest and uses them from now on. A \
        network's last node cannot be removed.",
    params(NodePath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Node removed", body = ConfiguredNetwork),
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
) -> Result<Json<ConfiguredNetwork>, Error> {
    let network = base::Network::new(base::Chain::new(path.chain), path.network);
    state.payments.service(&network)?;
    let mut documents = state
        .storage
        .read(endpoint::db::find_all)
        .await?
        .documents()
        .remove(&network)
        .unwrap_or_default();
    if path.index >= documents.len() {
        return Err(Error::not_found("no such node"));
    }
    documents.remove(path.index);
    let nodes = RpcEndpoints::new(documents)
        .map_err(|_| Error::invalid_request("a network keeps at least one node"))?;
    store(&state, &network, &nodes).await?;
    Ok(Json(install(&state, network, &nodes)?))
}

/// The shown form of `nodes`.
fn nodes(nodes: &RpcEndpoints) -> Vec<RpcNode> {
    nodes
        .views()
        .into_iter()
        .enumerate()
        .map(|(index, view)| RpcNode::new(index, view))
        .collect()
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
        .storage
        .write(move |connection| endpoint::db::upsert(connection, &network, &nodes))
        .await?;
    Ok(())
}

/// Sends every later call of `network` to `nodes`, failing over in order.
fn install(
    state: &AppState,
    network: base::Network,
    nodes: &RpcEndpoints,
) -> Result<ConfiguredNetwork, Error> {
    let client = state.payments.client(&network)?;
    let https = nodes
        .nodes()
        .iter()
        .map(rpc::Endpoint::connect)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::invalid_request(error.to_string()))?;
    let statistics = https.iter().map(rpc::Http::statistics).collect();
    let failover =
        rpc::Failover::new(https).map_err(|error| Error::invalid_request(error.to_string()))?;
    client.replace(Arc::new(failover), statistics);
    let service = state.payments.service(&network)?;
    Ok(ConfiguredNetwork {
        nodes: self::nodes(nodes),
        explorer: state.explorers.get(&network),
        indexing: service.indexer.stats().into(),
        max_history_days: max_history_days(service),
        network: network.into(),
    })
}

/// The network's history limit in whole days.
fn max_history_days(service: &base::Service) -> Option<u64> {
    service.max_history.map(|limit| limit.as_secs() / 86_400)
}
