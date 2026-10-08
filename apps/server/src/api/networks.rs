//! `/v1/networks`: every configured network and its RPC nodes, replacing
//! those nodes while the server runs, and rescanning one network.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use payments::{NetworkError, NetworkState};

use super::{Error, ErrorBody};
use crate::{
    models::{Endpoint, Network, RpcEndpoints},
    state::AppState,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(replace))
        .routes(routes!(rescan))
}

/// What a configured network is doing.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum ServiceState {
    /// Its node failed verification at startup, so it is neither indexed nor
    /// sending. Its nodes can still be replaced; they are used from the next
    /// start.
    Unavailable,
    /// Its indexed facts are being purged; wallet creation and sends on it
    /// answer 409 until indexing resumes.
    Rescanning,
    /// Indexing paused after its current bounded pass completed.
    Paused,
    /// Behind the node's tip and still applying blocks; also where a rescan
    /// leaves the network.
    CatchingUp,
    /// The checkpoint is at the tip the last pass observed.
    Ready,
    /// The last pass failed and will be attempted again.
    Retrying,
    /// The last pass failed terminally; a rescan or a restart clears it.
    Quarantined,
}

impl From<indexing::Condition> for ServiceState {
    fn from(value: indexing::Condition) -> Self {
        match value {
            indexing::Condition::Paused => Self::Paused,
            indexing::Condition::CatchingUp => Self::CatchingUp,
            indexing::Condition::Ready => Self::Ready,
            indexing::Condition::Retrying => Self::Retrying,
            indexing::Condition::Quarantined => Self::Quarantined,
        }
    }
}

/// One RPC node as it may be shown.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct RpcNode {
    /// Scheme, host and port of the node's URL. User information, path and
    /// query are never returned: any of them may hold a
    /// credential.
    origin: String,
}

/// One configured network, its state, and its nodes.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct ConfiguredNetwork {
    network: Network,
    state: ServiceState,
    /// Nodes in failover order; ordinal `i` is `rpc[i]` of `GET /health`.
    nodes: Vec<RpcNode>,
}

impl From<NetworkState> for ConfiguredNetwork {
    /// `configured` now: unavailable when nothing of it was composed,
    /// rescanning while its wallets are paused, and otherwise its indexing
    /// state read from the published health.
    fn from(configured: NetworkState) -> Self {
        let state = if !configured.served {
            ServiceState::Unavailable
        } else if configured.rescanning {
            ServiceState::Rescanning
        } else {
            configured
                .condition
                .map_or(ServiceState::CatchingUp, Into::into)
        };
        Self {
            network: configured.network.into(),
            state,
            nodes: configured
                .endpoints
                .iter()
                .map(|endpoint| RpcNode {
                    origin: Endpoint::new(Value::String(endpoint.url.clone()))
                        .map_or_else(|_| "unresolved".to_owned(), |node| node.origin().to_owned()),
                })
                .collect(),
        }
    }
}

/// `GET /v1/networks`: every configured network.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = NetworkList)]
struct ListResponse {
    /// Networks in configuration order.
    networks: Vec<ConfiguredNetwork>,
}

/// One RPC node, written as the configuration file's `rpc.endpoints` writes
/// it. The URL carries any credentials: userinfo (HTTP Basic) or query keys.
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

impl From<NetworkPath> for base::Network {
    fn from(path: NetworkPath) -> Self {
        Self::new(base::ChainName::new(path.chain), path.network)
    }
}

impl From<NetworkError> for Error {
    fn from(error: NetworkError) -> Self {
        match error {
            NetworkError::NotConfigured => Self::not_found("network is not configured"),
            NetworkError::Busy => Self::conflict("a change to this network is in progress"),
            NetworkError::NotInService => Self::not_in_service(),
            NetworkError::Invalid(message) => Self::invalid_request(message),
            NetworkError::Unverified(message) => Self::unprocessable(format!(
                "the node failed verification and the current nodes stay in use: {message}"
            )),
            NetworkError::Rescan(error) => error.into(),
            NetworkError::Index(error) => error.into(),
            error @ (NetworkError::NotStored(_) | NetworkError::Task(_)) => {
                Self::indexed(error.to_string())
            }
        }
    }
}

