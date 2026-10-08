//! `/v1/networks`: every configured network and its RPC nodes, replacing
//! those nodes while the server runs, and rescanning one network.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use base::BoxFuture;
use json_rpc::{Client, Statistics, Swap};

use super::{Error, ErrorBody};
use crate::{
    models::{Network, RpcEndpoints},
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

impl ConfiguredNetwork {
    /// `configured` now: unavailable when nothing of it was composed,
    /// rescanning while its wallets are paused, and otherwise its indexing
    /// state read from the published health.
    fn new(configured: &LiveNetwork, state: &AppState) -> Self {
        let network = configured.network();
        let service = if !configured.served() {
            ServiceState::Unavailable
        } else if state.wallets.paused(network) {
            ServiceState::Rescanning
        } else {
            state
                .health
                .borrow()
                .networks()
                .iter()
                .find(|row| row.status.network == *network)
                .map_or(ServiceState::CatchingUp, |row| row.state.into())
        };
        let (nodes, _) = configured.current();
        Self {
            network: network.clone().into(),
            state: service,
            nodes: nodes
                .endpoints()
                .iter()
                .map(|endpoint| RpcNode {
                    origin: endpoint.origin().to_owned(),
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

impl From<Rejected> for Error {
    fn from(rejected: Rejected) -> Self {
        match rejected {
            Rejected::Invalid(message) => Self::invalid_request(message),
            Rejected::Unverified(message) => Self::unprocessable(format!(
                "the node failed verification and the current nodes stay in use: {message}"
            )),
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
        .networks
        .iter()
        .map(|configured| ConfiguredNetwork::new(configured, &state))
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
    let configured = LiveNetwork::find(&state.networks, &network)?.clone();
    let documents = request.endpoints.into_iter().map(Value::from).collect();
    let nodes = RpcEndpoints::new(documents).map_err(Error::invalid_request)?;
    let _held = configured
        .maintain()
        .ok_or_else(|| Error::indexed("another change to this network is in progress"))?;
    let verified = (state.verify)(network.clone(), nodes.clone()).await?;
    // Stored, then installed, in a task of its own: a caller that hangs up
    // cannot leave the stored list and the live one apart.
    let (pool, installed) = (state.pool.clone(), configured.clone());
    tokio::spawn(async move {
        let written = RpcEndpoints::upsert(&pool, &network, &nodes).await;
        if written.is_ok() {
            installed.install(nodes, verified);
        }
        written
    })
    .await
    .map_err(|error| Error::indexed(error.to_string()))?
    .map_err(|error| Error::indexed(format!("the nodes were not stored: {error}")))?;
    Ok(Json(ConfiguredNetwork::new(&configured, &state)))
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
    let configured = LiveNetwork::find(&state.networks, &network)?;
    if !configured.served() {
        return Err(Error::not_in_service());
    }
    let held = configured
        .maintain()
        .ok_or_else(|| Error::conflict("a change to this network is in progress"))?;
    let indexer = state
        .index
        .units()
        .iter()
        .find(|unit| *unit.network() == network)
        .map(indexing::Route::indexer)
        .ok_or_else(Error::not_in_service)?;
    let (wallets, control) = (state.wallets.clone(), state.control.clone());
    let index_store = state.index_store.clone();
    // Its own task: the window must close and reopen whole even if the caller
    // hangs up. New wallets and sends are refused (409) and the running ones
    // drain before indexing pauses; they are admitted again once it resumed.
    tokio::spawn(async move {
        let _held = held;
        let paused = wallets.pause(indexer.network());
        paused.drained().await;
        control.rescan(indexer.as_ref(), index_store.as_ref()).await
    })
    .await
    .map_err(|error| Error::indexed(error.to_string()))??;
    Ok((
        StatusCode::ACCEPTED,
        Json(ConfiguredNetwork::new(configured, &state)),
    ))
}

/// One configured network.
///
/// Its nodes and its transport's counters change together ([`Self::install`])
/// and are read together ([`Self::current`]), so ordinal `i` of the nodes is
/// always counter `i` of the transport.
pub(crate) struct LiveNetwork {
    network: base::Network,
    /// What the composed chain calls through; `None` for a network whose node
    /// failed verification at startup, so nothing of it was composed.
    transport: Option<Arc<Swap>>,
    nodes: Mutex<RpcEndpoints>,
    /// Held by the one node replacement or rescan running on this network.
    maintenance: Arc<tokio::sync::Mutex<()>>,
}

/// Checks candidate nodes as `network`'s with its chain's own startup check,
/// and answers the transport they build with each node's counters. Built by
/// the composition root, which alone may name the chains.
pub(crate) type Verifier = Arc<
    dyn Fn(base::Network, RpcEndpoints) -> BoxFuture<'static, Result<Verified, Rejected>>
        + Send
        + Sync,
>;

/// A verified candidate's transport and its nodes' counters, in order.
pub(crate) type Verified = (Arc<dyn Client>, Vec<Statistics>);

/// Why a candidate node list was not verified.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rejected {
    /// The chain refuses the section with these nodes, or they build no
    /// transport (an unusable URL, say).
    Invalid(String),
    /// The node did not prove it serves this network.
    Unverified(String),
}

impl LiveNetwork {
    pub(crate) fn new(
        network: base::Network,
        transport: Option<Arc<Swap>>,
        nodes: RpcEndpoints,
    ) -> Self {
        Self {
            network,
            transport,
            nodes: Mutex::new(nodes),
            maintenance: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// The configured network `network` among `networks`.
    ///
    /// # Errors
    ///
    /// A 404 when the configuration names no such network.
    pub(crate) fn find<'a>(
        networks: &'a [Arc<Self>],
        network: &base::Network,
    ) -> Result<&'a Arc<Self>, Error> {
        networks
            .iter()
            .find(|configured| configured.network == *network)
            .ok_or_else(|| Error::not_found("network is not configured"))
    }

    pub(crate) const fn network(&self) -> &base::Network {
        &self.network
    }

    /// Whether this process composed the network: indexes it and sends on it.
    pub(crate) const fn served(&self) -> bool {
        self.transport.is_some()
    }

    /// The nodes in failover order, and each one's counters while served.
    pub(crate) fn current(&self) -> (RpcEndpoints, Vec<Statistics>) {
        let nodes = self.nodes();
        let statistics = self
            .transport
            .as_ref()
            .map(|transport| transport.statistics())
            .unwrap_or_default();
        (nodes.clone(), statistics)
    }

    /// Later calls reach `transport` and the listing follows; a network not
    /// served only records `nodes`, used from the next start.
    pub(crate) fn install(&self, nodes: RpcEndpoints, (transport, statistics): Verified) {
        let mut current = self.nodes();
        if let Some(swap) = &self.transport {
            swap.replace(transport, statistics);
        }
        *current = nodes;
    }

    /// The one change allowed on this network now, or `None` while another
    /// runs. Dropping it allows the next.
    pub(crate) fn maintain(&self) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        self.maintenance.clone().try_lock_owned().ok()
    }

    // Every critical section is one read or one replacement, so a panic
    // cannot leave the list torn: recovering the guard is sound.
    fn nodes(&self) -> MutexGuard<'_, RpcEndpoints> {
        self.nodes.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};

    use crate::app::App;

    const BITCOIN: &str = "/v1/networks/bitcoin/regtest";

    fn origins(network: &Value) -> Vec<String> {
        network["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .map(|node| node["origin"].as_str().expect("an origin").to_owned())
            .collect()
    }

    #[tokio::test]
    async fn every_network_is_listed_with_its_state_and_node_origins() {
        let app = App::ready().await.unavailable(&App::solana());
        let _rescanning = app.rescanning(&App::ethereum());
        let response = app.call("GET", "/v1/networks", None, true).await;
        assert_eq!(response.status, StatusCode::OK);
        let networks = response.json()["networks"].clone();
        let states: Vec<(String, String)> = networks
            .as_array()
            .expect("networks")
            .iter()
            .map(|row| {
                (
                    format!("{}/{}", row["network"]["chain"], row["network"]["name"]),
                    row["state"].as_str().expect("a state").to_owned(),
                )
            })
            .collect();
        assert_eq!(
            states,
            [
                (r#""bitcoin"/"regtest""#.to_owned(), "ready".to_owned()),
                (
                    r#""solana"/"localnet""#.to_owned(),
                    "unavailable".to_owned()
                ),
                (
                    r#""ethereum"/"mainnet""#.to_owned(),
                    "rescanning".to_owned()
                ),
            ]
        );
        assert_eq!(origins(&networks[0]), ["http://node.invalid:8545"]);
        assert_eq!(
            app.call("GET", "/v1/networks", None, false).await.status,
            StatusCode::UNAUTHORIZED
        );
    }

    /// What was sent may carry credentials; what is answered never does, and
    /// what is stored is the documents as sent.
    #[tokio::test]
    async fn replaced_nodes_are_answered_by_origin_only() {
        let app = App::ready().await;
        let body = json!({"endpoints": [
            "http://user:s3cret@new.invalid:9/wallet?apikey=s3cret",
            {"url": "https://other.invalid/v3/s3cret", "bounds": {"timeout_ms": 1000}},
        ]});
        let replaced = app
            .call("PUT", &format!("{BITCOIN}/rpc"), Some(body), true)
            .await;
        assert_eq!(replaced.status, StatusCode::OK);
        let expected = ["http://new.invalid:9", "https://other.invalid"];
        assert_eq!(origins(&replaced.json()), expected);
        let listed = app.call("GET", "/v1/networks", None, true).await;
        assert_eq!(origins(&listed.json()["networks"][0]), expected);
        for response in [&replaced, &listed] {
            let text = String::from_utf8_lossy(&response.body);
            assert!(!text.contains("s3cret"), "{text}");
        }
        assert_eq!(app.stored(&App::bitcoin()).await.len(), 2);
    }

    #[tokio::test]
    async fn a_replacement_is_refused_with_its_own_status() {
        let app = App::ready().await;
        let put = |path: &str, body: Value| {
            let path = path.to_owned();
            let app = &app;
            async move { app.call("PUT", &path, Some(body), true).await }
        };
        let rpc = format!("{BITCOIN}/rpc");
        for invalid in [
            json!({"endpoints": []}),
            json!({"endpoints": "http://one.invalid"}),
            json!({"endpoints": ["ws://one.invalid"]}),
            json!({"endpoints": ["http://one.invalid"], "repeat": "off"}),
            json!({"endpoints": [{"url": "http://one.invalid", "timeout_ms": 1}]}),
        ] {
            assert_eq!(put(&rpc, invalid).await.status, StatusCode::BAD_REQUEST);
        }
        let node = json!({"endpoints": ["http://one.invalid"]});
        let unknown = put("/v1/networks/bitcoin/mainnet/rpc", node.clone()).await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        let refused = put(
            &rpc,
            json!({"endpoints": ["http://user:s3cret@unverified.invalid"]}),
        )
        .await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        let message = refused.json()["message"]
            .as_str()
            .expect("a message")
            .to_owned();
        assert!(message.contains("current nodes stay in use"), "{message}");
        assert!(!message.contains("s3cret"));
        let rescanning = app.rescanning(&App::bitcoin());
        assert_eq!(
            put(&rpc, node).await.status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        drop(rescanning);
        let listed = app.call("GET", "/v1/networks", None, true).await.json();
        assert_eq!(
            origins(&listed["networks"][0]),
            ["http://node.invalid:8545"]
        );
        assert!(
            app.stored(&App::bitcoin()).await.is_empty(),
            "nothing stored"
        );
    }

    #[tokio::test]
    async fn a_rescan_is_accepted_once_and_refused_while_one_runs() {
        let app = App::ready().await.unavailable(&App::solana()).supervised();
        let rescans = format!("{BITCOIN}/rescans");
        let started = app.call("POST", &rescans, None, true).await;
        assert_eq!(started.status, StatusCode::ACCEPTED);
        assert_eq!(started.json()["network"]["name"], "regtest");
        assert_eq!(
            started.json()["state"],
            "ready",
            "admissions are open again"
        );
        assert_eq!(
            app.call("POST", "/v1/networks/bitcoin/mainnet/rescans", None, true)
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        let rescanning = app.rescanning(&App::bitcoin());
        let running = app.call("POST", &rescans, None, true).await;
        assert_eq!(running.status, StatusCode::CONFLICT);
        drop(rescanning);
        let unavailable = app
            .call("POST", "/v1/networks/solana/localnet/rescans", None, true)
            .await;
        assert_eq!(unavailable.status, StatusCode::CONFLICT);
        assert_eq!(
            app.call("POST", &rescans, None, false).await.status,
            StatusCode::UNAUTHORIZED
        );
    }

    /// The window a rescan closes: no wallet is registered and nothing is
    /// sent on that network, while every other network and every read go on.
    #[tokio::test]
    async fn a_closed_network_refuses_wallets_and_sends_but_not_reads() {
        let app = App::ready().await;
        let id = app.wallet_id().await;
        let paused = app.state.wallets.pause(&App::bitcoin());
        let created = app
            .call("POST", "/v1/wallets", Some(json!({"asset": "btc"})), true)
            .await;
        assert_eq!(created.status, StatusCode::CONFLICT);
        let transfer = json!({"destination": "addr-destination", "amount": "1"});
        let single = format!("/v1/wallets/{id}/transactions");
        let sent = app
            .call("POST", &single, Some(transfer.clone()), true)
            .await;
        assert_eq!(sent.status, StatusCode::CONFLICT);
        assert_eq!(
            sent.json()["message"],
            "the network is being rescanned; retry once it is indexing again"
        );
        let batch = json!({"transfers": [
            {"wallet_id": id, "destination": "addr-destination", "amount": "1"}
        ]});
        let batched = app
            .call("POST", "/v1/transactions", Some(batch), true)
            .await;
        assert_eq!(batched.status, StatusCode::CONFLICT);
        assert!(app.sent().is_empty());
        assert_eq!(
            app.call("GET", &single, None, true).await.status,
            StatusCode::OK
        );
        app.create("sol").await;
        drop(paused);
        let sent = app.call("POST", &single, Some(transfer), true).await;
        assert_eq!(sent.status, StatusCode::ACCEPTED);
    }
}
