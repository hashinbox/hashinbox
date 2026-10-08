//! **Contract.**
//!
//! The result of one synchronization pass, as [`crate::Indexing::sync`] reports
//! it.

use crate::{BlockPosition, BlockRef, IndexError, IndexErrorKind, Network, OutputKey};
use std::collections::BTreeSet;
use std::time::Duration;

/// What one pass's backfill phase did, and what it has left to do.
///
/// Present on a [`Outcome`] only while at least one watched address still
/// has uninspected history below its registration, so an operator reading
/// `Some(_)` is reading the one thing that makes this network's answers
/// provably short.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backlog {
    /// How many watched addresses are still waiting.
    pub pending: usize,
    /// The lowest block any of them still needs inspected.
    pub resume: BlockPosition,
    /// The highest block any of them needs; no backfill reads above it, so
    /// the work is bounded when it starts and cannot grow with the chain.
    pub anchor: BlockPosition,
    /// Blocks this pass inspected for them, bounded by the policy's batch size
    /// less whatever the forward phase already spent.
    pub inspected: usize,
    /// Why the phase stopped short of its budget, when it did.
    ///
    /// A backfill failure never fails the pass and never changes the network's
    /// state: forward synchronization is what an operator's clients read, and
    /// a node that cannot serve a historical range must not stop it.
    pub failure: Option<IndexError>,
}

/// Whether a network reached the tip it observed during a pass.
///
/// Forward progress only. A network that reached its tip is `Ready` even with
/// history still owed below a watch's registration: see
/// [`Outcome::backfill`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The batch ended before the observed tip; call `sync` again at once.
    CatchingUp,
    /// The checkpoint is at the observed tip.
    Ready,
}

/// The outcome of one `sync` pass for one network.
///
/// [`crate::Health`] holds the last one a network completed, beside the
/// standing condition no single pass can report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The network this status describes.
    pub network: Network,
    /// The last committed block after the pass: the network's confirmed line.
    pub checkpoint: Option<BlockRef>,
    /// The newest block held above the confirmed line, or `None` when nothing
    /// is. An operator reads this beside `checkpoint` to see that a
    /// checkpoint below the tip is the design and not lag.
    pub pending_tip: Option<BlockRef>,
    /// The source tip the pass indexed towards.
    pub observed_tip: Option<BlockRef>,
    /// Whether the pass reached `observed_tip`, committed or held.
    pub phase: Phase,
    /// Required spends this pass committed without a matching indexed output,
    /// in block order, each naming the address whose history is provably short
    /// (see [`crate::IndexStore::commit`]).
    ///
    /// Empty on a pass that accounted for everything. A non-empty list is a
    /// per-address catch-up fact, not a failure: the pass succeeded and the
    /// checkpoint moved.
    pub unmatched_spends: Vec<OutputKey>,
    /// The backfill still owed to addresses watched from below the checkpoint,
    /// or `None` when nothing is owed.
    ///
    /// It does not move [`Outcome::phase`], which is forward progress and
    /// nothing else. An owed range can be one the node will never serve, so a
    /// phase that waited for it would wait for the life of the process; what
    /// the range actually weakens is each read it touches, which already
    /// states how far behind its rows are.
    pub backfill: Option<Backlog>,
}

// What indexing reports about itself: `Condition` (one network's standing
// condition), `Health` (that state with its checkpoint and failure)
// and `SyncHealth` (every network at once). Cached values a reader polls,
// not the result of one pass -- that is `Phase`.

/// What one network's indexing is doing.
///
/// Cached, unlike [`Phase`](crate::Phase): a network's failure no
/// longer ends the process, so the failure has to live somewhere. `Phase`
/// remains the result of one pass; this is the network's standing condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Condition {
    /// The driver finished its active pass and is paused by its caller.
    Paused,
    /// Behind the observed tip and still applying blocks.
    CatchingUp,
    /// The checkpoint is at the observed tip.
    Ready,
    /// The last pass failed retryably; the next one is waited for.
    Retrying,
    /// The last pass failed terminally; nothing is retried until a resume.
    Quarantined,
}

