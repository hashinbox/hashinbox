//! The pass itself: read the chain's two lines, reconcile what is held above
//! the confirmed one, run both phases, commit what fell below it.
//!
//! It is a procedure over the state [`Service`] already owns — its watches, its
//! pending blocks, its chain adapter and its store — and not a second object
//! holding a second copy of them. Nothing here is handed a watch set; it reads
//! its own.
//!
//! The phases live beside this file, one each: [`catchup`](crate::catchup)
//! moves one network's checkpoint towards the confirmed tip, and
//! [`backfill`](crate::backfill) turns round to fill in the blocks below it
//! that an address watched from its own past was never inspected for. What those
//! two files hold is the *scheduling* of a stretch of chain and nothing else —
//! forward first and with the budget it needs, so a historical range this node
//! cannot serve can never delay the tip.
//!
//! The two things they would otherwise each need a copy of are here instead:
//! [`scan`](crate::scan) is the one procedure that turns a block into facts, and
//! [`Service::apply`] is the one commit lifecycle both directions reach —
//! permit, write, the line the batch leaves behind, the claims the watch set
//! takes over, and the reconciliation an uncertain outcome needs. What is left
//! per direction is what genuinely differs: holding a block above the line and
//! notifying listeners belong to the forward half alone.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::{
    BlockCommit, BlockHeight, BlockInterpreter, BlockPosition, BlockRef, BlockSource,
    CanonicalBlock, IndexError, IndexErrorKind, IndexStore, IndexedBatch, IndexedBlock,
    OutputChanges, OutputKey, Repository, Service, SyncPhase, SyncPlan, SyncStatus,
    overlay::Frontier,
};

/// What one pass has committed so far: the line it left behind, and the
/// required spends it could not account for along the way.
pub(crate) struct Outcome {
    pub(crate) checkpoint: Option<BlockRef>,
    pub(crate) unknown_spends: Vec<OutputKey>,
}

/// The stretch of chain one pass works over: the confirmed block it may
/// commit up to, and the observed block it may read up to, already checked
/// against each other. Blocks between the two are held, never committed.
pub(crate) struct BlockWindow {
    pub(super) observed: BlockRef,
    pub(super) confirmed: BlockRef,
}

/// The highest confirmed line one service has been told about.
///
/// The only history either guard on [`crate::ChainTip::confirmed_tip`] keeps,
/// and the whole of what the backwards guard needs: two numbers per network,
/// in memory, reset by a restart — which is correct, because a restart cannot
/// vouch for what a previous process was told.
pub(crate) struct ConfirmedLine {
    position: BlockPosition,
    height: BlockHeight,
}

struct RunningGuard<'a>(&'a AtomicBool);
impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl<S, I, R> Service<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
    R: Repository,
{
    /// One bounded pass over this service's own state.
    ///
    /// A pass reads two lines from the source. Everything at or below the
    /// confirmed line is committed to the append-only store; everything above
    /// it is held interpreted in `pending`, where a reorg is a truncation and
    /// no stored row is ever undone (`docs/design/030 §8 (ex-025)`).
    ///
    /// The contract is [`crate::Indexing::sync`], which calls this and adds
    /// nothing: scheduling, retry delays, quarantine and shutdown are the
    /// optional runner's, and this returns when the batch is spent.
    pub(crate) async fn advance(&self) -> Result<SyncStatus, IndexError> {
        // The durable watch list, read into the set this service owns — once,
        // from one snapshot, before anything reads it.
        self.watches.load(&self.repository).await?;
        let _guard = self.enter()?;
        let window = self.window().await?;
        // The selection is captured once, here, after the window rather than
        // before it. An address watched while the window was being read has a
        // birthday of the current checkpoint plus one, which covers the blocks
        // this pass is about to apply; planning against an earlier set would
        // skip that address for blocks it was watched to cover.
        let checkpoint = self.repository.checkpoint(&self.config.network).await?;
        // The selection is captured against that checkpoint, because which
        // observations belong in it depends on it: one whose own history has not
        // reached the line is inspected for what it owes instead
        // (`crate::WatchProgress::caught_up`).
        let mut plan = self.watches.plan(checkpoint.clone())?;
        self.confirm_checkpoint(checkpoint.as_ref()).await?;
        self.reinspect(&plan).await?;
        self.unwind().await?;
        let mut committed = Outcome {
            checkpoint,
            unknown_spends: Vec::new(),
        };
        let applied = self.catch_up(&window, &mut committed, &plan).await?;
        self.commit(&window, &mut committed, &mut plan).await?;
        // Forward first, and with the budget it needs: filling in history gets
        // what is left of the batch, which on a network that is behind its tip
        // is nothing at all.
        let budget = self.config.policy.batch_size.saturating_sub(applied);
        let backfill = self.backfill(&plan, &mut committed, budget).await?;
        self.status(&window, committed, backfill)
    }
}

