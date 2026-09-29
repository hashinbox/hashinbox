//! The blocks above the confirmed line: fetched, interpreted, not committed.

use std::{
    collections::BTreeSet,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    Address, BlockPosition, BlockRef, CanonicalBlock, Decimal, Holder, IndexError, IndexErrorKind,
    IndexedOutput, OutputKey, PendingValue,
};

/// One block above the confirmed line: validated against its predecessor,
/// ready to commit, and not committed.
struct HeldBlock {
    /// The validated facts this block will commit, with no precondition of its
    /// own: which checkpoint authorises the write is settled where the write
    /// happens, not here (`crate::IndexedBatch`).
    facts: CanonicalBlock,
    /// The address-filter selection the facts were produced against.
    selection: u64,
}

/// One reading of the blocks above the line, and the revision naming it.
///
/// Every read folds a sample rather than the overlay itself, for the ordering
/// [`Sample::above`] states. A read that spans more than one page carries
/// `revision` in its cursor, so a later page is folded over the same blocks or
/// refused.
pub(crate) struct Sample {
    /// The blocks, oldest first, in commit order.
    pub(crate) blocks: Vec<CanonicalBlock>,
    /// Which revision of the held list these blocks are.
    pub(crate) revision: u64,
}

impl Sample {
    /// The sampled blocks a durable read at `checkpoint` does not already hold.
    ///
    /// Every answer that folds this sample is two reads — the store, and the
    /// blocks the same index holds above the confirmed line — and a commit can
    /// land between them, moving one block out of the second and into the
    /// first. Two rules together make that harmless, and both are needed:
    ///
    /// 1. the overlay is sampled **first**, so the store read can only be
    ///    newer than the sample and everything the store gained in between is
    ///    a block the sample still holds;
    /// 2. the sample is then narrowed to what the store's own checkpoint says
    ///    the store does not hold, which drops exactly those blocks.
    ///
    /// Each rule alone is a defect. Reading the store first loses a block
    /// committed in between altogether: its spends stop applying, so outputs
    /// it spent are offered as spendable again, and the outputs it created
    /// disappear. Sampling first and narrowing nothing counts that block
    /// twice, because the store and the sample both hold it while the commit
    /// is halfway done. With both, the answer is the state at the moment the
    /// overlay was sampled, and confirmed and pending stay disjoint.
    ///
    /// The narrowing is the sample's own, taken with the checkpoint the store's
    /// answer carried, so no read can narrow by a line other than the one its
    /// rows were read at.
    pub(crate) fn above(self, checkpoint: Option<&BlockRef>) -> Vec<CanonicalBlock> {
        let Some(line) = checkpoint else {
            return self.blocks;
        };
        self.blocks
            .into_iter()
            .filter(|facts| facts.block().position > line.position)
            .collect()
    }
}

/// The blocks above the confirmed line, oldest first.
///
/// This is not a second store. It is the blocks synchronization already
/// fetched and already interpreted, held instead of thrown away, so the
/// durable store only ever receives blocks that cannot revert while every read
/// can still answer "what is on its way" (`docs/design/030 §8 (ex-025)`). A reorg shallower
/// than the line is [`Frontier::unwind`] — no store I/O and no backend code at
/// all.
///
/// One payload, one bound: the interpreted facts ([`CanonicalBlock`]), which
/// cover every block above the line unconditionally and are proportional to the
/// watched addresses those blocks touch, usually nothing. The native blocks
/// those facts were read from are not here — they are megabytes, they are the
/// chain's type rather than this crate's, and only the fetching path ever
/// wants one again, so [`crate::fetch::NativeBlocks`] keeps its own bounded
/// cache of them. Neither is the checkpoint a commit of one is conditional on:
/// what this collection holds is facts, and a reader of it needs no
/// precondition (`docs/design/030 §8 (ex-032)`, the model's `Blocks` row).
#[derive(Default)]
pub(crate) struct Frontier {
    blocks: Mutex<Vec<HeldBlock>>,
    /// Which list of blocks this is: bumped by every change to `blocks`, and by
    /// nothing else.
    ///
    /// Read and written only while the guard over `blocks` is held, which is
    /// what makes it a label rather than a second fact to race against: a
    /// reader copies the blocks and reads the revision without letting go, so
    /// the number it gets names the list it got. That is also why it is not
    /// behind a lock of its own — a second lock is a second instant.
    ///
    /// It is deliberately *not* [`HeldBlock::selection`]. That is the address
    /// filter the facts were interpreted against, and it is unchanged by
    /// [`Self::admit`] — a block held at an unchanged selection leaves every
    /// `selection` in the list alone — so it cannot tell one list from another.
    /// The two answer different questions: `selection` says whether the facts
    /// must be interpreted again, this says whether the list is still the one
    /// some reader saw.
    revision: AtomicU64,
}

