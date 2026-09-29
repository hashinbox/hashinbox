//! One reading of the chain, and the whole of what every reading checks before
//! it believes a block.
//!
//! Both halves of a pass read a stretch of chain and turn each block into this
//! network's facts: [`catchup`](crate::catchup) reads towards the observed tip,
//! and [`backfill`](crate::backfill) reads the blocks below the checkpoint that
//! an observation watched from its own past still owes. What they *check* is the
//! same, so it is here once rather than once per direction — two copies of it is
//! how a historical block came to be accepted under weaker rules than a
//! committed one (`docs/DEFECT_PATTERNS.md` section 1.2).
//!
//! The counterpart of [`fetch`](crate::fetch), which checks what one *request*
//! came back with: no more blocks than were asked for, none outside the range,
//! and strictly ascending positions. Those are properties of one answer, and a
//! reading's continuity is not — the forward half validates each block against
//! whatever is already held above the line, including on a re-inspection that
//! fetched nothing at all — so continuity is a cursor threaded through the
//! reading ([`Scan`]) rather than a check inside a fetch.
//!
//! What stays per direction is *scheduling* and nothing else: the forward half
//! runs first and spends the batch budget first, so a historical range this node
//! cannot serve can never delay the tip every client reads.

use std::sync::Arc;

use crate::{
    Address, BlockInterpreter, BlockParent, BlockPosition, BlockRef, BlockSource, CanonicalBlock,
    IndexError, IndexErrorKind, IndexedBlock, Service, SyncPlan, overlay::Frontier,
};

/// One reading of the chain in progress: the block the next one must follow.
///
/// A reading is contiguous by construction, and that is what lets a single
/// [`BlockRef`] say how far anything has been inspected
/// ([`crate::WatchProgress::indexed_through`]): every block is checked against
/// the one before it, and the first against whatever the reading resumes after —
/// the newest block held above the line, the network's checkpoint, or the block
/// one observation last recorded for itself.
pub(crate) struct Scan {
    /// The block read last, which the next one must follow.
    previous: Option<BlockRef>,
}

impl Scan {
    /// A reading that resumes after `previous`, or one whose first block has
    /// nothing here to follow when it is `None`.
    pub(crate) const fn after(previous: Option<BlockRef>) -> Self {
        Self { previous }
    }

    /// Checks that `block` follows what this reading has read, and takes it over
    /// as the block the next one must follow.
    ///
    /// Produced blocks are adjacent by *height* and linked by parent, whatever
    /// the positions in between: Solana's positions are slots and most hold no
    /// block, so a gap between two positions is not a gap in the reading
    /// ([`crate::WatchProgress::next_required`]).
    ///
    /// # Errors
    ///
    /// A retryable [`IndexErrorKind::CannotConnect`] for a block that does not
    /// follow, and [`IndexErrorKind::InvalidBlock`] when the previous block's
    /// height has no successor.
    pub(crate) fn follows(&mut self, block: &BlockRef) -> Result<(), IndexError> {
        if block.position == BlockPosition(0) {
            if block.parent.is_some() {
                return Err(IndexError::cannot_connect(
                    "genesis block must not have a parent",
                ));
            }
        } else if block.parent.is_none() {
            return Err(IndexError::cannot_connect(
                "non-genesis block is missing its parent",
            ));
        }
        if let Some(read) = &self.previous {
            let expected_height = read.height.checked_successor().ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "checkpoint height is exhausted",
                    false,
                )
            })?;
            if block.position <= read.position
                || block.height != expected_height
                || block.parent.as_ref()
                    != Some(&BlockParent {
                        position: read.position,
                        hash: read.hash.clone(),
                    })
            {
                return Err(IndexError::cannot_connect(
                    "source block does not follow the block before it",
                ));
            }
        }
        self.previous = Some(block.clone());
        Ok(())
    }
}

impl<S, I, R> Service<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
{
    /// The next block of one reading, inspected for `addresses` and validated as
    /// this network's facts.
    ///
    /// **The single inspection procedure**, and the whole of it: the block
    /// follows the reading so far, the interpreter did not change which block it
    /// read, and what it produced is something this network can hold — no
    /// transaction identity twice, no overlapping output change, no movement or
    /// credit of another network ([`CanonicalBlock::new`]). One validation point,
    /// reached from both directions, so neither can be given a rule the other
    /// lacks.
    ///
    /// What is deliberately *not* here is the source's own canonicality. Only
    /// blocks above the confirmed line can still be taken back, and only
    /// [`Self::admit`] reads any; a historical block is at or below a checkpoint
    /// this pass already proved canonical, and a confirmed block never reverts
    /// (`docs/design/030 §8 (ex-025)`), so asking again would be one RPC per
    /// block for no further proof.
    pub(super) fn inspect(
        &self,
        scan: &mut Scan,
        block: &I::Block,
        addresses: &[Address],
    ) -> Result<CanonicalBlock, IndexError> {
        let reference = block.block_ref();
        scan.follows(&reference)?;
        let interpreted = self.interpreter.inspect(block, addresses)?;
        interpreted.validate_reference(&reference)?;
        CanonicalBlock::new(self.config.network.clone(), interpreted)
    }

    /// Inspects one block and holds it above the confirmed line, in `pending`.
    ///
    /// Which overlay is an argument because it is not always the shared one:
    /// [`Service::reinspect`](Self::reinspect) admits into a private collection
    /// and swaps it in afterwards, so a read never sees a half-rebuilt one. That
    /// collection is also the forward reading's own cursor — a block follows
    /// whatever is held, or the checkpoint when nothing is — so the [`Scan`] is
    /// taken from it here rather than carried across calls.
    ///
    /// Nothing durable happens here, and nothing written down says where the
    /// block will go: it is inspected by [`Self::inspect`], checked one last time
    /// against the source that served it, and kept as facts.
    pub(super) async fn admit(
        &self,
        pending: &Frontier,
        source_block: I::Block,
        addresses: &[Address],
        plan: &SyncPlan,
    ) -> Result<(), IndexError> {
        let block = source_block.block_ref();
        let mut scan = Scan::after(pending.tip()?.or_else(|| plan.checkpoint().cloned()));
        let facts = self.inspect(&mut scan, &source_block, addresses)?;
        let canonical = self.source.canonical_at(block.position).await?;
        if canonical.as_ref() != Some(&block) {
            return Err(IndexError::cannot_connect(
                "source block changed before commit",
            ));
        }
        self.native.keep(block, Arc::new(source_block))?;
        pending.admit(facts, plan.selection())
    }
}
