//! The half of one pass that reads *below* the confirmed line.
//!
//! Not a second loop, not a second owner and not a second store operation: the
//! same [`Service`] that moved the checkpoint forward turns round, once, and
//! inspects the blocks an observation still owes from its own birthday, against
//! the watch set it already holds. The affected observation is simply further
//! behind (`docs/design/030 §8 (ex-032)`, "What a commit contains").
//!
//! **What this file owns is the scheduling and nothing else.** Every block is
//! inspected by [`Service::inspect`], the one procedure the forward half also
//! reaches ([`crate::scan`]), and the batch is committed by
//! [`Service::apply`], the one commit lifecycle — so neither direction can be
//! given a validation or a step the other lacks. What is left here is where to
//! read from, whom each block is read for, and what is still owed afterwards.
//!
//! Forward progress goes first and spends the batch budget first, so a network
//! that is behind its tip does no filling at all — the tip is what every client
//! reads, and history must never delay it (`docs/design/024` section 3.3).
//!
//! Two things bound the work. A pass reads at most what the forward half left of
//! [`SyncPolicy::batch_size`](crate::SyncPolicy::batch_size), and a watch may
//! not start further below the checkpoint than
//! [`SyncPolicy::max_backfill_blocks`](crate::SyncPolicy::max_backfill_blocks),
//! refused synchronously at [`crate::Watching::watch`]. What it reads *up to* is
//! the checkpoint: an observation's own catch-up ends where the front of the
//! chain begins, which is what makes [`crate::WatchProgress::indexed_through`]
//! one contiguous claim and what
//! [`caught_up`](crate::WatchProgress::caught_up) decides.

use std::collections::BTreeSet;

use crate::{
    Address, BackfillStatus, BlockInterpreter, BlockRef, BlockSource, CanonicalBlock, IndexError,
    IndexErrorKind, IndexedBatch, IndexedBlock, Network, Progress, Service, SyncPlan,
    pass::Outcome, scan::Scan, watches::Backlog,
};

/// Stops asking for history a watch still owes.
///
/// The counterpart of the reporting `024` section 7 settled on. A range this
/// node cannot serve does not hold the API down — it rides on `GET /v1/status`
/// as `networks[].backfill` with the refusal that stopped it — but it does cost
/// one refused fetch per pass, for as long as the watch exists, and nothing in
/// the index ever gives up on it. This is the operator's answer back.
///
/// It is a durable write rather than a signal, which is why it is not on
/// `SyncControl` (the `supervisor` feature's own handle): that one bumps an
/// in-memory counter a parked unit waits on, and holds no store
/// (`docs/design/030` section 5.5 (b)).
pub trait Backfilling: Send + Sync {
    /// Settles every range `network` still owes, and returns the watches it
    /// re-anchored, each carrying its new birthday.
    ///
    /// Each affected watch is left as the one it would have been had it been
    /// created now from [`Start::Fresh`](crate::Start::Fresh): same id, same
    /// address, same asset, birthday at the checkpoint's successor, and **nothing
    /// inspected for it**. That is the one honest way to settle a range without
    /// claiming to have read it — a watch born above the line owes nothing below
    /// it, and [`crate::WatchProgress::indexed_through`] stays `None` until a
    /// block is actually inspected.
    ///
    /// **Nothing indexed is deleted** — the rows a partial catch-up already
    /// wrote, and the rows the address collected as somebody else's counterparty,
    /// stay and are still served; the raised birthday only stops claiming the
    /// range nobody is going to read. Forward indexing for the address never
    /// pauses, so the cancellation costs no block. It also takes effect at once:
    /// the watch is caught up the moment its birthday is above the line, so the
    /// very next pass inspects it for the blocks arriving now.
    ///
    /// **A cancelled range can be resumed**, at the price of reading it again:
    /// [`Watching::unwatch`](crate::Watching::unwatch) the address and watch it
    /// from a lower `Start::At`, or purge the progress with the offline rescan,
    /// which re-derives every range from its registered birthday. What cannot
    /// be resumed is the *claim* without the reading, which is the point.
    ///
    /// It is a network act because the evidence an operator has is a network
    /// aggregate: `networks[].backfill` reports one `resume`, one `anchor` and
    /// one pending count for the whole network, and cancelling per address would
    /// require naming addresses that report is incapable of naming. The returned
    /// watches are what it actually touched.
    ///
    /// Idempotent: a network that owes nothing is `Ok(vec![])`, not an error.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::NetworkMismatch`] for a network this index does not
    /// own, and the store's own errors.
    fn cancel<'a>(
        &'a self,
        network: &'a Network,
    ) -> crate::BoxFuture<'a, Result<Vec<crate::Watch>, IndexError>>;
}