impl Frontier {
    fn entries(&self) -> Result<MutexGuard<'_, Vec<HeldBlock>>, IndexError> {
        self.blocks.lock().map_err(|_| {
            IndexError::new(IndexErrorKind::Store, "pending overlay is poisoned", false)
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
            .map(|entry| entry.facts.block().clone()))
    }

    /// Holds `facts` above the line, as interpreted at `selection`.
    pub(crate) fn admit(&self, facts: CanonicalBlock, selection: u64) -> Result<(), IndexError> {
        let mut entries = self.entries()?;
        entries.push(HeldBlock { facts, selection });
        self.changed();
        Ok(())
    }

    /// The oldest held block, when it is at or below `line`.
    pub(crate) fn confirmed(
        &self,
        line: BlockPosition,
    ) -> Result<Option<CanonicalBlock>, IndexError> {
        Ok(self
            .entries()?
            .first()
            .map(|entry| entry.facts.clone())
            .filter(|facts| facts.block().position <= line))
    }

    /// Drops the oldest held block once storage has it.
    pub(crate) fn committed(&self, block: &BlockRef) -> Result<(), IndexError> {
        let mut entries = self.entries()?;
        if entries
            .first()
            .is_some_and(|entry| entry.facts.block() == block)
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
        entries.retain(|entry| entry.facts.block().position < position);
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
            .map(|entry| entry.facts.block().clone())
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

    /// Every held block, oldest first, leaving the overlay as it is.
    ///
    /// Deliberately *not* a take. A re-inspection reads this, rebuilds the
    /// whole overlay off to the side, and swaps it in with [`Self::replace`];
    /// emptying the shared collection first published a partial view of the
    /// blocks above the line for as long as the rebuild took — a pending
    /// transaction missing from a history page, an output a pending block
    /// spent offered as spendable again — and a rebuild that failed left that
    /// view standing until a later pass repaired it.
    pub(crate) fn ascending(&self) -> Result<Vec<BlockRef>, IndexError> {
        Ok(self
            .entries()?
            .iter()
            .map(|entry| entry.facts.block().clone())
            .collect())
    }

    /// Swaps in everything `rebuilt` holds, as one change under one lock.
    ///
    /// A reader sees either every block interpreted against the old selection
    /// or every block interpreted against the new one, and never a prefix of
    /// either.
    ///
    /// `rebuilt`'s own revision is discarded: the revision belongs to this
    /// overlay's history of changes, not to the collection that was assembled
    /// off to the side, and a re-inspection is a change even when it hands back
    /// the same blocks — the facts inside them are interpreted against a
    /// different address selection.
    pub(crate) fn replace(&self, rebuilt: Self) -> Result<(), IndexError> {
        let rebuilt = rebuilt.blocks.into_inner().map_err(|_| {
            IndexError::new(
                IndexErrorKind::Store,
                "rebuilt pending overlay is poisoned",
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
    /// One owner folds this into every answer — history, balance, and UTXO
    /// selection — so the three reads cannot disagree about what is pending.
    pub(crate) fn sampled(&self) -> Result<Sample, IndexError> {
        let entries = self.entries()?;
        Ok(Sample {
            blocks: entries.iter().map(|entry| entry.facts.clone()).collect(),
            revision: self.revision.load(Ordering::Relaxed),
        })
    }
}

/// The fold itself stays with the overlay it reads; the value is
/// [`crate::History`]'s, so the type lives beside that trait.
impl PendingValue {
    /// The value `of`'s address and asset move in `blocks`.
    ///
    /// Deliberately not [`crate::Effect`]'s netting fold: a movement between
    /// two of the holder's own endpoints is reported as both arriving and
    /// leaving here, because the question above the line is what is moving,
    /// where a balance's question is what the total will be.
    ///
    /// A block credit counts as arriving, for the same reason it counts in
    /// the settled fold: it is value on its way to the address. It is the one
    /// thing reported here that the pending half of a *history* page cannot
    /// show, because that page lists transactions and a credit is not one
    /// (`docs/design/021` section 4.4). Leaving it out instead would understate
    /// an address for the whole depth of the confirmed line, which is the
    /// silent drift this record exists to stop.
    pub(crate) fn fold(of: &dyn Holder, blocks: &[CanonicalBlock]) -> Self {
        let movements = blocks
            .iter()
            .flat_map(CanonicalBlock::transactions)
            .flat_map(|transaction| &transaction.movements)
            .filter(|movement| &movement.asset == of.asset());
        let (mut incoming, mut outgoing) = (Vec::new(), Vec::new());
        for movement in movements {
            let at = |endpoint: Option<&Address>| endpoint == Some(of.address());
            incoming.extend(at(movement.to()).then(|| movement.amount.clone()));
            outgoing.extend(at(movement.from()).then(|| movement.amount.clone()));
        }
        let credits = blocks
            .iter()
            .flat_map(CanonicalBlock::credits)
            .filter(|credit| &credit.asset == of.asset() && &credit.to == of.address());
        incoming.extend(credits.map(|credit| credit.amount.clone()));
        Self {
            incoming: Decimal::total(&incoming),
            outgoing: Decimal::total(&outgoing),
            block: blocks.last().map(|facts| facts.block().clone()),
        }
    }
}

/// What the blocks above the line change about one address's live outputs.
///
/// Selection folds this over the confirmed table rather than filtering that
/// table by depth. A read-time depth filter would be actively wrong: a spend
/// of an old output is already applied while the change it created would be
/// hidden, and Bitcoin's outbox infers "spent" from absence, so the wallet
/// would free the input, see no change, and report a hole. Both halves move
/// together here because they come from the same blocks.
pub(crate) struct LiveChanges {
    /// Outputs those blocks created for the address and no later one spent.
    pub(crate) created: Vec<IndexedOutput>,
    /// Keys those blocks spent, whether or not the store holds them.
    pub(crate) spent: BTreeSet<OutputKey>,
}

impl LiveChanges {
    /// The net effect of `blocks` on `address`'s live outputs.
    pub(crate) fn fold(address: &Address, blocks: &[CanonicalBlock]) -> Self {
        let mut created = Vec::new();
        let mut spent = BTreeSet::new();
        for changes in blocks.iter().map(CanonicalBlock::outputs) {
            for key in changes.spent.iter().chain(&changes.tracked_spends) {
                spent.insert(key.clone());
            }
            created.retain(|output: &IndexedOutput| !spent.contains(&output.key()));
            created.extend(
                changes
                    .created
                    .iter()
                    .filter(|output| output.address == *address)
                    .cloned(),
            );
        }
        Self { created, spent }
    }
}

#[cfg(test)]
mod tests {
    use base::ChainName;

    use crate::{
        AssetId, BlockCredit, BlockHash, BlockHeight, BlockPosition, BlockRef, CanonicalBlock,
        Decimal, InterpretedBlock, MovementId, Network, OutputChanges, PendingValue, Subject,
    };

    /// A credit above the confirmed line is value on its way, so it is
    /// reported as pending rather than withheld until the block settles.
    ///
    /// Left out, an address would read short for the whole depth of the line —
    /// twelve blocks on a typical Ethereum deployment — with nothing saying
    /// so, which is the drift `docs/design/021` section 4.1 exists to stop.
    #[test]
    fn a_block_credit_above_the_line_is_pending_incoming() {
        let network = Network::new(ChainName::new("test"), "mainnet");
        let asset = AssetId::native(network.chain.clone());
        let holder = Subject {
            address: crate::Address::new(network.clone(), "beneficiary"),
            asset: asset.clone(),
        };
        let interpreted = InterpretedBlock {
            block: BlockRef {
                position: BlockPosition(1),
                height: BlockHeight(1),
                hash: BlockHash(vec![1]),
                parent: None,
                timestamp: None,
            },
            transactions: Vec::new(),
            outputs: OutputChanges::default(),
            credits: vec![BlockCredit {
                id: MovementId("withdrawal:1".into()),
                asset,
                amount: Decimal::from(2_u64),
                to: holder.address.clone(),
            }],
        };
        let held = vec![CanonicalBlock::new(network, interpreted).expect("a valid block")];

        let pending = PendingValue::fold(&holder, &held);

        assert_eq!(pending.incoming, Decimal::from(2_u64));
        assert_eq!(pending.outgoing, Decimal::zero());
    }
}