impl<S, I, R> Service<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
    R: IndexStore,
{
    fn status(
        &self,
        window: &BlockWindow,
        committed: Outcome,
        backfill: Option<crate::BackfillStatus>,
    ) -> Result<SyncStatus, IndexError> {
        let pending_tip = self.pending.tip()?;
        let reached = pending_tip
            .as_ref()
            .or(committed.checkpoint.as_ref())
            .is_some_and(|value| value.position >= window.observed.position);
        Ok(SyncStatus {
            network: self.config.network.clone(),
            checkpoint: committed.checkpoint,
            pending_tip,
            observed_tip: Some(window.observed.clone()),
            // Forward progress alone. A backfill this network still owes is
            // reported on `backfill`, never by withholding the phase: the
            // range below a watch's registration may be one a pruned node can
            // never serve, and folding that into `CatchingUp` made one such
            // watch hold every reader of every chain at 503 forever
            // (`docs/design/024` section 7). What the gap actually weakens is
            // each read it touches, and `Completeness` already says so there.
            phase: if reached {
                SyncPhase::Ready
            } else {
                SyncPhase::CatchingUp
            },
            unknown_spends: committed.unknown_spends,
            backfill,
        })
    }

    /// The window this pass may work over: the source's observed and confirmed
    /// tips, checked against each other and against what it said last time.
    ///
    /// [`crate::ChainTip::confirmed_tip`] states two things about the line it
    /// returns: it is never above the source's own tip, and it never moves
    /// backwards. Both are cheap to believe and expensive to be wrong about —
    /// either one broken means committing a block that can still revert — so
    /// both are guards rather than documentation (`docs/design/030 §8 (ex-025)` section 9).
    /// The first is one comparison; the second needs [`ConfirmedLine`], which
    /// is two numbers.
    ///
    /// The confirmed line is read **first**, and the order is the whole of
    /// whether an honest source can fail that comparison. These are two round
    /// trips and a chain does not stand still between them: reading the tip
    /// first leaves the *older* number as the ceiling, so more than
    /// `confirmation_depth` blocks arriving in the gap accuses a source that
    /// said nothing untrue. Reading the confirmed line first makes the ceiling
    /// the *newer* number, so an advance can only widen the window.
    async fn window(&self) -> Result<BlockWindow, IndexError> {
        let confirmed = self
            .source
            .confirmed_tip()
            .await
            .map_err(IndexError::from)?;
        let observed = self.source.tip().await.map_err(IndexError::from)?;
        if confirmed.position > observed.position || confirmed.height > observed.height {
            return Err(IndexError::new(
                IndexErrorKind::Source,
                "source reports a confirmed tip above its own tip",
                true,
            ));
        }
        self.hold_confirmed(&confirmed)?;
        Ok(BlockWindow {
            observed,
            confirmed,
        })
    }

    /// Refuses a confirmed line below the highest one this source has stated,
    /// and records the new high-water mark.
    ///
    /// A line that walks backwards is a provider lagging behind its peers or
    /// lying outright — a misconfigured Ethereum node, a replica behind a load
    /// balancer — and believing it would let the pass commit above a line the
    /// chain may still take back. Retryable, because the usual cause is one
    /// stale replica out of several: a single pass fails, the next one asks
    /// again, and a source that keeps doing it quarantines the network through
    /// the ordinary attempt count rather than needing a rule of its own.
    fn hold_confirmed(&self, confirmed: &BlockRef) -> Result<(), IndexError> {
        let mut line = self.confirmed.lock().map_err(|_| {
            IndexError::new(IndexErrorKind::Store, "confirmed line is poisoned", false)
        })?;
        if let Some(highest) = line.as_ref()
            && (confirmed.position < highest.position || confirmed.height < highest.height)
        {
            return Err(IndexError::new(
                IndexErrorKind::Source,
                "source moved its confirmed tip backwards",
                true,
            ));
        }
        *line = Some(ConfirmedLine {
            position: confirmed.position,
            height: confirmed.height,
        });
        Ok(())
    }

    /// Checks that the committed checkpoint is still canonical.
    ///
    /// This is the one failure the append-only store cannot absorb: the chain
    /// reorganized past its own confirmed line, so rows that were promised
    /// irreversible are not. It is impossible on Ethereum and Solana, whose
    /// lines are consensus guarantees, and on Bitcoin it means a reorg deeper
    /// than the configured confirmation depth. Terminal, and repaired only by
    /// an offline rescan.
    async fn confirm_checkpoint(&self, checkpoint: Option<&BlockRef>) -> Result<(), IndexError> {
        let Some(local) = checkpoint else {
            return Ok(());
        };
        let canonical = self
            .source
            .canonical_at(local.position)
            .await
            .map_err(IndexError::from)?;
        if canonical.as_ref() == Some(local) {
            return Ok(());
        }
        Err(IndexError::confirmed_reorg(format!(
            "committed block at height {} is no longer canonical; the chain \
             reorganized past its confirmed line and this network must be \
             rescanned",
            local.height.0
        )))
    }

    /// Drops every pending block the source no longer calls canonical.
    ///
    /// Each held block is linked to the one below it, so the newest one
    /// vouches for all of them: the ordinary pass costs a single
    /// `canonical_at`, and only a real reorg walks down.
    async fn unwind(&self) -> Result<(), IndexError> {
        for block in self.pending.descending()? {
            let canonical = self
                .source
                .canonical_at(block.position)
                .await
                .map_err(IndexError::from)?;
            if canonical.as_ref() == Some(&block) {
                return Ok(());
            }
            self.pending.unwind(block.position)?;
        }
        Ok(())
    }

    /// Inspects every pending block again when the address selection changed.
    ///
    /// A block above the line was interpreted for the addresses in the selection
    /// then. Committing it unchanged under a selection that has since gained one
    /// would commit facts that omit that address, and nothing ever revisits a
    /// committed block, so the facts are produced again first.
    ///
    /// **Three things widen the selection, and this asks about all three**,
    /// because it compares what the plan captured rather than waiting to be told:
    /// a registration; an observation finishing its own catch-up and *joining*
    /// the front ([`crate::WatchProgress::caught_up`]); and a reload of the
    /// durable mirror restoring progress this process had not recorded. Only the
    /// first publishes anything, and keying this on the publication count alone
    /// is how a block above the line committed without an address whose progress
    /// then advanced straight through it ([`SyncPlan::selection`]). Only the
    /// newest blocks cost a refetch ([`crate::fetch::NativeBlocks`]).
    async fn reinspect(&self, plan: &SyncPlan) -> Result<(), IndexError> {
        if !self.pending.outdated(plan.selection())? {
            return Ok(());
        }
        // Built off to the side and swapped in as one change. Each recovery is
        // an await and may be a full refetch, so emptying the shared overlay
        // first left every read in between looking at a partial set of the
        // blocks above the line, and a recovery that failed left that partial
        // set published until a later pass repaired it. The rebuilt overlay is
        // reachable by nothing until `replace` installs it.
        let rebuilt = Frontier::default();
        for block in self.pending.ascending()? {
            let source_block = self.recover(block).await?;
            let addresses = plan.active_addresses(source_block.block_ref().position);
            self.admit(&rebuilt, source_block, &addresses, plan).await?;
        }
        self.pending.replace(rebuilt)
    }

    fn enter(&self) -> Result<RunningGuard<'_>, IndexError> {
        self.running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| IndexError::retryable_conflict("synchronization is already running"))?;
        Ok(RunningGuard(&self.running))
    }

    /// Commits every held block at or below the confirmed line, oldest first.
    ///
    /// Each block is one [`IndexedBatch`]: the facts, the checkpoint it extends,
    /// and the claims it proves — every observation in the forward selection
    /// whose next required block this one is or passes, advanced to it. An empty
    /// block advances them exactly as a busy one does, which is what makes
    /// `indexed_through` mean *inspected* rather than *matched*.
    async fn commit(
        &self,
        window: &BlockWindow,
        committed: &mut Outcome,
        plan: &mut SyncPlan,
    ) -> Result<(), IndexError> {
        while let Some(facts) = self.pending.confirmed(window.confirmed.position)? {
            let block = facts.block().clone();
            // A commit whose outcome was lost leaves the store's own line at or
            // above this block: it is written, its claims are recorded, and
            // there is nothing left to do but stop holding it. Presenting it
            // again would be a batch the store recognises and a listener must
            // not hear about twice, so the pass does neither.
            if committed
                .checkpoint
                .as_ref()
                .is_some_and(|line| line.position >= block.position)
            {
                self.pending.committed(&block)?;
                continue;
            }
            self.extend(facts, committed, plan).await?;
        }
        Ok(())
    }

    /// Commits one held block, under a permit, and takes over what it proved.
    ///
    /// The precondition is formed here, at the write, out of the checkpoint this
    /// pass holds for the network: the overlay carries facts and knows nothing
    /// about what authorises committing them. `committed.checkpoint` is the
    /// store's own checkpoint as this pass read it, advanced by each block it has
    /// committed since, so it is exactly the block this one must extend — and
    /// [`IndexedBatch::new`] checks that rather than trusting the caller.
    ///
    /// What is forward-specific is only what is left here: the block stops being
    /// held above the line, the line this pass works from moves with it, and
    /// listeners hear about it. Everything a landed commit settles in *either*
    /// direction is [`Self::apply`]'s.
    async fn extend(
        &self,
        facts: CanonicalBlock,
        committed: &mut Outcome,
        plan: &mut SyncPlan,
    ) -> Result<(), IndexError> {
        let block = facts.block().clone();
        // Storage consumes the facts, so keep the ones a listener needs — but
        // only when one is actually registered.
        let changed = self
            .listeners
            .listening()
            .then(|| (facts.transactions().to_vec(), facts.outputs().clone()));
        let proved = self
            .watches
            .forward(committed.checkpoint.as_ref(), &block)?;
        let batch = IndexedBatch::new(
            self.config.network.clone(),
            committed.checkpoint.clone(),
            vec![facts],
            proved,
            self.interpreter.settlement(),
        )?;
        let unknown = self.apply(plan, committed, batch).await?;
        self.pending.committed(&block)?;
        plan.advance(block.clone());
        committed.checkpoint = Some(block.clone());
        self.announce(block, changed, unknown);
        Ok(())
    }

    /// One batch through the store, and everything a landed commit settles.
    ///
    /// **The commit lifecycle, once, for both directions.** A batch of blocks
    /// arriving at the front of the chain and a batch filling in blocks an
    /// observation owes differ in how they are *scheduled* and in nothing that
    /// happens here: each reserves this network's one repository transition, is
    /// written, leaves the network at the line the batch itself derived, hands the
    /// watch set the claims it proved, and adds whatever required spends the store
    /// could not account for to the pass's report. Two copies of that sequence is
    /// what `docs/DEFECT_PATTERNS.md` section 3.6 is about.
    ///
    /// The line the permit completes with is [`IndexedBatch::checkpoint`] rather
    /// than a value a caller chose, because it is a property of the blocks: a
    /// batch above the line moves it to its highest block and a batch at or below
    /// it leaves it exactly where it was. That is the same derivation the store
    /// applies, and it is why the two directions share one permit without either
    /// losing the other's work.
    ///
    /// **An uncertain outcome is reconciled here, in both forms it takes.** A
    /// store refuses a batch whose expected checkpoint or expected progress is
    /// not what it holds, which happens when a write this index lost the outcome
    /// of actually landed: the set's belief is then behind the store's, no number
    /// of retries derived from that belief will ever be accepted, so the mirror is
    /// marked stale and the next pass reads it again — an expected-value guard
    /// made recoverable rather than a wedge (`docs/design/030 §8 (ex-032)`,
    /// `Progress`). A future cancelled or failed between the permit's `start` and
    /// its completion leaves the outcome *unknown* rather than refused, and the
    /// permit's own drop guard is what records that the admission needs a reload.
    pub(super) async fn apply(
        &self,
        plan: &SyncPlan,
        committed: &mut Outcome,
        batch: IndexedBatch,
    ) -> Result<Vec<OutputKey>, IndexError> {
        let line = batch.checkpoint().cloned();
        let proved = batch.progress().to_vec();
        let mut permit = plan.begin()?;
        permit.start();
        let unknown = self.repository.commit(batch).await.inspect_err(|error| {
            if error.kind == IndexErrorKind::Conflict {
                self.watches.stale();
            }
        })?;
        permit.complete(line)?;
        // Durable, so the set may take the claims over: they and the facts that
        // justify them were one transaction.
        self.watches.advanced(&proved)?;
        committed.unknown_spends.extend(unknown.iter().cloned());
        Ok(unknown)
    }

    /// Notifies every listener of one committed block, in registration order.
    ///
    /// Synchronous on purpose: no list is copied per block, and a hook that
    /// cannot await cannot slow the pass by awaiting (`docs/design/030 §8
    /// (ex-029)` section 6.3). The list is shared for the loop rather than
    /// locked across it, so a callback may register another listener.
    fn announce(
        &self,
        block: BlockRef,
        changed: Option<(Vec<crate::CanonicalTransaction>, OutputChanges)>,
        unknown_spends: Vec<OutputKey>,
    ) {
        let Some((transactions, outputs)) = changed else {
            return;
        };
        self.listeners.announce(&BlockCommit {
            network: self.config.network.clone(),
            block,
            transactions,
            outputs,
            unknown_spends,
        });
    }
}
