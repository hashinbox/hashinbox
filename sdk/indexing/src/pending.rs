//! **Organ.**
//!
//! The blocks above the confirmed line: fetched, interpreted, not committed.

use std::sync::{
    Mutex, MutexGuard,
    atomic::{AtomicU64, Ordering},
};

use crate::{BlockPosition, BlockRef, BlockWithOutputs, IndexError, IndexErrorKind};

/// One pending block: validated against its predecessor, ready to commit, and
/// not committed. It becomes a commit once the confirmed line reaches it, and
/// is discarded if the chain takes it back first.
struct Candidate {
    /// The validated block this will commit, with no precondition of its own:
    /// which checkpoint authorises the write is settled where the write
    /// happens, not here (`crate::IndexedBatch`).
    canonical: BlockWithOutputs,
    /// The address selection the block was interpreted against.
    selection: u64,
}

/// What a consumer samples an index's pending facts through.
///
/// **The whole of what an index exposes about the blocks above its confirmed
/// line**, and deliberately no more: a reading of them, never the collection.
/// Nothing here answers a balance, a history page or a spendable set. Those are
/// a repository's queries, and a consumer that wants them pending-inclusive
/// composes the repository's answer with a [`Sample`] itself.
///
/// A trait because the composition and the index live in different crates: a
/// consumer holds this capability rather than the generic
/// [`crate::Indexer`] that implements it, and its own tests substitute a list
/// they can change between the two moments of a commit.
pub trait Pending: Send + Sync {
    /// Every block held above the confirmed line at this instant.
    ///
    /// Take it **before** reading the repository, and narrow it with the
    /// answer that read came back with ([`Sample::above`]).
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`] when the held list cannot be read.
    fn sampled(&self) -> Result<Sample, IndexError>;
}

/// One reading of the blocks above the line, and the revision naming it.
///
/// The blocks are reachable only through [`Self::above`], so a consumer cannot
/// fold a sample without saying which durable read it completes. A read that
/// spans more than one page carries [`Self::revision`] in its cursor, so a
/// later page is folded over the same blocks or refused.
pub struct Sample {
    /// The blocks, oldest first, in commit order.
    blocks: Vec<BlockWithOutputs>,
    /// Which revision of the held list these blocks are.
    revision: u64,
}

impl Sample {
    /// A reading of `blocks`, oldest first, taken from the list `revision`
    /// names.
    ///
    /// An index builds one from the list it holds. It is public for whatever
    /// else implements [`Pending`], which owes the same two properties: the
    /// blocks are in commit order, and the revision differs whenever the list
    /// does.
    #[must_use]
    pub const fn new(blocks: Vec<BlockWithOutputs>, revision: u64) -> Self {
        Self { blocks, revision }
    }

    /// Which list this reading is of.
    ///
    /// Equal on two samples exactly when nothing was held, dropped, unwound or
    /// re-interpreted between them. The durable checkpoint cannot stand in for
    /// it: a block held above the line does not move the checkpoint.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// The sampled blocks `answer` does not already hold.
    ///
    /// Every answer that folds this sample is two reads — the store, and the
    /// blocks the same index holds above the confirmed line — and a commit can
    /// land between them, moving one block out of the second and into the
    /// first. Two rules together make that harmless, and both are needed:
    ///
    /// 1. the pending blocks are sampled **first**, so the store read can only
    ///    be newer than the sample and everything the store gained in between
    ///    is a block the sample still holds;
    /// 2. the sample is then narrowed to what the store's own checkpoint says
    ///    the store does not hold, which drops exactly those blocks.
    ///
    /// Each rule alone is a defect. Reading the store first loses a block
    /// committed in between altogether: its spends stop applying, so outputs
    /// it spent are offered as spendable again, and the outputs it created
    /// disappear. Sampling first and narrowing nothing counts that block
    /// twice, because the store and the sample both hold it while the commit
    /// is halfway done. With both, the answer is the state at the moment the
    /// sample was taken, and confirmed and pending stay disjoint.
    ///
    /// **The line is taken from the answer, so it cannot be one fetched beside
    /// it.** A checkpoint read at another instant is a line the rows were not
    /// read at, and narrowing by it counts a block on both sides or on
    /// neither. That is why this takes what the store answered
    /// ([`Checkpointed`]) and not a [`BlockRef`]. The sample is consumed, so
    /// one reading completes one answer.
    ///
    /// A checkpoint that came from anywhere else is refused by the compiler,
    /// however it is spelled. This is the read that counted a transaction
    /// twice, and it no longer builds:
    ///
    /// ```compile_fail,E0277
    /// use indexing::{Checkpoint, IndexError, Network, Pending};
    ///
    /// async fn pending(
    ///     index: &(impl Pending + Checkpoint),
    ///     network: &Network,
    /// ) -> Result<usize, IndexError> {
    ///     let sample = index.sampled()?;
    ///     // Read beside the store's answer, so at another instant.
    ///     let checkpoint = index.checkpoint(network).await?;
    ///     Ok(sample.above(&checkpoint).len())
    /// }
    /// ```
    #[must_use]
    pub fn above<A: Checkpointed + ?Sized>(self, answer: &A) -> Vec<BlockWithOutputs> {
        let Some(line) = answer.checkpoint() else {
            return self.blocks;
        };
        self.blocks
            .into_iter()
            .filter(|canonical| canonical.block.reference().position > line.position)
            .collect()
    }
}

