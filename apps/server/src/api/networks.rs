//! `/v1/networks`: every configured network, its indexing and stored RPC
//! nodes, and replacing those nodes while the server runs.

use std::sync::Arc;

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
    models::{Endpoint, Indexing, Network, RpcEndpoints},
    state::AppState,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(replace))
}

/// One RPC node as it may be shown.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct RpcNode {
    /// Scheme, host and port of the node's URL. User information, path and
    /// query are never returned: any of them may hold a credential.
    origin: String,
}

/// One configured network, its indexing, and its stored nodes.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct ConfiguredNetwork {
    network: Network,
    /// What its indexer did since the process started.
    indexing: Indexing,
    /// Stored nodes in failover order; null while the network uses its
    /// configuration file's nodes. Always present.
    #[schema(required)]
    nodes: Option<Vec<RpcNode>>,
}

/// `GET /v1/networks`: every configured network.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = NetworkList)]
struct ListResponse {
    /// Networks in chain, then name, order.
    networks: Vec<ConfiguredNetwork>,
}

/// One RPC node, written as the configuration file's `endpoints` writes it.
/// The URL carries any credentials: userinfo (HTTP Basic) or query keys.
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

/// The network a maintenance route acts on.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct NetworkPath {
    /// Chain name, as `GET /v1/networks` lists it.
    chain: String,
    /// Network name, as `GET /v1/networks` lists it.
    network: String,
}

#[utoipa::path(
    get,
    path = "/v1/networks",
    operation_id = "list_networks",
    summary = "List every configured network, its indexing and its stored RPC nodes",
    description = "Every network the configuration names, what its indexer did since the \
        process started, and the nodes stored for it. Only each node's origin is returned.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Every configured network", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The stored nodes could not be read", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn list(State(state): State<Arc<AppState>>) -> Result<Json<ListResponse>, Error> {
    let stored = state.storage.endpoints().await?.documents();
    let networks = state
        .payments
        .stats()
        .into_iter()
        .map(|(network, stats)| ConfiguredNetwork {
            nodes: stored.get(&network).map(|documents| {
                documents
                    .iter()
                    .map(|document| RpcNode {
                        origin: Endpoint::new(document.clone()).map_or_else(
                            |_| "unresolved".to_owned(),
                            |node| node.origin().to_owned(),
                        ),
                    })
                    .collect()
            }),
            network: network.into(),
            indexing: stats.into(),
        })
        .collect();
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
        next start, which uses the stored list whatever the configuration file says. \
        Credentials in the URL are stored as sent.",
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
    let client = state.payments.client(&network)?;
    let documents = request.endpoints.into_iter().map(Value::from).collect();
    let nodes = RpcEndpoints::new(documents).map_err(Error::invalid_request)?;
    let https = nodes
        .nodes()
        .iter()
        .map(json_rpc::Endpoint::connect)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::invalid_request(error.to_string()))?;
    let statistics = https.iter().map(json_rpc::Http::statistics).collect();
    let failover = json_rpc::Failover::new(https)
        .map_err(|error| Error::invalid_request(error.to_string()))?;
    let shown = nodes
        .documents()
        .into_iter()
        .map(|document| RpcNode {
            origin: Endpoint::new(document)
                .map_or_else(|_| "unresolved".to_owned(), |node| node.origin().to_owned()),
        })
        .collect();
    // Stored before it is installed: a restart never forgets a change in use.
    state.storage.set_endpoints(&network, nodes).await?;
    client.replace(Arc::new(failover), statistics);
    let stats = state.payments.service(&network)?.indexer.stats();
    Ok(Json(ConfiguredNetwork {
        network: network.into(),
        indexing: stats.into(),
        nodes: Some(shown),
    }))
}
