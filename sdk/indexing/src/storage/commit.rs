//! What one commit carries, and the port that takes it.
//!
//! One operation, both directions. A pass reads blocks the network has not
//! committed yet *and* blocks it committed before an observation existed, and
//! the only difference between the two is whether the network's confirmed line
//! moves: a batch whose blocks extend the checkpoint advances it, a batch whose
//! blocks sit at or below it does not. That is a property of the blocks, so it
//! is derived from them rather than selected by the caller — which is why
//! [`IndexStore`] has one write and not two.
//!
//! The other half of a commit is the progress it proves. [`Progress`] advances
//! one observation's contiguous [`indexed_through`](crate::WatchProgress), in
//! the same transaction as the facts that justify it, so a claim can never
//! outlive the rows behind it.

use crate::{
    BlockRef, BlockWithOutputs, BoxFuture, FeeSettlement, IndexError, IndexErrorKind, Network,
    OutputKey,
};

/// How far one observation's inspection reached, and what the store must still
/// hold for the claim to be writable.
///
/// # Why `expected`, and why the store does not simply require progress to move
/// forward
///
/// The store compares `expected` against the value it holds and refuses the
/// whole batch if they differ. That is a compare-and-set supplied by whoever
/// read the progress, and it is the only honest guard here, because **a
/// monotonic rule would refuse correct writes**: a chunk whose outcome was lost
/// is redone under whatever budget the next pass has left, so it can settle
/// *less* than the write that actually landed did. A store refusing "it went
/// down" would wedge that observation for good, while a store comparing
/// `expected` refuses it once, retryably, and the index reloads the value the
/// store actually holds and carries on from there.
///
/// What *is* checked when a claim is built is the weaker and true statement: it
/// does not *retreat* from the value its own author read. Equality is therefore a
/// legal claim to build; only one going backwards from its own expectation is
/// refused, and that is a check on the index rather than a rule about the store's
/// history.
///
/// Legal to *build* is not the same as accepted twice. A batch that carried a
/// claim and landed has made its own `expected` stale, so presenting that exact
/// batch again is refused, retryably, by the compare above — see
/// [`IndexStore::commit`] for what recovery actually is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    /// The observation this advances, by its own identity ([`crate::Watch::id`]).
    pub watch_id: String,
    /// The `indexed_through` the store must still hold, or `None` when the
    /// author believes nothing has been recorded for this observation yet.
    pub expected: Option<BlockRef>,
    /// The block through which every applicable block from the observation's
    /// birthday has now been inspected for it, blocks with no matching activity
    /// included.
    ///
    /// A block the batch can point at, and checked as one by
    /// [`IndexedBatch::new`]: one of the batch's own blocks, or the checkpoint
    /// it leaves behind. Its position alone is not enough — that is all the
    /// store compares, so a reference with a position that exists and a hash and
    /// height that never did would be written as durable evidence about a block
    /// nothing produced.
    pub indexed_through: BlockRef,
}

/// One atomic write: the blocks an index inspected, and the progress they prove.
///
/// Built by [`IndexedBatch::new`], which validates everything a store would
/// otherwise have to re-check — the blocks belong to the network, they ascend,
/// the ones above the checkpoint chain onto it, and no claim reaches above the
/// line the batch leaves behind.
///
/// **What that validation does not prove, stated because it used to be assumed.**
/// It says nothing about the batch's *effect*. It does not decide which outputs
/// survive it, it does not reconcile a creation in one block against a spend in
/// a later one, and it does not narrow what reaches the store to a net result.
/// This type used to do exactly that, through a fold that discarded a creation
/// and its later spend together on the grounds that an output born and spent
/// inside one batch "never exists" — which was true of the live set and false of
/// the *record*. Discarding the spend threw away the one fact that makes a
/// replay of the creating block safe, so re-presenting that block later put the
/// output back as spendable: the defect `unmatched_spend` had been introduced to
/// prevent, reintroduced by the fold that was supposed to make it unnecessary.
///
/// There was nothing to buy with it. A store applies a block's creations and
/// then its spends, in block order, inside one transaction, and arrives at the
/// same live set while keeping the spend — so the batch carries each block's
/// facts as the chain reported them and decides nothing about them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedBatch {
    network: Network,
    expected_checkpoint: Option<BlockRef>,
    checkpoint: Option<BlockRef>,
    blocks: Vec<BlockWithOutputs>,
    progress: Vec<Progress>,
    settlement: FeeSettlement,
}