/// A repository's answer, carrying the checkpoint its rows were read at.
///
/// The one thing a [`Sample`] may be narrowed by
/// ([`Sample::above`]). A page, a balance and a list of outputs are each
/// read from one snapshot, and the checkpoint of that snapshot is part of the
/// answer; this is how a consumer hands the two over together, so the line
/// cannot be separated from the rows it belongs to.
///
/// Implemented by the answers a store gives: [`crate::OutputPage`] and
/// [`crate::Location`]. A consumer with an answer type of its own implements
/// it for that type, and owes what those do:
/// the checkpoint is the one the rows came from, not one read before or after.
///
/// Deliberately not implemented for a [`BlockRef`] or an `Option` of one. A
/// bare checkpoint says nothing about which read it came with, which is the
/// whole of what this trait states.
pub trait Checkpointed {
    /// The checkpoint the answer was read at, or `None` when the network had
    /// committed nothing yet.
    fn checkpoint(&self) -> Option<&BlockRef>;
}

/// The blocks above the confirmed line, oldest first.
///
/// This is not a second store. It is the blocks synchronization already
/// fetched and already interpreted, held instead of thrown away, so the
/// durable store only ever receives blocks that cannot revert while every read
/// can still answer "what is on its way". A
/// reorg shallower than the line is [`Blocks::unwind`] — no store I/O
/// and no backend code at all.
///
/// One payload, one bound: the interpreted facts ([`BlockWithOutputs`]), which
/// cover every block above the line unconditionally and are proportional to the
/// watched addresses those blocks touch, usually nothing. The native blocks
/// those facts were read from are not here — they are megabytes, they are the
/// chain's type rather than this crate's, and only the fetching path ever
/// wants one again, so [`crate::read::Cache`] keeps its own bounded
/// cache of them. Neither is the checkpoint a commit of one is conditional on:
/// what this collection holds is facts, and a reader of it needs no
/// precondition.
#[derive(Default)]
pub(crate) struct Blocks {
    blocks: Mutex<Vec<Candidate>>,
    /// Which list of blocks this is: bumped by every change to `blocks`, and by
    /// nothing else.
    ///
    /// Read and written only while the guard over `blocks` is held, which is
    /// what makes it a label rather than a second fact to race against: a
    /// reader copies the blocks and reads the revision without letting go, so
    /// the number it gets names the list it got. That is also why it is not
    /// behind a lock of its own — a second lock is a second instant.
    ///
    /// It is deliberately *not* [`Candidate::selection`]. That is the address
    /// selection the block was interpreted against, and it is unchanged by
    /// [`Self::admit`] — a block held at an unchanged selection leaves every
    /// `selection` in the list alone — so it cannot tell one list from another.
    /// The two answer different questions: `selection` says whether the facts
    /// must be interpreted again, this says whether the list is still the one
    /// some reader saw.
    revision: AtomicU64,
}

