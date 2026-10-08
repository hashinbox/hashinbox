//! **Action.**
//!
//! The pass itself: read the chain's two lines, reconcile what is held above
//! the confirmed one, run both phases, commit what fell below it.
//!
//! It is a procedure over the state [`Indexer`] already owns — its watches, its
//! pending blocks, its chain adapter and its store — and not a second object
//! holding a second copy of them. Nothing here is handed a watch set; it reads
//! its own.
//!
//! The phases live beside this file, one each: [`catchup`](crate::run)
//! moves one network's checkpoint towards the confirmed tip, and
//! [`backfill`](crate::run) turns round to fill in the blocks below it
//! that an address watched from its own past was never inspected for. What those
//! two files hold is the *scheduling* of a stretch of chain and nothing else —
//! forward first and with the budget it needs, so a historical range this node
//! cannot serve can never delay the tip.
//!
//! The two things they would otherwise each need a copy of are here instead:
//! [`scan`](crate::read) is the one procedure that turns a block into facts, and
//! [`Indexer::commit_batch`] is the one commit lifecycle both directions reach —
//! permit, write, the line the batch leaves behind, the claims the watch set
//! takes over, and the reconciliation an uncertain outcome needs. What is left
//! per direction is what genuinely differs: holding a block above the line and
//! notifying listeners belong to the forward half alone.

use crate::read::Scan;
use crate::watches::Owed;
use crate::{
    Address, Backlog, BlockCommit, BlockHeight, BlockInterpreter, BlockPosition, BlockRef,
    BlockSource, BlockWithOutputs, IndexError, IndexErrorKind, IndexStore, IndexedBatch, Indexer,
    NativeBlock, Outcome, OutputKey, Phase, Plan, Progress, Repository, pending,
};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

/// What one pass has committed so far: the line it left behind, and the
/// required spends it could not account for along the way.
pub(crate) struct Tally {
    pub(crate) checkpoint: Option<BlockRef>,
    pub(crate) unmatched_spends: Vec<OutputKey>,
}

/// The stretch of chain one pass works over: the confirmed block it may
/// commit up to, and the observed block it may read up to, already checked
/// against each other. Blocks between the two are held, never committed.
pub(crate) struct Window {
    pub(super) observed: BlockRef,
    pub(super) confirmed: BlockRef,
}

/// The highest confirmed line one service has been told about.
///
/// The only history either guard on [`crate::ChainTips::confirmed_tip`] keeps,
/// and the whole of what the backwards guard needs: two numbers per network,
/// in memory, reset by a restart — which is correct, because a restart cannot
/// vouch for what a previous process was told.
pub(crate) struct ConfirmedLine {
    position: BlockPosition,
    height: BlockHeight,
}

struct Guard<'a>(&'a AtomicBool);
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl<S, I, R> Indexer<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
    R: Repository,
{
    pub(crate) async fn reload(&self) -> Result<(), IndexError> {
        let _guard = self.enter()?;
        if self.repository.checkpoint(self.network()).await?.is_some() {
            return Err(IndexError::retryable_conflict(
                "purge the repository before resetting indexing",
            ));
        }
        self.pending.replace(pending::Blocks::default())?;
        self.native.clear()?;
        *self
            .confirmed
            .lock()
            .map_err(|_| IndexError::retryable_conflict("index state poisoned"))? = None;
        self.watches.mark_stale();
        self.watches.load(&self.repository).await
    }

    /// One bounded pass over this service's own state.
    ///
    /// A pass reads two lines from the source. Everything at or below the
    /// confirmed line is committed to the append-only store; everything above
    /// it is held interpreted in `pending`, where a reorg is a truncation and
    /// no stored row is ever undone.
    ///
    /// The contract is [`crate::Indexing::sync`], which calls this and adds
    /// nothing: scheduling, retry delays, quarantine and shutdown are the
    /// optional runner's, and this returns when the batch is spent.
    pub(crate) async fn advance(&self) -> Result<Outcome, IndexError> {
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
        let mut committed = Tally {
            checkpoint,
            unmatched_spends: Vec::new(),
        };
        let applied = self.catch_up(&window, &mut committed, &plan).await?;
        self.commit_held(&window, &mut committed, &mut plan).await?;
        // Forward first, and with the budget it needs: filling in history gets
        // what is left of the batch, which on a network that is behind its tip
        // is nothing at all.
        let budget = self.config.policy.batch_size.saturating_sub(applied);
        let backfill = self.backfill(&plan, &mut committed, budget).await?;
        self.status(&window, committed, backfill)
    }
}

