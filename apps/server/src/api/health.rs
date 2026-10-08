use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::networks::LiveNetwork;
use crate::{
    models::{Block, Network},
    state::AppState,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(read))
}

/// What one indexed network's synchronization is doing.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum IndexState {
    /// Inspection paused after its current bounded pass completed.
    Paused,
    /// Behind the node's tip and still applying blocks.
    CatchingUp,
    /// The checkpoint is at the tip the last pass observed.
    Ready,
    /// The last pass failed and will be attempted again.
    Retrying,
    /// The last pass failed terminally; this network is not indexed again
    /// until an operator clears it. Other networks are unaffected.
    Quarantined,
}

impl From<indexing::Condition> for IndexState {
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

/// Why one indexed network is failing: its forward synchronization, or the
/// historical range it still owes.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct IndexFailure {
    /// Failure category, for example `invalid_block` or `source`.
    pub(super) kind: String,
    /// Whether the same pass can succeed later.
    pub(super) retryable: bool,
    /// Consecutive attempts that got nowhere: failed passes, and passes whose
    /// backfill was refused.
    pub(super) attempts: u32,
    /// Unix seconds at which this network entered its current state.
    pub(super) since: u64,
    /// Human-readable reason; never contains credentials.
    pub(super) message: String,
}

impl IndexFailure {
    /// Why `indexed`'s forward synchronization is failing, when it is.
    fn of(indexed: &indexing::Health) -> Option<Self> {
        Self::reported(indexed, indexed.failure.as_ref())
    }

    /// One failure of `indexed`, with the network's own attempt count and the
    /// time it entered its state: both are facts about the network, and a
    /// backfill failure is counted into exactly the same attempts.
    fn reported(
        indexed: &indexing::Health,
        failure: Option<&indexing::IndexError>,
    ) -> Option<Self> {
        failure.map(|failure| Self {
            kind: Self::kind(failure.kind),
            retryable: failure.retryable,
            attempts: indexed.attempts,
            since: indexed.since,
            message: failure.message.clone(),
        })
    }

    /// `IndexErrorKind` is a closed domain enum without a wire encoding of its
    /// own, so the boundary names its variants the way every other enum here
    /// is named: `InvalidBlock` becomes `invalid_block`.
    fn kind(kind: indexing::IndexErrorKind) -> String {
        let variant = format!("{kind:?}");
        let mut name = String::with_capacity(variant.len() + 2);
        for (position, character) in variant.char_indices() {
            if character.is_ascii_uppercase() && position > 0 {
                name.push('_');
            }
            name.push(character.to_ascii_lowercase());
        }
        name
    }
}

// Missing historical ranges are reported without blocking forward readiness.
/// The history one network still owes addresses watched from below its
/// checkpoint, and why the last attempt at it stopped.
///
/// It is reported and never gating: a range a pruned node can no longer serve
/// would otherwise hold `ready` at false for the life of the process while
/// forward indexing answered every question a client actually asks. The range
/// is published here, and the reads it weakens say so themselves through their
/// own `behind`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct Backfill {
    /// How many watched addresses still owe history below their registration.
    pub(super) pending: u64,
    /// The lowest native block position any of them still needs inspected.
    pub(super) resume: u64,
    /// The highest native block position any of them needs: the network's
    /// confirmed line, which is where an address reading its own past rejoins
    /// the front of the chain. It follows the line until that happens.
    pub(super) anchor: u64,
    /// Why the last attempt stopped short of its budget; null while the range
    /// is merely unfinished. A range that keeps reporting the same failure is
    /// one this node cannot serve: history below `resume` for those addresses
    /// will not arrive until the node can. Always present.
    #[schema(required)]
    pub(super) failure: Option<IndexFailure>,
}

impl Backfill {
    /// What `indexed` still owes, when it owes anything.
    fn of(indexed: &indexing::Health) -> Option<Self> {
        let status = indexed.status.backfill.as_ref()?;
        Some(Self {
            pending: u64::try_from(status.pending).unwrap_or(u64::MAX),
            resume: status.resume.0,
            anchor: status.anchor.0,
            failure: IndexFailure::reported(indexed, status.failure.as_ref()),
        })
    }
}