impl Blocks {
    fn entries(&self) -> Result<MutexGuard<'_, Vec<Candidate>>, IndexError> {
        self.blocks.lock().map_err(|_| {
            IndexError::new(IndexErrorKind::Store, "pending blocks are poisoned", false)
        })
    }

    /// Records that the list is no longer the one any earlier reader sampled.
    ///
    /// Every caller holds the guard over `blocks`, which is the whole of the
    /// ordering this needs: the mutex serialises the bump against every other
    /// bump and against [`Self::sampled`]'s read, so nothing here has to.
    fn changed(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    /// The newest block held above the line, or `None` when it is empty.
    pub(crate) fn tip(&self) -> Result<Option<BlockRef>, IndexError> {
        Ok(self
            .entries()?
            .last()
            .map(|entry| entry.canonical.block.reference().clone()))
    }

    /// Holds `canonical` above the line, as interpreted at `selection`.
    pub(crate) fn admit(
        &self,
        canonical: BlockWithOutputs,
        selection: u64,
    ) -> Result<(), IndexError> {
        let mut entries = self.entries()?;
        entries.push(Candidate {
            canonical,
            selection,
        });
        self.changed();
        Ok(())
    }

    /// The oldest held block, when it is at or below `line`.
    pub(crate) fn confirmed(
        &self,
        line: BlockPosition,
    ) -> Result<Option<BlockWithOutputs>, IndexError> {
        Ok(self
            .entries()?
            .first()
            .map(|entry| entry.canonical.clone())
            .filter(|canonical| canonical.block.reference().position <= line))
    }

    /// Drops the oldest held block once storage has it.
    pub(crate) fn committed(&self, block: &BlockRef) -> Result<(), IndexError> {
        let mut entries = self.entries()?;
        if entries
            .first()
            .is_some_and(|entry| entry.canonical.block.reference() == block)
        {
            entries.remove(0);
            self.changed();
            return Ok(());
        }
        Err(IndexError::new(
            IndexErrorKind::Store,
            "committed block is not the oldest pending block",
            false,
        ))
    }

    /// Discards every held block at or above `position`. This is the whole of
    /// a reorg shallower than the confirmed line.
    pub(crate) fn unwind(&self, position: BlockPosition) -> Result<(), IndexError> {
        let mut entries = self.entries()?;
        let before = entries.len();
        entries.retain(|entry| entry.canonical.block.reference().position < position);
        if entries.len() != before {
            self.changed();
        }
        Ok(())
    }

    /// Every held block, newest first, so a reorg can walk down to the fork.
    pub(crate) fn descending(&self) -> Result<Vec<BlockRef>, IndexError> {
        let mut blocks = self
            .entries()?
            .iter()
            .map(|entry| entry.canonical.block.reference().clone())
            .collect::<Vec<_>>();
        blocks.reverse();
        Ok(blocks)
    }

    /// Whether any held block was interpreted against an older address
    /// selection than `selection`, so all of them must be inspected again.
    pub(crate) fn outdated(&self, selection: u64) -> Result<bool, IndexError> {
        Ok(self
            .entries()?
            .iter()
            .any(|entry| entry.selection != selection))
    }

    /// Every held block, oldest first, leaving the collection as it is.
    ///
    /// Deliberately *not* a take. A re-inspection reads this, rebuilds the
    /// whole collection off to the side, and swaps it in with [`Self::replace`];
    /// emptying the shared collection first published a partial view of the
    /// blocks above the line for as long as the rebuild took — a pending
    /// transaction missing from a history page, an output a pending block
    /// spent offered as spendable again — and a rebuild that failed left that
    /// view standing until a later pass repaired it.
    pub(crate) fn ascending(&self) -> Result<Vec<BlockRef>, IndexError> {
        Ok(self
            .entries()?
            .iter()
            .map(|entry| entry.canonical.block.reference().clone())
            .collect())
    }

    /// Swaps in everything `rebuilt` holds, as one change under one lock.
    ///
    /// A reader sees either every block interpreted against the old selection
    /// or every block interpreted against the new one, and never a prefix of
    /// either.
    ///
    /// `rebuilt`'s own revision is discarded: the revision belongs to this
    /// collection's history of changes, not to the one that was assembled
    /// off to the side, and a re-inspection is a change even when it hands back
    /// the same blocks — what is inside them is interpreted against a
    /// different address selection.
    pub(crate) fn replace(&self, rebuilt: Self) -> Result<(), IndexError> {
        let rebuilt = rebuilt.blocks.into_inner().map_err(|_| {
            IndexError::new(
                IndexErrorKind::Store,
                "rebuilt pending blocks are poisoned",
                false,
            )
        })?;
        let mut entries = self.entries()?;
        *entries = rebuilt;
        self.changed();
        Ok(())
    }

    /// Every block above the line, oldest first, in commit order, with the
    /// revision that names exactly this list.
    ///
    /// Copied and labelled under one guard, so the revision a reader gets names
    /// the list it got. This is the only reading of the collection that leaves
    /// the index ([`Pending`]).
    pub(crate) fn sampled(&self) -> Result<Sample, IndexError> {
        let entries = self.entries()?;
        Ok(Sample::new(
            entries
                .iter()
                .map(|entry| entry.canonical.clone())
                .collect(),
            self.revision.load(Ordering::Relaxed),
        ))
    }
}