impl From<indexing::RescanError> for Error {
    /// Whether the network is left paused is the operator's next decision.
    fn from(error: indexing::RescanError) -> Self {
        Self::indexed(match error {
            indexing::RescanError::NotStarted(error) => {
                format!("rescan did not start: {}", error.message)
            }
            indexing::RescanError::Interrupted(error) => format!(
                "rescan stopped after the purge and the network is paused; \
                 rescan it again: {}",
                error.message
            ),
        })
    }
}

#[utoipa::path(
    get,
    path = "/v1/networks",
    operation_id = "list_networks",
    summary = "List every configured network and its RPC nodes",
    description = "Every network the configuration names, its state, and the nodes it uses \
        now. Nodes come from the database once a network has stored them; the configuration \
        file only seeds that list on the first start. Only each node's origin is returned.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Networks in configuration order", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn list(State(state): State<Arc<AppState>>) -> Json<ListResponse> {
    let networks = state
        .payments
        .networks
        .list()
        .into_iter()
        .map(Into::into)
        .collect();
    Json(ListResponse { networks })
}

#[utoipa::path(
    put,
    path = "/v1/networks/{chain}/{network}/rpc",
    operation_id = "replace_network_rpc",
    summary = "Replace a network's RPC nodes",
    description = "Verifies the nodes with the chain's own startup check over the new list, \
        stores them, then sends every later call to them; calls already sent finish on the \
        node they reached. A node that fails the check leaves the current nodes in use and \
        stores nothing. The stored list is used on every later start, whatever the \
        configuration file says. Credentials in the URL are stored as sent. \
        A network that is unavailable stores the verified nodes and uses them from the next \
        start.",
    params(NetworkPath),
    request_body = ReplaceRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Nodes verified, stored and in use", body = ConfiguredNetwork),
        (status = 400, description = "Malformed node list or a node the chain refuses",
            body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network is not configured", body = ErrorBody),
        (status = 422, description = "The node was unreachable or serves another network; the \
            message says which, and the current nodes stay in use", body = ErrorBody),
        (status = 503, description = "Another change to this network is in progress, or the \
            nodes could not be stored", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn replace(
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
    request: Result<Json<ReplaceRequest>, JsonRejection>,
) -> Result<Json<ConfiguredNetwork>, Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let network = base::Network::from(path);
    state.payments.networks.get(&network)?;
    let documents = request.endpoints.into_iter().map(Value::from).collect();
    let nodes = RpcEndpoints::new(documents).map_err(Error::invalid_request)?;
    // Stored before it is installed: payments runs both in a task of its own.
    let (pool, stored) = (state.pool.clone(), network.clone());
    let candidates = nodes.nodes();
    let persist = async move {
        RpcEndpoints::upsert(&pool, &stored, &nodes)
            .await
            .map_err(|error| error.to_string())
    };
    let replaced = state
        .payments
        .networks
        .replace(&network, candidates, persist)
        .await
        .map_err(|error| match error {
            NetworkError::Busy => Error::indexed("another change to this network is in progress"),
            error => error.into(),
        })?;
    Ok(Json(replaced.into()))
}

#[utoipa::path(
    post,
    path = "/v1/networks/{chain}/{network}/rescans",
    operation_id = "rescan_network",
    summary = "Purge a network's indexed facts and index it again",
    description = "One action: refuses new wallets and sends on the network and waits for \
        those running, pauses its indexing, deletes its indexed history, balances, outputs \
        and checkpoint, and resumes indexing from every watch's birthday. Answers once indexing \
        has resumed; the network then reports `catching_up` until it reaches the tip. Watches, \
        wallet keys and every other network are untouched. A failure after the deletion leaves \
        the network paused; rescanning again finishes it.",
    params(NetworkPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 202, description = "Purged; indexing has resumed from the birthdays",
            body = ConfiguredNetwork),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network is not configured", body = ErrorBody),
        (status = 409, description = "A rescan or node change of this network is in progress, \
            or the network is not in service", body = ErrorBody),
        (status = 503, description = "The rescan failed; the message says whether the network \
            is paused", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn rescan(
    State(state): State<Arc<AppState>>,
    Path(path): Path<NetworkPath>,
) -> Result<(StatusCode, Json<ConfiguredNetwork>), Error> {
    let network = base::Network::from(path);
    state.payments.networks.rescan(&network).await?;
    let rescanned = state.payments.networks.get(&network)?;
    Ok((StatusCode::ACCEPTED, Json(rescanned.into())))
}