impl IndexedBatch {
    /// Validates one batch against the checkpoint it was planned under.
    ///
    /// `blocks` are canonical facts in strictly ascending position order. Those
    /// at or below `expected_checkpoint` fill in a block the network already
    /// committed, for an observation that was not being watched at the time;
    /// those above it extend the line and must chain onto it, block by block.
    /// The highest of them becomes the new checkpoint, and a batch with none is
    /// a write that leaves the line exactly where it was.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::NetworkMismatch`] for a block of another network,
    /// [`IndexErrorKind::InvalidBlock`] for blocks that do not ascend, do not
    /// chain onto the checkpoint, or for progress that claims a block above the
    /// line this batch leaves behind, retreats from its own expectation, or
    /// names a block neither `blocks` nor the resulting checkpoint holds,
    /// [`IndexErrorKind::Disconnected`] when a block above the checkpoint does
    /// not extend it, and [`IndexErrorKind::InvalidRequest`] for two claims on
    /// one observation.
    pub fn new(
        network: Network,
        expected_checkpoint: Option<BlockRef>,
        blocks: Vec<BlockWithOutputs>,
        progress: Vec<Progress>,
        settlement: FeeSettlement,
    ) -> Result<Self, IndexError> {
        let checkpoint = Self::line(&network, expected_checkpoint.as_ref(), &blocks)?;
        Self::claims(checkpoint.as_ref(), &blocks, &progress)?;
        Ok(Self {
            network,
            expected_checkpoint,
            checkpoint,
            blocks,
            progress,
            settlement,
        })
    }

    /// Where the chain that produced these blocks settles its network fee.
    ///
    /// It travels with the batch because it is consulted at **commit** time
    /// now: a chain that charges its fee beside the movements writes one debit
    /// row for the payer, and a chain whose fee is already inside the
    /// difference between its spent inputs and its created outputs writes none.
    /// Stating it here rather than at every read is what turns the rule into a
    /// stored fact, so no reader can be told the wrong one and no reader has to
    /// be told at all.
    #[must_use]
    pub const fn settlement(&self) -> FeeSettlement {
        self.settlement
    }