#[cfg(test)]
mod tests {
    use base::ChainName;

    use super::*;
    use crate::{BlockHash, BlockHeight, BlockWithOutputs, Network};

    fn held(position: u64) -> BlockWithOutputs {
        BlockWithOutputs::new(
            &Network::new(ChainName::new("test"), "mainnet"),
            BlockRef {
                position: BlockPosition(position),
                height: BlockHeight(position),
                hash: BlockHash(vec![position as u8]),
                parent: None,
                timestamp: None,
            },
            Vec::new(),
        )
        .expect("unique transaction ids")
    }

    fn positions(blocks: &[BlockWithOutputs]) -> Vec<u64> {
        blocks
            .iter()
            .map(|held| held.block.reference().position.0)
            .collect()
    }

    /// A store's answer, reduced to the line it was read at.
    struct ReadAt(Option<BlockRef>);

    impl Checkpointed for ReadAt {
        fn checkpoint(&self) -> Option<&BlockRef> {
            self.0.as_ref()
        }
    }

    /// The narrowing drops exactly the blocks the durable read already holds:
    /// the line itself and everything below it, and nothing above.
    ///
    /// Positions are sparse, so the line need not be a held position at all.
    #[test]
    fn a_sample_is_narrowed_to_the_blocks_above_the_line_it_is_given() {
        let pending = Blocks::default();
        for position in [4, 7, 9] {
            pending.admit(held(position), 0).expect("a held block");
        }
        let line = |position| held(position).block.reference().clone();
        let above = |checkpoint: Option<&BlockRef>| {
            let answer = ReadAt(checkpoint.cloned());
            positions(&pending.sampled().expect("a sample").above(&answer))
        };

        assert_eq!(above(None), [4, 7, 9], "no line, so nothing is held twice");
        assert_eq!(above(Some(&line(3))), [4, 7, 9]);
        assert_eq!(above(Some(&line(4))), [7, 9], "the line itself is stored");
        assert_eq!(above(Some(&line(8))), [9], "a position nothing was held at");
        assert!(above(Some(&line(9))).is_empty());
    }

    /// The revision names the list: every change to it moves the number, and a
    /// reading that changes nothing does not.
    ///
    /// A paginated read carries it between pages, so a change that left it
    /// alone would let two pages be folded over two different lists.
    #[test]
    fn every_change_to_the_held_blocks_moves_the_revision() {
        let pending = Blocks::default();
        let revision = || pending.sampled().expect("a sample").revision();
        let mut seen = vec![revision()];
        let mut changed = |what: &str| {
            let now = revision();
            assert!(!seen.contains(&now), "{what} left the revision alone");
            seen.push(now);
        };

        pending.admit(held(1), 0).expect("a held block");
        changed("holding a block");
        pending.admit(held(2), 0).expect("a second held block");
        changed("holding a second block");
        pending
            .committed(held(1).block.reference())
            .expect("the oldest block commits");
        changed("a commit");
        pending.unwind(BlockPosition(2)).expect("a reorg");
        changed("an unwind");
        let rebuilt = Blocks::default();
        rebuilt.admit(held(2), 1).expect("a re-interpreted block");
        pending.replace(rebuilt).expect("a re-inspection");
        changed("a re-inspection");

        assert_eq!(revision(), revision(), "reading is not a change");
        pending.unwind(BlockPosition(9)).expect("nothing to unwind");
        assert_eq!(
            Some(&revision()),
            seen.last(),
            "an unwind that discarded nothing is not a change either"
        );
    }
}