/// One network's current indexing health: its standing condition, and what the
/// last pass that completed reported.
///
/// A [`Outcome`] is what one pass returned; this is what a caller reads
/// between passes, so it holds that status whole rather than restating it, and
/// adds the four facts no pass can report about itself. `status.phase` is the
/// answer of that pass and nothing later: `state` is what the network is doing
/// now, which is why it has two variants [`Phase`] does not, and the two
/// disagree whenever a pass has failed since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Health {
    /// What the network is doing.
    pub state: Condition,
    /// The error that is holding *forward* indexing back; `None` while it is
    /// healthy. Its `kind`, `retryable` and `message` are the whole of what a
    /// probe or a status route reports.
    ///
    /// A backfill failure never appears here. It is not why the network is in
    /// its state — the state is about the tip, and the tip is fine — so it
    /// travels on the status's [`Outcome::backfill`] with the range it
    /// belongs to.
    pub failure: Option<IndexError>,
    /// Consecutive attempts that got nowhere: failed passes, and passes whose
    /// backfill failed. `0` while nothing is failing.
    pub attempts: u32,
    /// Unix seconds at which `state` was entered. Supplied by the driving
    /// loop: this crate owns no clock.
    pub since: u64,
    /// What the last completed pass reported: the network, its confirmed line,
    /// the newest block held above it, the tip it indexed towards and the
    /// history it still owes. Before the first pass it names the network and
    /// nothing else.
    ///
    /// A failed pass never rewrites it, so the checkpoint stays frozen where
    /// indexing got to while the network is retrying or quarantined.
    ///
    /// The owed backfill is reported rather than gating, and this is the whole
    /// of the availability decision: a range a pruned node can never serve
    /// would otherwise hold every chain's readiness down for the life of the
    /// process, while forward indexing answered every question a client
    /// actually asks. An operator reads the stuck range here; a caller reads
    /// the same fact as how far behind the balance or the history page it
    /// weakens is.
    pub status: Outcome,
}

/// Every configured network's health and the readiness it implies.
///
/// The collection owns the readiness rule, so a probe, a status route, and a
/// startup gate cannot disagree about what "ready" means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncHealth {
    networks: Vec<Health>,
    settled: BTreeSet<Network>,
}

impl Health {
    /// A network that has not reported yet: `CatchingUp`, no checkpoint.
    #[must_use]
    pub fn starting(network: &Network) -> Self {
        Self {
            state: Condition::CatchingUp,
            failure: None,
            attempts: 0,
            since: 0,
            status: Outcome {
                network: network.clone(),
                checkpoint: None,
                pending_tip: None,
                observed_tip: None,
                phase: Phase::CatchingUp,
                unmatched_spends: Vec::new(),
                backfill: None,
            },
        }
    }
}

impl SyncHealth {
    /// Every network of `networks`, in that order, none of them reported yet.
    #[must_use]
    pub fn starting(networks: &[Network]) -> Self {
        Self {
            networks: networks.iter().map(Health::starting).collect(),
            settled: BTreeSet::new(),
        }
    }

    /// Replaces one network's row, keeping the configured order.
    ///
    /// A network that is not configured is ignored: the snapshot describes the
    /// composition it was built from, and nothing else.
    pub fn apply(&mut self, health: &Health) {
        let Some(row) = self
            .networks
            .iter_mut()
            .find(|row| row.status.network == health.status.network)
        else {
            return;
        };
        row.clone_from(health);
        if matches!(health.state, Condition::Ready | Condition::Quarantined) {
            self.settled.insert(health.status.network.clone());
        }
    }

    /// Every configured network, in composition order.
    #[must_use]
    pub fn networks(&self) -> &[Health] {
        &self.networks
    }

    /// Whether the process can serve complete history for the chains it still
    /// answers for: no network is mid-flight, and at least one is `Ready`.
    ///
    /// A quarantined network does not hold readiness down, because it will not
    /// recover on its own and the healthy chains still work; its state is
    /// reported per network instead. Every network quarantined is not ready.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.networks
            .iter()
            .all(|row| matches!(row.state, Condition::Ready | Condition::Quarantined))
            && self
                .networks
                .iter()
                .any(|row| row.state == Condition::Ready)
    }

    /// Whether every network has settled at some point: reached `Ready` at least
    /// once, or become `Quarantined`. This is the gate a startup sequence waits
    /// on; `Retrying` is expected to heal and keeps holding it.
    ///
    /// **A latch, not a predicate over the current states.** [`Self::apply`]
    /// records a network the first time it reports `Ready` or `Quarantined` and
    /// nothing ever clears that record, so a network that settled and then fell
    /// behind again still counts here. What the networks are doing *now* is
    /// [`Self::ready`].
    #[must_use]
    pub fn ever_settled(&self) -> bool {
        self.settled.len() == self.networks.len()
    }
}

// The two bounds a pass works inside: how much it may read, and how far
// below the checkpoint a new watch may start.

/// How far below the checkpoint a new watch may start, by default.
///
/// Deliberately a number an operator can exceed on purpose and cannot reach by
/// typo. A mistyped start is the runaway this bounds: it is refused at
/// [`crate::Watching::watch`], synchronously, before any block is fetched,
/// because a background failure an hour later is not a refusal.
const MAX_BACKFILL_BLOCKS: u64 = 100_000;

