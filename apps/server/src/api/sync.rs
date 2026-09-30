use indexing::Checkpoint as _;

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    Payments,
    contract::{Block, Network},
    error::{Error, ErrorBody},
};

pub(super) fn routes() -> OpenApiRouter<Payments> {
    OpenApiRouter::new()
        .routes(routes!(read))
        .routes(routes!(resume))
        .routes(routes!(cancel_backfill))
}

/// Clears one network's quarantine from outside the synchronizer.
///
/// A quarantine is the index reporting that it will not try again by itself;
/// this is the one decision an operator makes back, because its cause — the
/// node was upgraded, the store was repaired, the configuration was corrected
/// — is fixed out of band and arrives long after the failure that reported it
/// (`docs/design/019` section 5.3).
///
/// It is a port rather than the supervisor's own handle because the routes
/// name no async runtime and no supervisor, and the route contract is held
/// against a double of it. The composition root holds `indexing::SyncControl`
/// and implements this over it.
pub(crate) trait Resume: Send + Sync + 'static {
    /// Clears `network`'s quarantine: its attempt count resets and its next
    /// pass runs at once.
    ///
    /// Nothing durable changes and no outcome is promised. A network whose
    /// cause is still unfixed fails again on that pass, through the ordinary
    /// attempt and quarantine path, and says so on `GET /v1/status`.
    ///
    /// # Errors
    ///
    /// [`indexing::IndexErrorKind::NetworkMismatch`] when the network is not
    /// configured.
    fn resume(&self, network: &base::Network) -> Result<(), indexing::IndexError>;
}

/// What one indexed network's synchronization is doing.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum IndexState {
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

impl From<indexing::NetworkState> for IndexState {
    fn from(value: indexing::NetworkState) -> Self {
        match value {
            indexing::NetworkState::CatchingUp => Self::CatchingUp,
            indexing::NetworkState::Ready => Self::Ready,
            indexing::NetworkState::Retrying => Self::Retrying,
            indexing::NetworkState::Quarantined => Self::Quarantined,
        }
    }
}

/// Why one indexed network is failing: its forward synchronization, or the
/// historical range it still owes.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(super) struct IndexFailure {
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
    fn of(indexed: &indexing::NetworkHealth) -> Option<Self> {
        Self::reported(indexed, indexed.failure.as_ref())
    }

    /// One failure of `indexed`, with the network's own attempt count and the
    /// time it entered its state: both are facts about the network, and a
    /// backfill failure is counted into exactly the same attempts.
    fn reported(
        indexed: &indexing::NetworkHealth,
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

// The availability floor of `docs/design/024` section 7, answered by reporting
// rather than by withholding.
/// The history one network still owes addresses watched from below its
/// checkpoint, and why the last attempt at it stopped.
///
/// It is reported and never gating: a range a pruned node can no longer serve
/// would otherwise hold `ready` at false for the life of the process while
/// forward indexing answered every question a client actually asks. The range
/// is published here, and the reads it weakens say so themselves through their
/// own `completeness`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(super) struct Backfill {
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
    fn of(indexed: &indexing::NetworkHealth) -> Option<Self> {
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
pub(super) struct NetworkCheckpoint {
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

/// `GET /v1/status`: readiness and every indexed network's checkpoint.
// linter:disable rust/redundant-module-prefix -- Utoipa publishes the schema under this name.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct SyncReport {
    /// Whether the API answers for every chain's tip: every network is
    /// `ready` or `quarantined`, and at least one is `ready`
    /// (`GET /health/ready`). It says nothing about the historical ranges
    /// `networks[].backfill` reports; those are per address, and each read
    /// they weaken carries its own `completeness`.
    ready: bool,
    /// Networks in index order.
    networks: Vec<NetworkCheckpoint>,
}

#[utoipa::path(
    get,
    path = "/v1/status",
    operation_id = "read_sync_status",
    summary = "Read readiness and every indexed network's checkpoint and state",
    description = "Whether the API serves complete history, and for each configured network \
        the last block it committed, the newest block it holds above that confirmed line, what \
        its synchronization is doing, why it is failing, and what history it still owes \
        addresses watched from below its checkpoint. One network's failure does not stop the \
        others: a quarantined network keeps its last checkpoint and reports it here, and a \
        network whose historical range this node cannot serve keeps answering for its tip.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Readiness and one checkpoint per indexed network",
            body = SyncReport),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "A checkpoint could not be read", body = ErrorBody)
    ),
    tag = "status"
)]
async fn read(State(payments): State<Payments>) -> Result<Json<SyncReport>, Error> {
    let health = payments.health.borrow().clone();
    let mut networks = Vec::with_capacity(health.networks().len());
    for indexed in health.networks() {
        let checkpoint = payments
            .index
            .checkpoint(&indexed.status.network)
            .await
            .map_err(wallets::WalletError::from)?;
        networks.push(NetworkCheckpoint {
            network: indexed.status.network.clone().into(),
            checkpoint: checkpoint.map(Into::into),
            pending_block: indexed.status.pending_tip.as_ref().map(Into::into),
            state: indexed.state.into(),
            failure: IndexFailure::of(indexed),
            backfill: Backfill::of(indexed),
        });
    }
    Ok(Json(SyncReport {
        ready: payments.ready(),
        networks,
    }))
}

/// `POST /v1/status/resume` body: the one network to take out of quarantine.
///
/// It names a network rather than resuming everything, because "the cause was
/// fixed" is a fact about one chain: a blanket resume would re-attempt chains
/// nobody looked at and hide which one the operator actually repaired.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct ResumeNetwork {
    /// The quarantined chain and network, exactly as `GET /v1/status` names it.
    network: Network,
}