impl<S, I, R> Service<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
    R: crate::Repository,
{
    /// Inspects what one pass can of the blocks its observations still owe, and
    /// reports what is left.
    ///
    /// Never fails the pass. A source that cannot serve a historical range, a
    /// checkpoint that moved under the batch, a lost admission race: all of them
    /// leave the forward progress this pass already committed intact and come
    /// back as [`BackfillStatus::failure`], because a node that cannot serve an
    /// old block is a property of the request, not of the chain
    /// (`docs/design/024` section 8). Only a terminal
    /// [`IndexErrorKind::Store`] — the store disagreeing with itself — stops the
    /// network, because that is the store failing rather than the read.
    pub(super) async fn backfill(
        &self,
        plan: &SyncPlan,
        committed: &mut Outcome,
        budget: usize,
    ) -> Result<Option<BackfillStatus>, IndexError> {
        let Some(checkpoint) = committed.checkpoint.clone() else {
            return Ok(None);
        };
        match self.fill(plan, committed, &checkpoint, budget).await {
            Ok(status) => Ok(status),
            Err(error) if error.kind == IndexErrorKind::Store && !error.retryable => Err(error),
            Err(error) => self.owing(&checkpoint, 0, Some(error)),
        }
    }

    async fn fill(
        &self,
        plan: &SyncPlan,
        committed: &mut Outcome,
        checkpoint: &BlockRef,
        budget: usize,
    ) -> Result<Option<BackfillStatus>, IndexError> {
        let owed = self.watches.behind(Some(checkpoint))?;
        if owed.is_empty() {
            return Ok(None);
        }
        // The lowest block wins, once, for the whole batch: every owing
        // observation shares one fetch and one interpretation per block, so ten
        // of them cost what one does. The top is the checkpoint, for all of
        // them, because that is where the front of the chain begins.
        let Some(from) = owed
            .iter()
            .map(|entry| entry.resume)
            .min()
            .filter(|_| budget > 0)
        else {
            return self.owing(checkpoint, 0, None);
        };
        let blocks = self.fetch_blocks(from, checkpoint.position, budget).await?;
        let filled = self.interpret(blocks, &owed)?;
        let reached = Self::reached(filled.last())?;
        let inspected = filled.len();
        let proved = Self::proved(&owed, &filled, &reached)?;
        // One batch for the whole read, and the only thing the filling direction
        // decides about it: these blocks are already below the line, so they
        // commit together rather than one at a time, and none of them is being
        // held above it. The permit, the line the network is left at, the claims
        // the watch set takes over and the spends the store could not account for
        // are `Service::apply`'s, exactly as they are at the front of the chain.
        let batch = IndexedBatch::new(
            self.config.network.clone(),
            Some(checkpoint.clone()),
            filled,
            proved,
            self.interpreter.settlement(),
        )?;
        self.apply(plan, committed, batch).await?;
        self.owing(checkpoint, inspected, None)
    }

    /// The highest block this read proved was inspected.
    ///
    /// A source asked for `[from, checkpoint]` must produce at least the
    /// checkpoint's own block, so returning nothing is a source refusing to serve
    /// the range rather than an empty stretch of positions. It is therefore an
    /// error and never a settlement: settling a range nothing read would claim
    /// coverage that does not exist, and nothing downstream would ever re-derive
    /// it (`docs/design/030 §8 (ex-032)` section 8, the named risk).
    fn reached(last: Option<&CanonicalBlock>) -> Result<BlockRef, IndexError> {
        last.map(|facts| facts.block().clone()).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::Source,
                "source served no block below the checkpoint",
                true,
            )
        })
    }

    /// Inspects every block this read served, each for the addresses that still
    /// owe it, as **one contiguous reading**.
    ///
    /// The procedure and every check are [`Service::inspect`]'s, so a historical
    /// block is held to exactly what a block arriving at the tip is held to. The
    /// [`Scan`] is what makes the sequence one reading rather than a bag of
    /// blocks: the durable claim this read proves says every block from where an
    /// observation resumed through [`Self::reached`] was inspected for it, and
    /// nothing downstream ever re-derives that, so a source that skipped a
    /// produced block in the middle of the range must be refused rather than
    /// believed.
    ///
    /// Only owing addresses go into the slice. An address whose catch-up is
    /// settled must not be re-interpreted here: its outputs were created and
    /// possibly already spent by blocks this read does not cover, so creating
    /// them again would resurrect a spent output. Every block of this read owes at
    /// least the observation that set `from`, so none of them is inspected for
    /// nothing and the batch always carries the block its claims name.
    fn interpret(
        &self,
        blocks: Vec<I::Block>,
        owed: &[Backlog],
    ) -> Result<Vec<CanonicalBlock>, IndexError> {
        let mut scan = Scan::after(None);
        let mut filled = Vec::with_capacity(blocks.len());
        for block in &blocks {
            let addresses = Self::owing_at(owed, block.block_ref().position);
            filled.push(self.inspect(&mut scan, block, &addresses)?);
        }
        Ok(filled)
    }

    /// The claims this read proves, each checked to resume from the very block its
    /// own observation last recorded.
    ///
    /// One claim per owing observation the read got to, all of them naming
    /// `reached`. What makes that honest is continuity at *both* ends: the blocks
    /// between are one reading ([`Self::interpret`]), and the first block an
    /// observation owed follows the block already recorded for it — the same
    /// [`Scan`] check, against that observation's own expectation instead of
    /// against the block before. Without it a claim could cover a stretch of chain
    /// between two reads that nothing ever inspected, and
    /// [`crate::IndexedBatch::new`] cannot catch that: it checks that a claim
    /// names a block the batch carries, which says nothing about where the
    /// observation resumed from.
    fn proved(
        owed: &[Backlog],
        filled: &[CanonicalBlock],
        reached: &BlockRef,
    ) -> Result<Vec<Progress>, IndexError> {
        owed.iter()
            .filter(|entry| entry.resume <= reached.position)
            .map(|entry| {
                // The read ends at `reached`, which is at or above every resume
                // this filter kept, so the block an observation owed first is one
                // of these.
                let first = filled
                    .iter()
                    .map(CanonicalBlock::block)
                    .find(|block| block.position >= entry.resume)
                    .unwrap_or(reached);
                Scan::after(entry.expected.clone()).follows(first)?;
                Ok(Progress {
                    watch: entry.watch.clone(),
                    expected: entry.expected.clone(),
                    indexed_through: reached.clone(),
                })
            })
            .collect()
    }

    /// The addresses that still owe `position`, deduplicated.
    fn owing_at(owed: &[Backlog], position: crate::BlockPosition) -> Vec<Address> {
        let mut addresses = BTreeSet::new();
        for entry in owed {
            if entry.resume <= position {
                addresses.insert(entry.address.clone());
            }
        }
        addresses.into_iter().collect()
    }

    /// What is still owed after the read, for the pass to report.
    ///
    /// `anchor` is the checkpoint rather than a per-watch line recorded at
    /// registration: under contiguous progress an observation reads up to the
    /// front of the chain and then joins it, so the top of what it owes moves
    /// with the network until it gets there.
    fn owing(
        &self,
        checkpoint: &BlockRef,
        inspected: usize,
        failure: Option<IndexError>,
    ) -> Result<Option<BackfillStatus>, IndexError> {
        let owed = self.watches.behind(Some(checkpoint))?;
        let Some(resume) = owed.iter().map(|entry| entry.resume).min() else {
            return Ok(None);
        };
        let addresses: BTreeSet<_> = owed.iter().map(|entry| &entry.address).collect();
        Ok(Some(BackfillStatus {
            pending: addresses.len(),
            resume,
            anchor: checkpoint.position,
            inspected,
            failure,
        }))
    }
}