/// Chain-neutral synchronization policy shared by concrete indexers.
///
/// Two numbers, both of them bounds on work: how much one pass may read, and
/// how much history a single new watch may ask for. Rollback retention and the
/// confirmation line are gone because the chain now answers them itself — the
/// line is [`ChainTips::confirmed_tip`], stated by the source rather than
/// guessed at in a configuration file, and retention described a journal that
/// no longer exists, because nothing the store holds is ever undone.
///
/// [`ChainTips::confirmed_tip`]: crate::ChainTips::confirmed_tip
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Maximum blocks read in one `sync` pass, so a pass stays bounded.
    ///
    /// The forward phase spends it first and the backfill phase gets what is
    /// left, which is what keeps a backfill from starving live indexing: a
    /// network that is behind its tip does no backfill at all until it catches
    /// up.
    pub batch_size: usize,
    /// Maximum blocks below the checkpoint a new watch may start at.
    ///
    /// Zero refuses every historical start, which is the pre-backfill
    /// behaviour stated as a policy instead of happening silently. It bounds
    /// what a *caller* may newly ask for; it does not bound the one-time
    /// coverage sweep an upgraded store performs for addresses registered
    /// before backfill existed.
    pub max_backfill_blocks: u64,
}

impl Budget {
    /// A policy from its batch size, with the default backfill bound.
    #[must_use]
    pub const fn new(batch_size: usize) -> Self {
        Self {
            batch_size,
            max_backfill_blocks: MAX_BACKFILL_BLOCKS,
        }
    }

    /// The same policy, bounding a new watch's history to `blocks`.
    #[must_use]
    pub const fn with_backfill(self, blocks: u64) -> Self {
        Self {
            max_backfill_blocks: blocks,
            ..self
        }
    }

    /// Rejects a start further below `checkpoint` than the policy allows.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`], naming the earliest start this
    /// policy would accept, so the caller can retry with a legal one instead
    /// of guessing.
    pub(crate) fn admits(
        &self,
        start: crate::BlockPosition,
        checkpoint: crate::BlockPosition,
    ) -> Result<(), IndexError> {
        let earliest = checkpoint.0.saturating_sub(self.max_backfill_blocks);
        if start.0 >= earliest {
            return Ok(());
        }
        Err(IndexError::new(
            IndexErrorKind::InvalidRequest,
            format!(
                "a watch may start at most {} blocks below the checkpoint at {}; \
                 the earliest acceptable start is {earliest}",
                self.max_backfill_blocks, checkpoint.0
            ),
            false,
        ))
    }

    /// Checks that the batch size is greater than zero.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`] for a zero batch size.
    pub fn validate(&self) -> Result<(), IndexError> {
        if self.batch_size == 0 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "batch size must be greater than zero",
                false,
            ));
        }
        Ok(())
    }
}

/// A network, its validated [`Budget`] and the [`Schedule`] its chain is
/// polled at: everything a [`crate::Indexer`] needs besides its source,
/// interpreter, and repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub(crate) network: crate::Network,
    pub(crate) policy: Budget,
    pub(crate) schedule: Schedule,
}

impl Config {
    /// Binds `policy` and `schedule` to `network`.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`] when [`Budget::validate`] fails.
    pub fn new(
        network: &impl crate::Networked,
        policy: Budget,
        schedule: Schedule,
    ) -> Result<Self, IndexError> {
        policy.validate()?;
        Ok(Self {
            network: network.canonical(),
            policy,
            schedule,
        })
    }

    /// The rhythm the chain declared for this network.
    #[must_use]
    pub const fn schedule(&self) -> Schedule {
        self.schedule
    }

    /// The validated policy this configuration binds.
    #[must_use]
    pub const fn policy(&self) -> Budget {
        self.policy
    }
}

// How often a driver polls one network: declared by its chain, carried by
// `Config`, read through `crate::Indexing::schedule`. Outside the driver
// feature: a caller looping `crate::Indexing::sync` itself reads it too.

/// How often one unit polls, and how far its retry wait may grow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// Wait after a pass that reached the observed tip. A unit that is still
    /// catching up never waits.
    pub interval: Duration,
    /// Upper bound on the retry wait, so a chain whose node is down keeps
    /// probing at a fixed, cheap rate instead of doubling forever.
    pub ceiling: Duration,
}

impl Schedule {
    /// How many polling intervals the retry wait may grow to by default.
    const CEILING_INTERVALS: u32 = 60;

    /// Polls every `interval`, capping the retry wait at sixty of them.
    #[must_use]
    pub const fn every(interval: Duration) -> Self {
        Self {
            interval,
            ceiling: interval.saturating_mul(Self::CEILING_INTERVALS),
        }
    }

    /// Polls every `interval`, capping the retry wait at `ceiling`. A ceiling
    /// below `interval` is raised to it: a retry never runs sooner than a
    /// healthy poll.
    #[must_use]
    pub fn new(interval: Duration, ceiling: Duration) -> Self {
        Self {
            interval,
            ceiling: ceiling.max(interval),
        }
    }

    /// The wait before retry number `attempts` (the first failed pass is `1`):
    /// the polling interval doubled once per earlier attempt, capped by
    /// [`Self::ceiling`].
    #[must_use]
    pub fn retry(&self, attempts: u32) -> Duration {
        const MAX_DOUBLINGS: u32 = 31;

        let doublings = attempts.saturating_sub(1).min(MAX_DOUBLINGS);
        self.interval
            .saturating_mul(1_u32 << doublings)
            .min(self.ceiling)
    }
}
