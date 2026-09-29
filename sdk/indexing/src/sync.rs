//! The result of one synchronization pass, as [`crate::Indexing::sync`] reports
//! it.

use crate::{BlockPosition, BlockRef, IndexError, Network, OutputKey};

/// What one pass's backfill phase did, and what it has left to do.
///
/// Present on a [`SyncStatus`] only while at least one watched address still
/// has uninspected history below its registration, so an operator reading
/// `Some(_)` is reading the one thing that makes this network's answers
/// provably short.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackfillStatus {
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
    /// a node that cannot serve a historical range must not stop it
    /// (`docs/design/024` section 8).
    pub failure: Option<IndexError>,
}

/// Whether a network reached the tip it observed during a pass.
///
/// Forward progress only. A network that reached its tip is `Ready` even with
/// history still owed below a watch's registration: see
/// [`SyncStatus::backfill`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPhase {
    /// The batch ended before the observed tip; call `sync` again at once.
    CatchingUp,
    /// The checkpoint is at the observed tip.
    Ready,
}

/// The outcome of one `sync` pass for one network.
///
/// Shares five fields with [`crate::NetworkHealth`] by design rather than by
/// accident; the reason the two are not one type is recorded there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncStatus {
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
    pub phase: SyncPhase,
    /// Required spends this pass committed without a matching indexed output,
    /// in block order, each naming the address whose history is provably short
    /// (see [`crate::IndexStore::commit`]).
    ///
    /// Empty on a pass that accounted for everything. A non-empty list is a
    /// per-address completeness fact, not a failure: the pass succeeded and the
    /// checkpoint moved.
    pub unknown_spends: Vec<OutputKey>,
    /// The backfill still owed to addresses watched from below the checkpoint,
    /// or `None` when nothing is owed.
    ///
    /// It does not move [`SyncStatus::phase`], which is forward progress and
    /// nothing else. An owed range can be one the node will never serve, so a
    /// phase that waited for it would wait for the life of the process; what
    /// the range actually weakens is each read it touches, which
    /// [`Completeness`](crate::Completeness) already states there
    /// (`docs/design/024` section 7).
    pub backfill: Option<BackfillStatus>,
}