    /// The line this batch leaves behind, after checking every block against the
    /// one below it.
    fn line(
        network: &Network,
        expected: Option<&BlockRef>,
        blocks: &[BlockWithOutputs],
    ) -> Result<Option<BlockRef>, IndexError> {
        let mut line = expected.cloned();
        let mut previous = None;
        for canonical in blocks {
            let block = canonical.block.reference();
            if canonical.block.network() != network {
                return Err(IndexError::new(
                    IndexErrorKind::NetworkMismatch,
                    "an indexed block belongs to another network",
                    false,
                ));
            }
            canonical.validate(network)?;
            if previous.is_some_and(|previous| block.position <= previous) {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "indexed blocks are not in ascending position order",
                    false,
                ));
            }
            previous = Some(block.position);
            // At or below the checkpoint the block is already committed and this
            // is filling it in for an observation it was not inspected for, so
            // there is no linkage to check: the unchanged checkpoint is the whole
            // of what authorises the write. Above it, the block must be the next
            // one, which is what makes the line contiguous.
            let filled = expected
                .is_some_and(|line| block.position <= line.position && block.height <= line.height);
            if filled {
                continue;
            }
            canonical.connects_to(line.as_ref())?;
            line = Some(block.clone());
        }
        Ok(line)
    }

    /// Rejects a claim no batch can justify: one that names an observation
    /// twice, and one this batch cannot support.
    fn claims(
        checkpoint: Option<&BlockRef>,
        blocks: &[BlockWithOutputs],
        progress: &[Progress],
    ) -> Result<(), IndexError> {
        for (index, claim) in progress.iter().enumerate() {
            if progress[..index]
                .iter()
                .any(|held| held.watch_id == claim.watch_id)
            {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "one observation is advanced twice in one batch",
                    false,
                ));
            }
        }
        progress
            .iter()
            .try_for_each(|claim| Self::justified(checkpoint, blocks, claim))
    }

    /// Rejects one claim this batch cannot support: above the line it leaves
    /// behind, below its own expectation, or naming a block it cannot point at.
    fn justified(
        checkpoint: Option<&BlockRef>,
        blocks: &[BlockWithOutputs],
        claim: &Progress,
    ) -> Result<(), IndexError> {
        // Progress is persisted only through the confirmed boundary: above it a
        // block is held in memory and a fork is a truncation, so a durable claim
        // to have inspected one could outlive the block.
        if checkpoint.is_none_or(|line| claim.indexed_through.position > line.position) {
            return Err(unjustified(
                "indexed progress claims a block above the confirmed line",
            ));
        }
        // Below its own expectation, never merely equal to it: a claim that
        // settles exactly what its author already read is a legal thing to
        // build. What cannot be justified is one that *retreats* from that
        // value. Whether the store then accepts it is a separate question — see
        // `IndexStore::commit`, "Recovery is reload-and-redo".
        if claim
            .expected
            .as_ref()
            .is_some_and(|held| claim.indexed_through.position < held.position)
        {
            return Err(unjustified(
                "indexed progress goes below the value it expects",
            ));
        }
        // A position is not a block, and a position is all every check above
        // compares — the store's own compare-and-set included. So a claim naming
        // a position that exists under a hash and a height that never did passes
        // all of them, and is written as durable evidence about a block nothing
        // produced. What this batch can point at is one of the blocks it carries,
        // which is the claim's own evidence, or the line it leaves behind, which
        // is a block the store already holds.
        if !Self::supports(checkpoint, blocks, &claim.indexed_through) {
            return Err(unjustified(
                "indexed progress names a block this batch does not carry",
            ));
        }
        Ok(())
    }

    /// Whether this batch can point at `block` by identity, rather than at a
    /// position that happens to match one.
    fn supports(
        checkpoint: Option<&BlockRef>,
        blocks: &[BlockWithOutputs],
        block: &BlockRef,
    ) -> bool {
        checkpoint == Some(block)
            || blocks
                .iter()
                .any(|canonical| canonical.block.reference() == block)
    }

    /// The network every block and claim in the batch belongs to.
    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.network
    }

    /// The checkpoint the store must still hold (compare-and-set), or `None`
    /// when the batch was planned before the network's first block.
    ///
    /// A value, not a condition a backend evaluates in SQL: the SQLite design is
    /// one `BEGIN IMMEDIATE` per operation, so a store reads its own checkpoint
    /// inside the write transaction and compares it. A `WHERE position = ? AND
    /// hash = ?` would be unsound anyway — that pair is not monotonic across a
    /// rescan, so it matches again after exactly the purge-then-rescan it would
    /// be introduced to refuse. What rules
    /// that out is that [`crate::Purging::purge_network`] is contractually
    /// offline.
    #[must_use]
    pub const fn expected_checkpoint(&self) -> Option<&BlockRef> {
        self.expected_checkpoint.as_ref()
    }

    /// The checkpoint the store holds after this batch: the highest block that
    /// extended the line, or the expected one when nothing did.
    #[must_use]
    pub const fn checkpoint(&self) -> Option<&BlockRef> {
        self.checkpoint.as_ref()
    }

    /// The batch's blocks, ascending.
    #[must_use]
    pub fn blocks(&self) -> &[BlockWithOutputs] {
        &self.blocks
    }

    /// The claims to record in the same transaction as the blocks.
    #[must_use]
    pub fn progress(&self) -> &[Progress] {
        &self.progress
    }
}

/// A claim the batch it rides on cannot justify. Never retryable: the write is
/// malformed, and presenting it again produces the same batch.
fn unjustified(message: &str) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidBlock, message, false)
}