/// The persisted checkpoint and indexing state of one network.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct NetworkCheckpoint {
    /// The indexed chain and network.
    pub(super) network: Network,
    /// The last committed block: this network's confirmed line, and the point
    /// `transactions` and `amount` are answered at. Null before the first one.
    /// Always present.
    #[schema(required)]
    pub(super) checkpoint: Option<Block>,
    /// The newest block held above the confirmed line; null when nothing is.
    /// Read beside `checkpoint` it shows that a checkpoint below the node's
    /// tip is the chain's confirmation depth rather than lag. Always present.
    #[schema(required)]
    pub(super) pending_block: Option<Block>,
    /// What this network's synchronization is doing.
    pub(super) state: IndexState,
    /// Why forward synchronization is held back. Present exactly when `state`
    /// is `retrying` or `quarantined`. Always present as a field.
    #[schema(required)]
    pub(super) failure: Option<IndexFailure>,
    /// The historical range this network still owes, and why it is stuck;
    /// null when nothing is owed. A network can be `ready` and still report
    /// one: the tip is indexed, and only the addresses named by that range
    /// have history still arriving. Always present.
    #[schema(required)]
    pub(super) backfill: Option<Backfill>,
}

/// The memory-only health snapshot of this process.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = Report)]
struct ReadResponse {
    /// Every network settled and at least one ready; false if reporting stopped.
    ready: bool,
    /// Latest notification for each network; no repository or node calls.
    networks: Vec<NetworkCheckpoint>,
    /// One set of counters per configured RPC endpoint.
    rpc: Vec<Rpc>,
    /// One send queue per asset that sends, in asset tag order.
    queues: Vec<Queue>,
}

/// One asset's in-memory send queue, read without touching its dispatch.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct Queue {
    /// The configured asset tag whose sends wait in this queue.
    asset: String,
    /// Sends admitted and not yet answered.
    depth: usize,
    /// Sends with the node right now.
    in_flight: usize,
    /// Sends answered since the process started, success or failure.
    completed: u64,
}

impl Queue {
    fn new(asset: &crate::asset::AssetTag, stats: wallets::Stats) -> Self {
        Self {
            asset: asset.to_string(),
            depth: stats.depth,
            in_flight: stats.in_flight,
            completed: stats.completed,
        }
    }
}

/// One RPC endpoint's sanitized execution counters.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct Rpc {
    /// Chain and network this client serves.
    network: Network,
    /// Zero-based ordinal of the node in the network's current list
    /// (`GET /v1/networks`).
    endpoint: usize,
    /// HTTP executions, including failed attempts hidden by transport retries.
    attempts: u64,
    /// Transport failures or JSON-RPC errors.
    failures: u64,
    /// Failed completions since the last successful completion.
    consecutive_failures: u64,
    /// Sanitized failure category; never RPC response text or endpoint credentials.
    last_failure: Option<String>,
}