/// What `POST /v1/status/resume` accepted: the network it resumed and the
/// failure that quarantine was holding.
///
/// `cleared` is what `GET /v1/status` reported for this network immediately
/// before, not a claim that the cause is gone. It is `202` and not `200` for
/// that reason: the next pass decides, and the same route's `GET` is where
/// the answer appears.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(super) struct ResumeAccepted {
    /// The network whose quarantine this cleared.
    pub(super) network: Network,
    /// The failure the quarantine was holding; null when it recorded none.
    /// Always present.
    #[schema(required)]
    pub(super) cleared: Option<IndexFailure>,
}

#[utoipa::path(
    post,
    path = "/v1/status/resume",
    operation_id = "resume_sync_network",
    summary = "Clear one network's quarantine and index it again",
    description = "Clears the quarantine of one network named exactly as `GET /v1/status` \
        names it: its attempt count resets and its next pass runs at once. Nothing durable \
        changes, and no outcome is promised — a network whose cause is still unfixed fails \
        again on that pass, through the ordinary attempt and quarantine path, and `GET \
        /v1/status` reports it. Other networks are untouched. A network that is not \
        quarantined answers 409 rather than pretending to act.",
    request_body = ResumeNetwork,
    security(("bearerAuth" = [])),
    responses(
        (status = 202, description = "The quarantine was cleared; the next pass decides",
            body = ResumeAccepted),
        (status = 400, description = "Malformed body", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "The network is not configured", body = ErrorBody),
        (status = 409, description = "The network is not quarantined", body = ErrorBody)
    ),
    tag = "status"
)]
async fn resume(
    State(payments): State<Payments>,
    request: Result<Json<ResumeNetwork>, JsonRejection>,
) -> Result<(StatusCode, Json<ResumeAccepted>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let health = payments.health.borrow().clone();
    let indexed = health
        .networks()
        .iter()
        .find(|indexed| Network::from(indexed.status.network.clone()) == request.network)
        .ok_or_else(|| Error::not_found("index network is not configured"))?;
    if indexed.state != indexing::NetworkState::Quarantined {
        return Err(Error::conflict(
            "index network is not quarantined; nothing to resume",
        ));
    }
    let cleared = IndexFailure::of(indexed);
    payments.resume.resume(&indexed.status.network)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ResumeAccepted {
            network: indexed.status.network.clone().into(),
            cleared,
        }),
    ))
}

/// `POST /v1/status/backfill/cancel` body: the one network to stop asking.
///
/// It names a network for the same reason the resume body does, and for one
/// more: what an operator can see is a network aggregate. `networks[].backfill`
/// reports one `resume`, one `anchor` and one count for the whole network and
/// names no address, so an address-by-address cancellation would be a request
/// this API gives nobody the information to make.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CancelBackfill {
    /// The network whose owed history to stop asking for, exactly as
    /// `GET /v1/status` names it.
    network: Network,
}

/// What `POST /v1/status/backfill/cancel` settled.
///
/// `stopped` is what `GET /v1/status` reported for this network immediately
/// before, and `watches` is what the index actually re-anchored: the first is a
/// snapshot the last pass published, the second is the act itself, and they can
/// differ by whatever a pass settled in between.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(super) struct BackfillCancelled {
    /// The network whose owed history this settled.
    pub(super) network: Network,
    /// How many watched addresses had their birthday raised to the checkpoint.
    /// Each keeps its id, its address and every row already indexed for it.
    pub(super) watches: u64,
    /// The range this stopped asking for, as the report held it just before.
    /// Always present.
    #[schema(required)]
    pub(super) stopped: Option<Backfill>,
}

#[utoipa::path(
    post,
    path = "/v1/status/backfill/cancel",
    operation_id = "cancel_sync_backfill",
    summary = "Stop asking for the history one network still owes",
    description = "Settles the historical range `GET /v1/status` reports as `networks[].backfill` \
        for one network. Every watched address that still owes history is left as one created \
        now would be: same id, same address, birthday at the current checkpoint, nothing owed. \
        **Nothing indexed is deleted** — the rows a partial backfill already wrote stay and are \
        still served; what stops is the reading, so each affected address's reads claim \
        completeness from the raised birthday instead. Forward indexing is untouched. The range \
        can be asked for again by watching the address from a lower start, at the price of \
        reading it. A network that owes nothing answers 409 rather than pretending to act.",
    request_body = CancelBackfill,
    security(("bearerAuth" = [])),
    responses(
        (status = 202, description = "The owed range was settled; no pass will ask for it again",
            body = BackfillCancelled),
        (status = 400, description = "Malformed body", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "The network is not configured", body = ErrorBody),
        (status = 409, description = "The network owes no history", body = ErrorBody),
        (status = 503, description = "The index could not settle the range", body = ErrorBody)
    ),
    tag = "status"
)]
async fn cancel_backfill(
    State(payments): State<Payments>,
    request: Result<Json<CancelBackfill>, JsonRejection>,
) -> Result<(StatusCode, Json<BackfillCancelled>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let health = payments.health.borrow().clone();
    let indexed = health
        .networks()
        .iter()
        .find(|indexed| Network::from(indexed.status.network.clone()) == request.network)
        .ok_or_else(|| Error::not_found("index network is not configured"))?;
    let stopped = Backfill::of(indexed);
    if stopped.is_none() {
        return Err(Error::conflict(
            "index network owes no history; nothing to cancel",
        ));
    }
    let cancelled = payments.cancel(&indexed.status.network).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(BackfillCancelled {
            network: indexed.status.network.clone().into(),
            watches: cancelled,
            stopped,
        }),
    ))
}