/// Required atomic indexing writes and checkpoint reads, beside [`crate::WatchStore`].
/// Committed facts are append-only during synchronization; offline removal is the
/// separate [`crate::Purging`] capability. Balance, history and output reads are optional.
///
/// Every mutation reads its expected checkpoint and conditional rows inside the
/// same transaction that writes them. Every optional query reads its answer and
/// checkpoint in one snapshot. Consumers sample pending blocks before that read,
/// then narrow them by the returned checkpoint using [`crate::PendingSample::above`];
/// splitting the snapshot could omit a block or count it twice.
pub trait IndexStore: Send + Sync {
    /// The network's current checkpoint, or `None` before its first commit.
    ///
    /// The network's durable line, not its tip: blocks above it are held in
    /// memory by whatever is indexing, and reported separately. It is a
    /// per-network fact and stays one — the confirmed/pending boundary, the
    /// compare-and-set base of [`Self::commit`], the top of every observation's
    /// own catch-up, and the anchor a paginated read pins itself to. Per-watch
    /// progress is recorded *beside* it and does not replace it.
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;

    /// Atomically writes one batch's history, credits and live-output changes,
    /// advances the observations it proves, and moves the checkpoint to
    /// [`IndexedBatch::checkpoint`].
    ///
    /// Answers the required spends it could not match to a live output, in block
    /// order; each names an address whose history is provably short. The
    /// checkpoint is read and compared inside the same transaction, and a batch
    /// planned under a different one is refused whole and retryably.
    ///
    /// # Insert-or-verify, one rule for the whole batch
    ///
    /// A row the store already holds is **verified** against the one being
    /// written and kept, not refused: a newly observed address routinely already
    /// has rows in its own past, because a history row is written for every
    /// endpoint of every movement. What is refused is the thing that cannot
    /// honestly happen — one `(address, height, transaction)` with two contents —
    /// which is corruption and terminal.
    ///
    /// That rule is stated in terms of rows because the stores here keep one per
    /// `(address, height, transaction)`. **A store that keeps no such row owes
    /// the same idempotency by other means**: whatever it folds — a counter, an
    /// aggregate, an outbound message — must survive the same batch's blocks
    /// arriving twice.
    ///
    /// # Recovery is reload-and-redo, not verbatim retry
    ///
    /// This is stated here because it is easy to read the rule above as "an
    /// uncertain write is resolved by presenting the batch again", and that is
    /// **not what these stores implement**. The blocks of a batch are idempotent,
    /// so re-presenting them is harmless. The *claims* are not: a claim advances
    /// the observation it names, so the first landed commit has itself moved the
    /// `expected` the second presentation compares, and the exact same batch a
    /// second time is refused with a retryable
    /// [`IndexErrorKind::Conflict`] rather than accepted as a no-op. A producer
    /// whose commit's outcome it never saw recovers by reading the progress the
    /// store now holds and building the batch again against it — which is what
    /// the built-in driver does, and what
    /// `tests/contracts/backfill.rs`'s
    /// `BackfillContract::redoes_a_batch_without_duplicating_it`
    /// asserts of every store here. A batch carrying no claims at all *is*
    /// idempotent verbatim.
    ///
    /// # Progress is compared, then written
    ///
    /// Each [`Progress`] entry carries the `indexed_through` its author believed
    /// the store held. The store reads what it holds, inside this transaction,
    /// and refuses the **whole batch** with a retryable
    /// [`IndexErrorKind::Conflict`] if any of them differs. A batch naming an
    /// observation the store does not hold is refused the same way. That
    /// comparison is what makes one owner's belief checkable without forbidding
    /// a redone batch from settling less than a lost one did; see [`Progress`].
    ///
    /// # What this asks of a store that holds no outputs
    ///
    /// The answered spends are **not** optional, and this is the one obligation
    /// here that a store cannot discharge by holding fewer rows. Answering an
    /// empty vector is legal Rust and silently wrong on a UTXO chain: it claims
    /// every required spend matched an output this store held, which is what an
    /// address watched from above the block that funded it does not do, and
    /// nothing downstream can tell the difference. A store that keeps no
    /// live-output set must either track the outpoints
    /// [`crate::OutputChanges::spent`] names well enough to answer it, or be used
    /// only on chains whose interpreter produces no output changes at all — every
    /// supported account chain, where the collection is always empty and an empty
    /// answer is the true one.
    ///
    /// # Errors
    ///
    /// A retryable [`IndexErrorKind::Conflict`] when the checkpoint moved, a
    /// claim did not match, or the batch names an unknown observation;
    /// [`IndexErrorKind::Store`] for a row that disagrees with a stored one, and
    /// the store's own error.
    fn commit<'a>(
        &'a self,
        batch: IndexedBatch,
    ) -> BoxFuture<'a, Result<Vec<OutputKey>, IndexError>>;
}