impl Rpc {
    /// The counters of the nodes each network uses now, so they follow a
    /// replacement.
    fn current(networks: &[Arc<LiveNetwork>]) -> Vec<Self> {
        networks
            .iter()
            .flat_map(|configured| {
                let (nodes, statistics) = configured.current();
                let counted = statistics.into_iter().take(nodes.endpoints().len());
                counted
                    .enumerate()
                    .map(|(ordinal, statistics)| {
                        Self::new(configured.network(), ordinal, &statistics)
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn new(network: &base::Network, endpoint: usize, statistics: &json_rpc::Statistics) -> Self {
        let snapshot = statistics.snapshot();
        Self {
            network: network.clone().into(),
            endpoint,
            attempts: snapshot.attempts,
            failures: snapshot.failures,
            consecutive_failures: snapshot.consecutive_failures,
            last_failure: snapshot.last_failure,
        }
    }
}

#[utoipa::path(
    get,
    path = "/health",
    operation_id = "read_health",
    summary = "Read readiness and network health from memory",
    description = "Public memory-only readiness. Ready when every network is ready or \
        quarantined and at least one is ready. No RPC or storage reads.",
    responses(
        (status = 200, description = "Ready", body = ReadResponse),
        (status = 503, description = "Not ready", body = ReadResponse)
    ),
    tag = "health"
)]
async fn read(State(state): State<Arc<AppState>>) -> (StatusCode, Json<ReadResponse>) {
    let reporting = state.health.has_changed().is_ok();
    let health = state.health.borrow();
    let ready = reporting && health.ready();
    let networks = health
        .networks()
        .iter()
        .map(|indexed| NetworkCheckpoint {
            network: indexed.status.network.clone().into(),
            checkpoint: indexed.status.checkpoint.as_ref().map(Into::into),
            pending_block: indexed.status.pending_tip.as_ref().map(Into::into),
            state: indexed.state.into(),
            failure: IndexFailure::of(indexed),
            backfill: Backfill::of(indexed),
        })
        .collect();
    let rpc = Rpc::current(&state.networks);
    let queues = state
        .wallets
        .queues()
        .iter()
        .map(|(asset, stats)| Queue::new(asset, *stats))
        .collect();
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(ReadResponse {
            ready,
            networks,
            rpc,
            queues,
        }),
    )
}

#[cfg(test)]
mod tests {
    use indexing::{BlockPosition, Condition, IndexError, IndexErrorKind, SyncHealth};
    use serde_json::{Value, json};

    use super::*;
    use crate::api::emits_required;
    use crate::app::App;

    #[test]
    fn status_rows_require_exactly_what_they_emit() {
        emits_required(
            "NetworkCheckpoint",
            NetworkCheckpoint {
                network: Network {
                    chain: "fixture".to_owned(),
                    name: "regtest".to_owned(),
                },
                checkpoint: None,
                pending_block: None,
                state: IndexState::Quarantined,
                failure: Some(IndexFailure {
                    kind: "invalid_block".to_owned(),
                    retryable: false,
                    attempts: 1,
                    since: 1_758_614_400,
                    message: "block contains a duplicate transaction".to_owned(),
                }),
                backfill: None,
            },
        );
        emits_required(
            "Queue",
            Queue {
                asset: "btc".to_owned(),
                depth: 1,
                in_flight: 1,
                completed: 0,
            },
        );
        emits_required(
            "Backfill",
            Backfill {
                pending: 2,
                resume: 842_000,
                anchor: 845_112,
                failure: None,
            },
        );
    }

    #[tokio::test]
    async fn readiness_follows_the_published_sync_health() {
        let app = App::catching_up().await;

        assert_eq!(
            app.call("GET", "/health", None, false).await.status,
            StatusCode::SERVICE_UNAVAILABLE
        );

        app.publish(App::health(Condition::Ready));

        assert_eq!(
            app.call("GET", "/health", None, false).await.status,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn one_quarantined_network_keeps_the_others_ready_and_reports_its_failure() {
        let app = App::ready().await;
        let mut quarantined = App::health(Condition::Ready);
        quarantined.apply(&App::reported(&App::solana(), Condition::Quarantined));
        app.publish(quarantined);

        assert_eq!(
            app.call("GET", "/health", None, false).await.status,
            StatusCode::OK,
            "a quarantined network must not hold readiness at 503 forever"
        );
        let status = app.call("GET", "/health", None, true).await.json();
        assert_eq!(status["ready"], true);
        let networks = status["networks"].as_array().expect("networks");
        assert_eq!(networks[0]["state"], "ready");
        assert_eq!(networks[0]["failure"], Value::Null);
        assert_eq!(networks[1]["network"]["chain"], "solana");
        assert_eq!(networks[1]["state"], "quarantined");
        assert_eq!(
            networks[1]["failure"],
            json!({
                "kind": "invalid_block",
                "retryable": false,
                "attempts": 1,
                "since": 1_758_614_400_u64,
                "message": "block contains a duplicate transaction",
            }),
            "a quarantined network still reports its frozen checkpoint and why"
        );
        assert_eq!(networks[1]["checkpoint"]["height"], 8);

        app.publish(App::health(Condition::Quarantined));
        assert_eq!(
            app.call("GET", "/health", None, false).await.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "every network quarantined serves nothing useful"
        );
    }

    #[tokio::test]
    async fn closed_readiness_is_unavailable_even_when_the_last_value_was_true() {
        for last_value in [false, true] {
            let mut app = if last_value {
                App::ready().await
            } else {
                App::catching_up().await
            };
            app.stop_publishing();

            let response = app.call("GET", "/health", None, false).await;
            assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.json()["ready"], false);
        }
    }

    #[tokio::test]
    async fn status_reports_readiness_and_every_indexed_checkpoint() {
        let app = App::catching_up().await;
        let response = app.call("GET", "/health", None, true).await;
        assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
        let status = response.json();
        assert_eq!(status["ready"], false);
        let networks = status["networks"].as_array().expect("networks");
        assert_eq!(networks.len(), 3);
        assert_eq!(networks[0]["network"]["chain"], "bitcoin");
        assert_eq!(networks[0]["checkpoint"]["height"], 8);
        assert_eq!(networks[0]["state"], "catching_up");
        assert_eq!(networks[0]["failure"], Value::Null);
        app.publish(App::health(Condition::Ready));
        let status = app.call("GET", "/health", None, true).await.json();
        assert_eq!(status["ready"], true);
    }

    /// The availability floor at the HTTP edge: a range this node can never serve
    /// is published on the status row and does not make the API unready.
    #[tokio::test]
    async fn a_network_that_owes_an_unservable_range_is_still_ready_and_says_so() {
        let app = App::catching_up().await;
        app.publish(stalled());

        let status = app.call("GET", "/health", None, true).await.json();

        assert_eq!(
            status["ready"], true,
            "forward indexing reached every tip, so the API answers"
        );
        let stuck = &status["networks"][0];
        assert_eq!(stuck["state"], "ready");
        assert_eq!(stuck["failure"], Value::Null, "nothing holds the tip back");
        assert_eq!(stuck["backfill"]["pending"], 1);
        assert_eq!(stuck["backfill"]["resume"], 2);
        assert_eq!(stuck["backfill"]["anchor"], 8);
        assert_eq!(stuck["backfill"]["failure"]["kind"], "source");
        assert_eq!(
            stuck["backfill"]["failure"]["message"], "block is not available on this node",
            "an operator reads the stuck range here rather than inferring it from a 503"
        );
        assert_eq!(
            status["networks"][1]["backfill"],
            Value::Null,
            "a network that owes nothing says so"
        );

        let probe = app.call("GET", "/health", None, false).await;
        assert_eq!(probe.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn health_reads_rpc_counters_after_the_endpoint_has_stopped() {
        use json_rpc::Client as _;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local node");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let node = axum::Router::new().route(
            "/",
            axum::routing::post(|axum::Json(request): axum::Json<Value>| async move {
                axum::Json(json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {"code": -32001, "message": "private-provider-detail"}
                }))
            }),
        );
        let serving = tokio::spawn(async move {
            axum::serve(listener, node).await.expect("local response");
        });
        let client = json_rpc::Endpoint::new(&url).connect().expect("client");
        let statistics = client.statistics();
        assert!(
            client
                .request(json_rpc::Request::once("tip", json!([])))
                .await
                .expect("envelope")
                .is_err()
        );
        serving.abort();
        let _ = serving.await;
        let app = App::ready()
            .await
            .with_rpc(App::bitcoin(), statistics.clone());
        let response = app.call("GET", "/health", None, false).await;
        assert_eq!(response.status, StatusCode::OK);
        let body = response.json();
        assert_eq!(body["rpc"][0]["attempts"], 1);
        assert_eq!(body["rpc"][0]["failures"], 1);
        assert_eq!(body["rpc"][0]["consecutive_failures"], 1);
        assert_eq!(statistics.snapshot().attempts, 1, "health did not call RPC");
        let wire = body.to_string();
        assert!(!wire.contains("private-provider-detail"));
        assert!(!wire.contains(&url));
    }

    /// Every asset's send queue is on the public report, idle before a send
    /// and counting it once answered.
    #[tokio::test]
    async fn health_reports_every_assets_send_queue() {
        let app = App::ready().await;
        let idle = |asset: &str, completed: u64| {
            json!({
                "asset": asset,
                "depth": 0,
                "in_flight": 0,
                "completed": completed
            })
        };

        let before = app.call("GET", "/health", None, false).await.json();
        assert_eq!(
            before["queues"],
            json!([idle("btc", 0), idle("sol", 0), idle("usdc", 0)])
        );

        let id = app.wallet_id().await;
        let sent = app
            .call(
                "POST",
                &format!("/v1/wallets/{id}/transactions"),
                Some(json!({"destination": "addr-destination", "amount": "1"})),
                true,
            )
            .await;
        assert_eq!(sent.status, StatusCode::ACCEPTED);

        let after = app.call("GET", "/health", None, false).await.json();
        assert_eq!(
            after["queues"],
            json!([idle("btc", 1), idle("sol", 0), idle("usdc", 0)]),
            "the answered send is counted on its own asset's queue"
        );
    }

    /// Every network ready, with the first one owing a range its node
    /// will never serve.
    fn stalled() -> SyncHealth {
        let networks = [App::bitcoin(), App::solana(), App::ethereum()];
        let mut health = SyncHealth::starting(&networks);
        for each in &networks {
            let mut row = App::reported(each, Condition::Ready);
            if *each == App::bitcoin() {
                row.status.backfill = Some(indexing::Backlog {
                    pending: 1,
                    resume: BlockPosition(2),
                    anchor: BlockPosition(8),
                    inspected: 0,
                    failure: Some(IndexError::new(
                        IndexErrorKind::Source,
                        "block is not available on this node",
                        true,
                    )),
                });
            }
            health.apply(&row);
        }
        health
    }
}