impl<S, I, R> Indexer<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
    R: IndexStore,
{
    fn status(
        &self,
        window: &Window,
        committed: Tally,
        backfill: Option<crate::Backlog>,
    ) -> Result<Outcome, IndexError> {
        let pending_tip = self.pending.tip()?;
        let reached = pending_tip
            .as_ref()
            .or(committed.checkpoint.as_ref())
            .is_some_and(|value| value.position >= window.observed.position);
        Ok(Outcome {
            network: self.config.network.clone(),
            checkpoint: committed.checkpoint,
            pending_tip,
            observed_tip: Some(window.observed.clone()),
            // Forward progress alone. A backfill this network still owes is
            // reported on `backfill`, never by withholding the phase: the
            // range below a watch's registration may be one a pruned node can
            // never serve, and folding that into `CatchingUp` made one such
            // watch hold every reader of every chain at 503 forever. What the gap actually
            // weakens is
            // each read it touches, and each read already says how far behind it is.
            phase: if reached {
                Phase::Ready
            } else {
                Phase::CatchingUp
            },
            unmatched_spends: committed.unmatched_spends,
            backfill,
        })
    }

    /// The window this pass may work over: the source's observed and confirmed
    /// tips, checked against each other and against what it said last time.
    ///
    /// [`crate::ChainTips::confirmed_tip`] states two things about the line it
    /// returns: it is never above the source's own tip, and it never moves
    /// backwards. Both are cheap to believe and expensive to be wrong about —
    /// either one broken means committing a block that can still revert — so
    /// both are guards rather than documentation.
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
    async fn window(&self) -> Result<Window, IndexError> {
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
        Ok(Window {
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
        if self.is_canonical(local).await? {
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
            if self.is_canonical(&block).await? {
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
    /// then advanced straight through it ([`Plan::selection`]). Only the
    /// newest blocks cost a refetch ([`crate::read::Cache`]).
    async fn reinspect(&self, plan: &Plan) -> Result<(), IndexError> {
        if !self.pending.outdated(plan.selection())? {
            return Ok(());
        }
        // Built off to the side and swapped in as one change. Each recovery is
        // an await and may be a full refetch, so emptying the shared collection
        // first left every read in between looking at a partial set of the
        // blocks above the line, and a recovery that failed left that partial
        // set published until a later pass repaired it. The rebuilt one is
        // reachable by nothing until `replace` installs it.
        let rebuilt = pending::Blocks::default();
        for block in self.pending.ascending()? {
            let source_block = self.refetch(block).await?;
            let addresses = plan.active_addresses(source_block.block_ref().position);
            self.admit(&rebuilt, source_block, &addresses, plan).await?;
        }
        self.pending.replace(rebuilt)
    }

    fn enter(&self) -> Result<Guard<'_>, IndexError> {
        self.running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| IndexError::retryable_conflict("synchronization is already running"))?;
        Ok(Guard(&self.running))
    }

    /// Commits every held block at or below the confirmed line, oldest first.
    ///
    /// Each block is one [`IndexedBatch`]: the facts, the checkpoint it extends,
    /// and the claims it proves — every observation in the forward selection
    /// whose next required block this one is or passes, advanced to it. An empty
    /// block advances them exactly as a busy one does, which is what makes
    /// `indexed_through` mean *inspected* rather than *matched*.
    async fn commit_held(
        &self,
        window: &Window,
        committed: &mut Tally,
        plan: &mut Plan,
    ) -> Result<(), IndexError> {
        while let Some(canonical) = self.pending.confirmed(window.confirmed.position)? {
            let block = canonical.block.reference().clone();
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
            self.commit_held_block(canonical, committed, plan).await?;
        }
        Ok(())
    }

    /// Commits one held block, under a permit, and takes over what it proved.
    ///
    /// The precondition is formed here, at the write, out of the checkpoint this
    /// pass holds for the network: a pending block carries facts and knows nothing
    /// about what authorises committing them. `committed.checkpoint` is the
    /// store's own checkpoint as this pass read it, advanced by each block it has
    /// committed since, so it is exactly the block this one must extend — and
    /// [`IndexedBatch::new`] checks that rather than trusting the caller.
    ///
    /// What is forward-specific is only what is left here: the block stops being
    /// held above the line, the line this pass works from moves with it, and
    /// listeners hear about it. Everything a landed commit settles in *either*
    /// direction is [`Self::commit_batch`]'s.
    async fn commit_held_block(
        &self,
        canonical: BlockWithOutputs,
        committed: &mut Tally,
        plan: &mut Plan,
    ) -> Result<(), IndexError> {
        let block = canonical.block.reference().clone();
        let proved = self
            .watches
            .forward(committed.checkpoint.as_ref(), &block)?;
        let batch = IndexedBatch::new(
            self.config.network.clone(),
            committed.checkpoint.clone(),
            vec![canonical],
            proved,
            self.interpreter.settlement(),
        )?;
        self.commit_batch(plan, committed, batch).await?;
        self.pending.committed(&block)?;
        plan.advance(block.clone());
        committed.checkpoint = Some(block.clone());
        self.announce(block);
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
    /// made recoverable rather than a wedge. A future cancelled or failed between the permit's
    /// `start` and
    /// its completion leaves the outcome *unknown* rather than refused, and the
    /// permit's own drop guard is what records that the admission needs a reload.
    pub(super) async fn commit_batch(
        &self,
        plan: &Plan,
        committed: &mut Tally,
        batch: IndexedBatch,
    ) -> Result<Vec<OutputKey>, IndexError> {
        let line = batch.checkpoint().cloned();
        let proved = batch.progress().to_vec();
        let mut permit = plan.begin()?;
        permit.start();
        let unknown = self.repository.commit(batch).await.inspect_err(|error| {
            if error.kind == IndexErrorKind::Conflict {
                self.watches.mark_stale();
            }
        })?;
        permit.complete(line)?;
        // Durable, so the set may take the claims over: they and the facts that
        // justify them were one transaction.
        self.watches.advanced(&proved)?;
        committed.unmatched_spends.extend(unknown.iter().cloned());
        Ok(unknown)
    }

    /// Notifies every listener of one committed block, in registration order.
    ///
    /// Synchronous on purpose: no list is copied per block, and a hook that
    /// cannot await cannot slow the pass by awaiting. The list is shared for the loop rather than
    /// locked across it, so a callback may register another listener.
    fn announce(&self, block: BlockRef) {
        self.listeners.announce(&BlockCommit {
            network: self.config.network.clone(),
            block,
        });
    }
}

// The forward phase of one pass: from the checkpoint towards the observed tip.
//
// It runs first and spends the batch budget first, because the tip is what
// every client reads and a backfill must never delay it
// ([`crate::run`]). Nothing here is durable: a block is validated,
// interpreted, and held above the confirmed line, and only
// `Indexer::commit_held` takes what fell below it.

impl<S, I, R> Indexer<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
    R: crate::IndexStore,
{
    pub(super) async fn catch_up(
        &self,
        window: &Window,
        committed: &mut Tally,
        plan: &Plan,
    ) -> Result<usize, IndexError> {
        let mut applied = 0_usize;
        if committed.checkpoint.is_none() && self.pending.tip()?.is_none() {
            applied = self.initialize_checkpoint(window, plan).await?;
        }

        let Some(tip) = self.pending.tip()?.or_else(|| committed.checkpoint.clone()) else {
            return Ok(applied);
        };
        if applied >= self.config.policy.batch_size || tip.position >= window.observed.position {
            return Ok(applied);
        }
        let start = tip.position.checked_successor().ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "checkpoint position is exhausted",
                false,
            )
        })?;
        let remaining = self.config.policy.batch_size - applied;
        let blocks = self
            .fetch_blocks(start, window.observed.position, remaining)
            .await?;
        if blocks.is_empty() {
            return Err(IndexError::disconnected(
                "source returned no produced block before its observed tip",
            ));
        }
        for source_block in blocks {
            let position = source_block.block_ref().position;
            let addresses = plan.active_addresses(position);
            self.admit(&self.pending, source_block, &addresses, plan)
                .await?;
            applied += 1;
        }
        Ok(applied)
    }

    /// Anchors an empty index.
    ///
    /// The anchor is the confirmed line, not the observed tip: an index with
    /// nothing to catch up on should start from a block it can commit at once,
    /// and everything above the line is held as a pending block. A birthday
    /// below the line still wins, so a wallet created before the index existed
    /// is indexed from its own start.
    async fn initialize_checkpoint(
        &self,
        window: &Window,
        plan: &Plan,
    ) -> Result<usize, IndexError> {
        let start = match plan.earliest_position() {
            Some(position) if position <= window.confirmed.position => position,
            _ => {
                let anchor = self
                    .one_block(window.confirmed.position, window.confirmed.position)
                    .await?;
                self.admit(&self.pending, anchor, &[], plan).await?;
                return Ok(1);
            }
        };
        let first = self.one_block(start, window.observed.position).await?;
        let first_ref = first.block_ref();
        let mut applied = 0;
        if let Some(parent) = &first_ref.parent {
            let anchor = self.one_block(parent.position, parent.position).await?;
            let anchor_ref = anchor.block_ref();
            if anchor_ref.position != parent.position || anchor_ref.hash != parent.hash {
                return Err(IndexError::disconnected(
                    "birthday anchor does not match the first block parent",
                ));
            }
            self.admit(&self.pending, anchor, &[], plan).await?;
            applied += 1;
        }
        if applied < self.config.policy.batch_size {
            let addresses = plan.active_addresses(first_ref.position);
            self.admit(&self.pending, first, &addresses, plan).await?;
            applied += 1;
        }
        Ok(applied)
    }
}

// The half of one pass that reads *below* the confirmed line.
//
// Not a second loop, not a second owner and not a second store operation: the
// same [`Indexer`] that moved the checkpoint forward turns round, once, and
// inspects the blocks an observation still owes from its own birthday, against
// the watch set it already holds. The affected observation is simply further
// behind.
//
// **What this file owns is the scheduling and nothing else.** Every block is
// inspected by [`Indexer::inspect`], the one procedure the forward half also
// reaches ([`crate::read`]), and the batch is committed by
// [`Indexer::commit_batch`], the one commit lifecycle — so neither direction can be
// given a validation or a step the other lacks. What is left here is where to
// read from, whom each block is read for, and what is still owed afterwards.
//
// Forward progress goes first and spends the batch budget first, so a network
// that is behind its tip does no filling at all — the tip is what every client
// reads, and history must never delay it.
//
// Two things bound the work. A pass reads at most what the forward half left of
// [`Budget::batch_size`](crate::Budget::batch_size), and a watch may
// not start further below the checkpoint than
// [`Budget::max_backfill_blocks`](crate::Budget::max_backfill_blocks),
// refused synchronously at [`crate::Watching::watch`]. What it reads *up to* is
// the checkpoint: an observation's own catch-up ends where the front of the
// chain begins, which is what makes [`crate::WatchProgress::indexed_through`]
// one contiguous claim and what
// [`caught_up`](crate::WatchProgress::caught_up) decides.

impl<S, I, R> Indexer<S, I, R>
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
    /// back as [`Backlog::failure`], because a node that cannot serve an
    /// old block is a property of the request, not of the chain. Only a terminal
    /// [`IndexErrorKind::Store`] — the store disagreeing with itself — stops the
    /// network, because that is the store failing rather than the read.
    pub(super) async fn backfill(
        &self,
        plan: &Plan,
        committed: &mut Tally,
        budget: usize,
    ) -> Result<Option<Backlog>, IndexError> {
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
        plan: &Plan,
        committed: &mut Tally,
        checkpoint: &BlockRef,
        budget: usize,
    ) -> Result<Option<Backlog>, IndexError> {
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
        // are `Indexer::commit_batch`'s, exactly as they are at the front of the chain.
        let batch = IndexedBatch::new(
            self.config.network.clone(),
            Some(checkpoint.clone()),
            filled,
            proved,
            self.interpreter.settlement(),
        )?;
        self.commit_batch(plan, committed, batch).await?;
        self.owing(checkpoint, inspected, None)
    }

    /// The highest block this read proved was inspected.
    ///
    /// A source asked for `[from, checkpoint]` must produce at least the
    /// checkpoint's own block, so returning nothing is a source refusing to serve
    /// the range rather than an empty stretch of positions. It is therefore an
    /// error and never a settlement: settling a range nothing read would claim
    /// coverage that does not exist, and nothing downstream would ever re-derive
    /// it.
    fn reached(last: Option<&BlockWithOutputs>) -> Result<BlockRef, IndexError> {
        last.map(|canonical| canonical.block.reference().clone())
            .ok_or_else(|| {
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
    /// The procedure and every check are [`Indexer::inspect`]'s, so a historical
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
        owed: &[Owed],
    ) -> Result<Vec<BlockWithOutputs>, IndexError> {
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
        owed: &[Owed],
        filled: &[BlockWithOutputs],
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
                    .map(|facts| facts.block.reference())
                    .find(|block| block.position >= entry.resume)
                    .unwrap_or(reached);
                Scan::after(entry.expected.clone()).follows(first)?;
                Ok(Progress {
                    watch_id: entry.watch_id.clone(),
                    asset: entry.asset.clone(),
                    expected: entry.expected.clone(),
                    indexed_through: reached.clone(),
                })
            })
            .collect()
    }

    /// The addresses that still owe `position`, deduplicated.
    fn owing_at(owed: &[Owed], position: crate::BlockPosition) -> Vec<Address> {
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
    ) -> Result<Option<Backlog>, IndexError> {
        let owed = self.watches.behind(Some(checkpoint))?;
        let Some(resume) = owed.iter().map(|entry| entry.resume).min() else {
            return Ok(None);
        };
        let addresses: BTreeSet<_> = owed.iter().map(|entry| &entry.address).collect();
        Ok(Some(Backlog {
            pending: addresses.len(),
            resume,
            anchor: checkpoint.position,
            inspected,
            failure,
        }))
    }
}
