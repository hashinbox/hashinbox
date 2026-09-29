//! The forward phase of one pass: from the checkpoint towards the observed tip.
//!
//! It runs first and spends the batch budget first, because the tip is what
//! every client reads and a backfill must never delay it
//! ([`crate::backfill`]). Nothing here is durable: a block is validated,
//! interpreted, and held above the confirmed line, and only
//! `Service::commit` takes what fell below it.

use crate::{
    BlockInterpreter, BlockSource, IndexError, IndexErrorKind, IndexedBlock, Service, SyncPlan,
    pass::{BlockWindow, Outcome},
};

impl<S, I, R> Service<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
    R: crate::IndexStore,
{
    pub(super) async fn catch_up(
        &self,
        window: &BlockWindow,
        committed: &mut Outcome,
        plan: &SyncPlan,
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
            return Err(IndexError::cannot_connect(
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
    /// and everything above the line is what the overlay is for. A birthday
    /// below the line still wins, so a wallet created before the index existed
    /// is indexed from its own start.
    async fn initialize_checkpoint(
        &self,
        window: &BlockWindow,
        plan: &SyncPlan,
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
                return Err(IndexError::cannot_connect(
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