/// An index store held behind a reference count is an index store.
///
/// The other half of what makes a custom sink usable without being [`Clone`];
/// see [`crate::WatchStore`]'s own forwarding impl.
impl<T: IndexStore + ?Sized> IndexStore for std::sync::Arc<T> {
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        (**self).checkpoint(network)
    }

    fn commit<'a>(
        &'a self,
        batch: IndexedBatch,
    ) -> BoxFuture<'a, Result<Vec<OutputKey>, IndexError>> {
        (**self).commit(batch)
    }
}

#[cfg(test)]
mod tests {
    use base::ChainName;

    use super::*;
    use crate::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockWithOutputs};

    fn network() -> Network {
        Network::new(ChainName::new("test"), "mainnet")
    }

    fn block(height: u64) -> BlockRef {
        BlockRef {
            position: BlockPosition(height),
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8]),
            parent: height.checked_sub(1).map(|value| BlockParent {
                position: BlockPosition(value),
                hash: BlockHash(vec![value as u8]),
            }),
            timestamp: None,
        }
    }

    /// The facts of an empty block at `height`.
    fn canonical(height: u64) -> BlockWithOutputs {
        BlockWithOutputs::new(&network(), block(height), Vec::new())
            .expect("unique transaction ids")
    }

    fn advance(watch: &str, expected: Option<BlockRef>, through: u64) -> Progress {
        Progress {
            watch_id: watch.to_owned(),
            expected,
            indexed_through: block(through),
        }
    }

    #[test]
    fn a_block_above_the_checkpoint_must_extend_it() {
        let mut reference = block(2);
        reference.parent = Some(BlockParent {
            position: BlockPosition(1),
            hash: BlockHash(vec![99]),
        });
        let forked =
            BlockWithOutputs::new(&network(), reference, Vec::new()).expect("no transactions");

        let error = IndexedBatch::new(
            network(),
            Some(block(1)),
            vec![forked],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect_err("a disconnected block");

        assert_eq!(error.kind, IndexErrorKind::Disconnected);
        assert!(error.retryable);
    }

    /// A batch validates every block it carries, so facts that never passed
    /// [`BlockWithOutputs::validate`] cannot reach a store: here a transaction
    /// that names another block than the one carrying it.
    #[test]
    fn a_batch_refuses_a_block_whose_facts_are_invalid() {
        let stray = crate::Transaction {
            id: crate::TransactionRef::new(network(), "stray"),
            movements: Vec::new(),
            fee: None,
            order: 0,
            status: crate::Status::Included { block: block(2) },
        };
        let facts = BlockWithOutputs::new(&network(), block(1), vec![stray]).expect("one id");

        let error = IndexedBatch::new(
            network(),
            None,
            vec![facts],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect_err("a transaction of another block");

        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert_eq!(error.message, "transaction names another block");
    }

    /// The line a batch leaves behind is derived from its blocks: one that only
    /// fills in blocks the network already holds moves nothing, and one that
    /// extends the line moves it to its highest block.
    #[test]
    fn the_line_after_a_batch_is_whatever_its_blocks_make_it() {
        let filling = IndexedBatch::new(
            network(),
            Some(block(5)),
            vec![canonical(2), canonical(3)],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("two blocks below the checkpoint");
        assert_eq!(filling.checkpoint(), Some(&block(5)));
        assert_eq!(filling.expected_checkpoint(), Some(&block(5)));

        let extending = IndexedBatch::new(
            network(),
            Some(block(5)),
            vec![canonical(6), canonical(7)],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("two blocks above it");
        assert_eq!(extending.checkpoint(), Some(&block(7)));

        let first = IndexedBatch::new(
            network(),
            None,
            vec![canonical(0), canonical(1)],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("the network's first blocks");
        assert_eq!(first.checkpoint(), Some(&block(1)));

        // The block that *is* the checkpoint is a fill, not an extension: a
        // block already committed is inspected again only for an observation it
        // was not inspected for, and it moves nothing.
        let replayed = IndexedBatch::new(
            network(),
            Some(block(5)),
            vec![canonical(5)],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("the block that is already the line");
        assert_eq!(replayed.checkpoint(), Some(&block(5)));
    }

    #[test]
    fn blocks_must_ascend_and_belong_to_the_network() {
        let error = IndexedBatch::new(
            network(),
            Some(block(5)),
            vec![canonical(3), canonical(2)],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect_err("descending blocks");
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);

        let elsewhere = BlockWithOutputs::new(
            &Network::new(ChainName::new("other"), "mainnet"),
            block(2),
            Vec::new(),
        )
        .expect("unique transaction ids");
        let error = IndexedBatch::new(
            network(),
            Some(block(1)),
            vec![elsewhere],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect_err("a foreign block");
        assert_eq!(error.kind, IndexErrorKind::NetworkMismatch);
    }

    /// A claim is checked against the line the batch leaves behind and against
    /// the value its own author read — never against the store's history, which
    /// this type cannot see and which a redone batch may legitimately settle less
    /// of.
    #[test]
    fn a_claim_stays_below_the_line_and_never_retreats_from_its_expectation() {
        IndexedBatch::new(
            network(),
            Some(block(5)),
            Vec::new(),
            vec![advance("w", Some(block(3)), 5)],
            FeeSettlement::Charged,
        )
        .expect("a claim up to the line itself");
        IndexedBatch::new(
            network(),
            Some(block(5)),
            Vec::new(),
            vec![advance("w", Some(block(5)), 5)],
            FeeSettlement::Charged,
        )
        .expect("a claim settling exactly what its author read is legal to build");

        for (claim, message) in [
            (
                advance("w", None, 6),
                "indexed progress claims a block above the confirmed line",
            ),
            (
                advance("w", Some(block(5)), 4),
                "indexed progress goes below the value it expects",
            ),
        ] {
            let error = IndexedBatch::new(
                network(),
                Some(block(5)),
                Vec::new(),
                vec![claim],
                FeeSettlement::Charged,
            )
            .expect_err(message);
            assert_eq!(
                error,
                IndexError::new(IndexErrorKind::InvalidBlock, message, false)
            );
        }

        let error = IndexedBatch::new(
            network(),
            Some(block(5)),
            Vec::new(),
            vec![advance("w", None, 4), advance("w", Some(block(4)), 5)],
            FeeSettlement::Charged,
        )
        .expect_err("one observation advanced twice");
        assert_eq!(error.kind, IndexErrorKind::InvalidRequest);

        let error = IndexedBatch::new(
            network(),
            None,
            Vec::new(),
            vec![advance("w", None, 1)],
            FeeSettlement::Charged,
        )
        .expect_err("a claim with no line at all");
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
    }

    /// A claim must name a block the batch can point at, by identity and not by
    /// position: one of the blocks it carries, or the line it leaves behind.
    ///
    /// Position alone is not an identity. A claim at the checkpoint's position
    /// carrying another block's hash and another block's height is a reference
    /// to a block that does not exist, and no store can detect it — the
    /// position it compares is the right one.
    #[test]
    fn a_claim_names_a_block_this_batch_can_point_at() {
        let impossible = BlockRef {
            hash: BlockHash(vec![99]),
            height: BlockHeight(999),
            ..block(10)
        };
        let error = IndexedBatch::new(
            network(),
            Some(block(10)),
            Vec::new(),
            vec![Progress {
                watch_id: "w".to_owned(),
                expected: None,
                indexed_through: impossible,
            }],
            FeeSettlement::Charged,
        )
        .expect_err("a block reference nothing in this batch supports");
        assert_eq!(
            error,
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "indexed progress names a block this batch does not carry",
                false,
            )
        );

        // The two references a batch does support: a block it carries, and the
        // line it leaves behind.
        IndexedBatch::new(
            network(),
            Some(block(10)),
            vec![canonical(4)],
            vec![advance("w", None, 4)],
            FeeSettlement::Charged,
        )
        .expect("a claim on a block the batch carries");
        IndexedBatch::new(
            network(),
            Some(block(10)),
            Vec::new(),
            vec![advance("w", None, 10)],
            FeeSettlement::Charged,
        )
        .expect("a claim on the line the batch leaves behind");
    }
}
