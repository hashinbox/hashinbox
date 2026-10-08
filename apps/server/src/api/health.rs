use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

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
    fn current(networks: Vec<payments::NetworkState>) -> Vec<Self> {
        networks
            .into_iter()
            .flat_map(|configured| {
                let counted = configured
                    .statistics
                    .iter()
                    .take(configured.endpoints.len());
                counted
                    .enumerate()
                    .map(|(ordinal, statistics)| {
                        Self::new(&configured.network, ordinal, statistics)
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
    let receiver = state.payments.networks.health_receiver();
    let reporting = receiver.has_changed().is_ok();
    let health = receiver.borrow();
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
    let rpc = Rpc::current(state.payments.networks.list());
    let queues = state
        .payments
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
