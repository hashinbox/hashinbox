//! Canonical blocks, their atomic commit, and the offline rescan.
//!
//! # The compare-and-set, and why the one `docs/design/030 §8 (ex-031)` proposed is wrong
//!
//! Section 5.2 proposes `UPDATE checkpoint SET ... WHERE chain = ? AND
//! network = ? AND position = ? AND hash = ?` plus a row-count check, on the
//! grounds that naming the checkpoint's own durable state cannot be reset by a
//! delete the way a per-key version counter can. The first half is true and the
//! conclusion does not follow. A version counter is *monotonic*: the value a
//! reader saw is never produced again. `(position, hash)` is not — it is the
//! chain's own coordinate, and the entire purpose of `purge_network` is that
//! the network is re-indexed from the same chain afterwards, which restores the
//! same `(position, hash)` at the same height. A condition on those two columns
//! therefore matches again after exactly the sequence it was introduced to
//! refuse. `blocks_test::the_proposed_checkpoint_condition_cannot_see_an_aba`
//! is that sequence, written out.
//!
//! What this adapter does instead is not a better condition. It is no condition
//! at all: [`super::Database::transaction`] begins `IMMEDIATE`, so the
//! checkpoint read and the writes it authorizes are inside one transaction that
//! holds the file's single write lock for its whole length. A version counter
//! is needed only when the read and the commit are two transactions with a
//! window between them; this removes the window rather than guarding it, and
//! with it the whole condition vocabulary.

use rusqlite::Transaction;

use super::{Repository, Tables, conflict, tables::Place};
use crate::{
    BlockRef, BoxFuture, CanonicalBlock, CanonicalTransaction, FeeSettlement, IndexError,
    IndexStore, IndexedBatch, Network, OutputKey, PurgedRows, Purging,
};

/// Every table one network's indexing owns, in an order a foreign key is happy
/// with: a child before its parent.
///
/// The watch list is deliberately absent, and only half absent: the
/// observations are application state and a purged network re-indexes from the
/// earliest registered birthday (`docs/design/019` section 5.3), while the
/// progress and the balance they carry are claims about rows this purge just
/// removed and are reset in the same transaction.
const PURGED: [&str; 4] = [
    "transaction_address",
    "transaction_fact",
    "output",
    "network_checkpoint",
];

impl IndexStore for Repository {
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            let tables = self.tables(network)?;
            self.database
                .blocking(move |connection| tables.checkpoint(connection))
                .await
        })
    }

    fn commit<'a>(
        &'a self,
        batch: IndexedBatch,
    ) -> BoxFuture<'a, Result<Vec<OutputKey>, IndexError>> {
        Box::pin(async move {
            let tables = self.tables(batch.network())?;
            self.database
                .transaction(move |transaction| tables.apply(transaction, &batch))
                .await
        })
    }
}

impl Purging for Repository {
    /// One statement per table, and no materialized batch.
    ///
    /// This is the other half of `docs/design/030 §8 (ex-031)` section 5.2, and the one it
    /// gets right: a store that has to name every key it removes builds one
    /// delete operation per row -- of the whole network -- in memory before it
    /// commits any of them. `DELETE ... WHERE` names a predicate instead, and
    /// the rows never exist as Rust values at all.
    fn purge_network<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<PurgedRows, IndexError>> {
        Box::pin(async move {
            let tables = self.tables(network)?;
            self.database
                .transaction(move |transaction| tables.rescan(transaction))
                .await
        })
    }
}

impl Tables {
    /// Writes one batch's transactions, credits, output changes, balances and
    /// progress, and the line it leaves behind — or nothing at all.
    ///
    /// The checkpoint is read here, inside the write transaction, rather than by
    /// the caller before it: that is the whole compare-and-set. Whether the line
    /// moves is [`IndexedBatch::checkpoint`]'s answer, not a second method and
    /// not a flag.
    fn apply(
        &self,
        transaction: &Transaction<'_>,
        batch: &IndexedBatch,
    ) -> Result<Vec<OutputKey>, IndexError> {
        let current = self.checkpoint(transaction)?;
        if current.as_ref() != batch.expected_checkpoint() {
            return Err(conflict("checkpoint changed before commit"));
        }
        let mut unknown = Vec::new();
        // Block by block, creations before spends, in the order the chain built
        // them. An output created in one block of this batch and spent in a
        // later one therefore leaves a row marked spent rather than no row at
        // all — which is what stops a replay of the creating block from offering
        // it again, and is the whole reason the batch no longer folds.
        for facts in batch.blocks() {
            let position = Self::stored(facts.block().position.0)?;
            let height = Self::stored(facts.block().height.0)?;
            self.refill(transaction, facts, batch.settlement())?;
            self.refill_credits(transaction, facts.credits(), (height, position))?;
            self.fill_outputs(transaction, &facts.outputs().created)?;
            unknown.extend(self.spend(transaction, facts.outputs())?);
        }
        for claim in batch.progress() {
            self.advance(transaction, claim)?;
        }
        if let Some(line) = batch
            .checkpoint()
            .filter(|line| Some(*line) != current.as_ref())
        {
            self.set_checkpoint(transaction, line)?;
        }
        Ok(unknown)
    }

    /// Insert-if-absent, verify-if-present, for every transaction of one block
    /// and every address that took part in it.
    ///
    /// **One rule, both directions.** A newly watched address routinely already
    /// has rows in its own past: an effect is written for *every* endpoint of
    /// every movement, so the address has one wherever it was somebody else's
    /// counterparty (`docs/design/024` section 1.4). Refusing those would loop
    /// forever, and rewriting them blindly would hide corruption, so a row that
    /// differs is terminal.
    ///
    /// The transaction's own place is `(height, transaction id)`, which is a
    /// function of the block rather than of this batch: a block is reported with
    /// only the transactions that touched a selected address, so anything
    /// derived from the *position* of a transaction in that list would differ
    /// between two selections of the same block.
    fn refill(
        &self,
        transaction: &Transaction<'_>,
        facts: &CanonicalBlock,
        settlement: FeeSettlement,
    ) -> Result<(), IndexError> {
        for entry in facts.transactions() {
            let place = self.insert_fact(transaction, entry)?;
            self.participants(transaction, entry, settlement, &place)?;
        }
        Ok(())
    }

    /// One transaction's membership and effect rows, for every address that
    /// took part in it.
    fn participants(
        &self,
        transaction: &Transaction<'_>,
        entry: &CanonicalTransaction,
        settlement: FeeSettlement,
        place: &Place,
    ) -> Result<(), IndexError> {
        for address in entry.addresses() {
            self.insert_membership(transaction, &address, place)?;
            self.insert_effects(transaction, &address, entry, settlement, place)?;
        }
        Ok(())
    }

    /// Removes every indexing-owned row of this network, counts each kind, and
    /// resets what every observation of it claimed.
    ///
    /// One transaction, not one per table: a network left with history but no
    /// checkpoint is itself a terminal state, so a purge that stopped halfway
    /// would replace one unrecoverable network with another.
    ///
    /// The watch rows survive and their claims do not. Every claim described
    /// rows this purge just removed, and `WatchStore::add` preserves progress on
    /// an exact repeat by contract, so nothing could reset them afterwards.
    fn rescan(&self, transaction: &Transaction<'_>) -> Result<PurgedRows, IndexError> {
        // The two halves of `balance_movement`, counted apart because an
        // operator counts a withdrawal and a transaction's movement as
        // different facts: the transactionless rows first, then the rest.
        let mut removed = PurgedRows {
            credits: self.purge(transaction, "balance_movement", "AND transaction_id = ''")?,
            movements: self.purge(transaction, "balance_movement", "")?,
            ..PurgedRows::default()
        };
        for table in PURGED {
            let rows = self.purge(transaction, table, "")?;
            match table {
                "transaction_address" => removed.history = rows,
                "output" => removed.outputs = rows,
                "network_checkpoint" => removed.checkpoint = rows,
                _ => (),
            }
        }
        self.disclaim(transaction)?;
        Ok(removed)
    }
}

/// The statement that `docs/design/030 §8 (ex-031)` section 5.2 proposes as the checkpoint's
/// compare-and-set, kept only so a test can run it.
///
/// It is never called by the adapter: see this module's own documentation.
#[cfg(test)]
pub(super) fn proposed(
    transaction: &Transaction<'_>,
    network: &Network,
    expected: &BlockRef,
    next: &BlockRef,
) -> Result<usize, IndexError> {
    use rusqlite::params;

    use super::failure;

    transaction
        .execute(
            "UPDATE network_checkpoint SET position = ?5, height = ?6, hash = ?7
             WHERE chain = ?1 AND network = ?2 AND position = ?3 AND hash = ?4",
            params![
                network.chain.as_str(),
                network.name,
                Tables::stored(expected.position.0)?,
                expected.hash.0,
                Tables::stored(next.position.0)?,
                Tables::stored(next.height.0)?,
                next.hash.0,
            ],
        )
        .map_err(|error| failure(&error, "the proposed condition failed"))
}

/// The three invariants `docs/design/030 §8 (ex-031)` step 2 could test none of.
///
/// The shared block-store contracts are not here: they run in
/// `tests/repository_contract.rs`, against this store and the in-memory one
/// together, so a disagreement between the two is one failing pair.
#[cfg(test)]
mod blocks_test {
    use std::thread;

    use base::Decimal;
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::super::database_test::{block_on, network, open};
    use super::super::failure;
    use super::*;
    use crate::{
        Address, AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, CanonicalBlock,
        FeeSettlement, IndexErrorKind, IndexedOutput, InterpretedBlock, MovementId,
        ObservationDraft, ObservationDraftStatus, OutputChanges, OutputId, TransactionRef,
        ValueMovement,
    };

    /// One batch carrying `interpreted` alone, extending `checkpoint`.
    fn batch_of(
        checkpoint: Option<BlockRef>,
        interpreted: InterpretedBlock,
    ) -> Result<IndexedBatch, IndexError> {
        IndexedBatch::new(
            network(),
            checkpoint,
            vec![CanonicalBlock::new(network(), interpreted)?],
            Vec::new(),
            FeeSettlement::Charged,
        )
    }

    /// One fixture block, with the hash a rescan of the same chain produces
    /// again.
    fn block(height: u64) -> BlockRef {
        let hash = |height: u64| BlockHash(height.to_be_bytes().to_vec());
        BlockRef {
            position: BlockPosition(height),
            height: BlockHeight(height),
            hash: hash(height),
            parent: Some(BlockParent {
                position: BlockPosition(height - 1),
                hash: hash(height - 1),
            }),
            timestamp: Some(1_000 + height),
        }
    }

    fn address(value: &str) -> Address {
        Address::new(network(), value)
    }

    fn output(height: u64) -> IndexedOutput {
        IndexedOutput {
            id: OutputId {
                transaction: TransactionRef::new(network(), format!("block-{height}")),
                index: 0,
            },
            address: address("owner"),
            asset: AssetId::native(network().chain),
            amount: Decimal::from(9_u64),
            evidence: vec![1, 2, 3],
            created_at: BlockHeight(height),
            coinbase: false,
        }
    }

    fn draft(height: u64) -> ObservationDraft {
        ObservationDraft {
            transaction_id: TransactionRef::new(network(), format!("block-{height}")),
            transaction_order: 0,
            status: ObservationDraftStatus::Included,
            movements: vec![ValueMovement::mint(
                MovementId(format!("mint-{height}")),
                AssetId::native(network().chain),
                Decimal::from(5_u64),
                address("owner"),
            )],
            fee: None,
        }
    }

    /// The batch of `height`, creating one output, on top of `height - 1`.
    fn addition(height: u64) -> IndexedBatch {
        batch_of(
            (height > 1).then(|| block(height - 1)),
            InterpretedBlock {
                block: block(height),
                transactions: vec![draft(height)],
                outputs: OutputChanges {
                    created: vec![output(height)],
                    ..OutputChanges::default()
                },
                credits: Vec::new(),
            },
        )
        .expect("a connected fixture block")
    }

    /// Commits `heights` in order.
    fn index(repository: &Repository, heights: std::ops::RangeInclusive<u64>) {
        for height in heights {
            block_on(repository.commit(addition(height))).expect("fixture block");
        }
    }

    /// How many rows one table holds, whatever network wrote them.
    fn rows(repository: &Repository, table: &'static str) -> u64 {
        let counted = move |connection: &mut Connection| {
            let statement = format!("SELECT count(*) FROM {table}");
            connection
                .query_row(&statement, [], |row| row.get::<_, i64>(0))
                .map(|count| count as u64)
                .map_err(|error| failure(&error, "count"))
        };
        block_on(repository.database.blocking(counted)).expect("row count")
    }

    /// **Invariant 1, the proposed condition.** `docs/design/030 §8 (ex-031)` section 5.2
    /// argues that `... WHERE position = ? AND hash = ?` closes the ABA a
    /// per-key version counter reopens across `purge_network`, because it names
    /// the checkpoint's own durable state rather than a number standing in for
    /// it. It does not. A version counter is monotonic: a value a reader saw is
    /// never produced again. `(position, hash)` is the *chain's* coordinate,
    /// and a rescan re-indexes the same chain, so the deleted row comes back
    /// byte for byte and the stale condition matches a row it never read.
    ///
    /// This is that sequence, at the level of the statement itself.
    #[test]
    fn the_proposed_checkpoint_condition_cannot_see_an_aba() {
        let directory = TempDir::new().expect("temporary directory");
        let repository = open(&directory, "proposed.db");
        index(&repository, 1..=5);
        let read = block_on(repository.checkpoint(&network()))
            .expect("checkpoint")
            .expect("five blocks are committed");

        block_on(repository.purge_network(&network())).expect("the offline rescan");
        assert_eq!(
            block_on(repository.checkpoint(&network())).expect("purged"),
            None
        );
        index(&repository, 1..=5);

        let expected = read.clone();
        let affected = block_on(repository.database.transaction(move |transaction| {
            proposed(transaction, &network(), &expected, &block(6))
        }))
        .expect("the proposed statement runs");

        assert_eq!(
            affected, 1,
            "the condition was satisfied by a row written after the row it read \
             was deleted: exactly the ABA section 5.2 claims it forbids",
        );
        assert_eq!(
            block_on(repository.checkpoint(&network()))
                .expect("checkpoint")
                .map(|held| held.position),
            Some(BlockPosition(6)),
            "and the stale write landed",
        );
    }

    /// **Invariant 1, at the port.** The ABA the document worries about is not
    /// in the engine's vocabulary at all:
    /// `IndexedBatch::expected_checkpoint` is a `BlockRef` *value*, so an
    /// addition built before a purge still matches the identical checkpoint a
    /// rescan rebuilds, and the store accepts it.
    ///
    /// The engine is therefore not where this is decided, which is the finding:
    /// `purge_network` is contractually offline (`BlockStore::purge_network`),
    /// and that -- not a condition -- is what rules the interleaving out.
    #[test]
    fn a_rescanned_network_accepts_a_stale_addition() {
        let directory = TempDir::new().expect("temporary directory");
        let repository = open(&directory, "stale.db");
        index(&repository, 1..=5);
        let held = addition(6);
        block_on(repository.purge_network(&network())).expect("purge");
        index(&repository, 1..=5);
        block_on(repository.commit(held)).expect("the stale addition");
        assert_eq!(
            block_on(repository.checkpoint(&network()))
                .expect("checkpoint")
                .map(|held| held.position),
            Some(BlockPosition(6)),
            "the store accepts a batch planned before the purge",
        );
    }

    /// **Invariant 1, adversarially.** A purge on one connection while a commit
    /// runs on another: `BEGIN IMMEDIATE` holds the file's single write lock,
    /// so the two serialize whichever way the scheduler picks, and the end state
    /// is never a network with history but no checkpoint -- which is itself a
    /// terminal state (`docs/design/019` section 5.3).
    #[test]
    fn a_purge_racing_a_commit_never_leaves_history_without_a_checkpoint() {
        let mut refusals = 0;
        for attempt in 0..20_u64 {
            let directory = TempDir::new().expect("temporary directory");
            let writer = open(&directory, "race.db");
            let purger = open(&directory, "race.db");
            index(&writer, 1..=3);
            let (writer, purger) = (&writer, &purger);

            // Alternate which side is spawned: an unfair lock plus a thread
            // that is always started second gives one ordering every time, and
            // one ordering is not a race.
            let own = network();
            let own = &own;
            let committed = thread::scope(|scope| {
                if attempt % 2 == 0 {
                    let purging = scope.spawn(|| purger.purge_network(own));
                    let committed = block_on(IndexStore::commit(writer, addition(4)));
                    block_on(purging.join().expect("the purging thread"))
                        .expect("a purge is never refused");
                    return committed;
                }
                let committing = scope.spawn(|| block_on(IndexStore::commit(writer, addition(4))));
                block_on(purger.purge_network(own)).expect("a purge is never refused");
                committing.join().expect("the committing thread")
            });
            refusals += u64::from(committed.is_err());

            let checkpoint = block_on(writer.checkpoint(&network())).expect("checkpoint");
            let history = rows(writer, "transaction_address");
            let outputs = rows(writer, "output");
            match checkpoint {
                None => assert_eq!(
                    (history, outputs),
                    (0, 0),
                    "attempt {attempt}: a purged network keeps no row at all",
                ),
                Some(block) => assert_eq!(
                    (history, outputs),
                    (1, 1),
                    "attempt {attempt}: the only surviving state is the commit that \
                     landed after the purge, at {block:?}",
                ),
            }
        }
        println!("  the commit was refused in {refusals} of 20 interleavings");
    }

    /// **Invariant 3.** `add` writes history, movements, live outputs and the
    /// checkpoint or none of them. The refusal here happens *after* the history
    /// and movement rows of the failing block are already inserted, so the four
    /// tables can only agree if the whole transaction rolled back.
    #[test]
    fn a_refused_commit_leaves_every_table_as_it_was() {
        let directory = TempDir::new().expect("temporary directory");
        let repository = open(&directory, "atomic.db");
        index(&repository, 1..=1);
        let before = [
            rows(&repository, "transaction_address"),
            rows(&repository, "balance_movement"),
            rows(&repository, "output"),
            rows(&repository, "network_checkpoint"),
        ];

        let again = IndexedOutput {
            created_at: BlockHeight(2),
            ..output(1)
        };
        let recreated = batch_of(
            Some(block(1)),
            InterpretedBlock {
                block: block(2),
                transactions: vec![draft(2)],
                outputs: OutputChanges {
                    // The block's own history and movements are written first
                    // and this is refused after them.
                    created: vec![again],
                    ..OutputChanges::default()
                },
                credits: Vec::new(),
            },
        )
        .expect("a valid block that the store must refuse");
        let error = block_on(repository.commit(recreated)).expect_err("a re-created output");

        assert_eq!(error.kind, IndexErrorKind::Store);
        assert!(!error.retryable, "one outpoint cannot have two contents");
        assert_eq!(
            [
                rows(&repository, "transaction_address"),
                rows(&repository, "balance_movement"),
                rows(&repository, "output"),
                rows(&repository, "network_checkpoint"),
            ],
            before,
            "a refused commit wrote a row into some table",
        );
        assert_eq!(
            block_on(repository.checkpoint(&network())).expect("checkpoint"),
            Some(block(1)),
        );
    }
}

/// The two costs `docs/design/030 §8 (ex-031)` never booked: an offline rescan of a long
/// network, and what one shared connection does to a reader.
///
/// Commit and read throughput itself is `examples/bench.rs`; what is here is
/// only what a second connection makes observable, which a single-threaded
/// benchmark cannot show.
///
/// Ignored by default; every one of these writes thousands of durable commits:
///
///   cargo test --release -p indexing --features sqlite \
///       -- --ignored --nocapture throughput_test
#[cfg(test)]
mod throughput_test {
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::{Duration, Instant},
    };

    use base::Decimal;
    use tempfile::TempDir;

    use super::super::database_test::{block_on, narrow, network, open};
    use super::*;
    use crate::{
        Address, AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, CanonicalBlock,
        ConfirmedHistory, FeeSettlement, HistoryQuery, IndexedBatch, IndexedOutput,
        InterpretedBlock, MovementId, ObservationDraft, ObservationDraftStatus, OutputChanges,
        OutputId, OutputKey, TransactionRef, ValueMovement,
    };

    /// `examples/bench.rs`'s default profile, narrowed to the shape these
    /// measurements commit.
    const TXS: usize = 40;
    const MOVEMENTS: usize = 2;
    const ADDRESSES: usize = 16;
    const CREATED: usize = 40;
    const SPENT: usize = 30;
    /// How long the network a rescan removes is, by default. It is an
    /// environment variable so the memory profile of a purge can be watched as
    /// the network grows: that, and not the wall clock, is the claim
    /// `docs/design/030 §8 (ex-031)` section 5.2 makes about the unbounded batch.
    fn purged_blocks() -> u64 {
        std::env::var("PURGE_BLOCKS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(600)
    }

    /// The control for the same measurement: `PURGE_SKIP=1` writes the network
    /// and stops, so the peak the purge itself adds can be told apart from the
    /// peak writing the fixture reached.
    fn purge_skipped() -> bool {
        std::env::var("PURGE_SKIP").is_ok_and(|value| value != "0")
    }
    /// How many blocks the reader-contention fixture settles before the writer
    /// and the reader start running against each other.
    const SETTLED: u64 = 400;

    fn address(index: usize) -> Address {
        Address::new(network(), format!("addr-{:04}", index % ADDRESSES))
    }

    fn asset() -> AssetId {
        AssetId::native(network().chain)
    }

    fn identity(height: u64, index: usize) -> TransactionRef {
        TransactionRef::new(network(), format!("{height:010}-{index:05}"))
    }

    fn movement(height: u64, index: usize, ordinal: usize) -> ValueMovement {
        ValueMovement::transfer(
            MovementId(format!("{height}-{index}-{ordinal}")),
            asset(),
            Decimal::from(1_000 + ordinal as u64),
            address(index),
            address(index + 1),
        )
    }

    fn transactions(height: u64) -> Vec<ObservationDraft> {
        (0..TXS)
            .map(|index| ObservationDraft {
                transaction_id: identity(height, index),
                transaction_order: index as u64,
                status: ObservationDraftStatus::Included,
                movements: (0..MOVEMENTS)
                    .map(|ordinal| movement(height, index, ordinal))
                    .collect(),
                fee: None,
            })
            .collect()
    }

    fn outputs(height: u64) -> Vec<IndexedOutput> {
        (0..CREATED)
            .map(|index| IndexedOutput {
                id: OutputId {
                    transaction: identity(height, index),
                    index: index as u32,
                },
                address: address(index),
                asset: asset(),
                amount: Decimal::from(50_000 + index as u64),
                evidence: vec![0x51; 22],
                created_at: BlockHeight(height),
                coinbase: index == 0,
            })
            .collect()
    }

    fn reference(height: u64, parent: Option<&BlockRef>) -> BlockRef {
        BlockRef {
            position: BlockPosition(height),
            height: BlockHeight(height),
            hash: BlockHash(format!("hash-{height:010}").into_bytes()),
            parent: parent.map(|block| BlockParent {
                position: block.position,
                hash: block.hash.clone(),
            }),
            timestamp: Some(1_700_000_000 + height),
        }
    }

    /// One block of the profile, and the outputs the next one spends.
    fn interpret(
        height: u64,
        parent: Option<&BlockRef>,
        spend: Vec<OutputKey>,
    ) -> (InterpretedBlock, Vec<OutputKey>) {
        let created = outputs(height);
        let next = created.iter().take(SPENT).map(IndexedOutput::key).collect();
        (
            InterpretedBlock {
                block: reference(height, parent),
                transactions: transactions(height),
                outputs: OutputChanges {
                    created,
                    spent: spend,
                    tracked_spends: Vec::new(),
                },
                credits: Vec::new(),
            },
            next,
        )
    }

    /// Where a run of profile commits stopped: the tip, and the outputs the
    /// next block spends.
    type Tip = (Option<BlockRef>, Vec<OutputKey>);

    /// Commits `blocks` blocks of the profile and reports the commit time, the
    /// worst single block, and where it stopped.
    fn commit(repository: &impl IndexStore, blocks: u64) -> (f64, Duration, Tip) {
        let started = Instant::now();
        let (worst, tip) = extend(repository, (None, Vec::new()), 0, blocks);
        (started.elapsed().as_secs_f64(), worst, tip)
    }

    /// One batch carrying `interpreted` alone, extending `checkpoint`.
    fn batch_of(
        checkpoint: Option<BlockRef>,
        interpreted: InterpretedBlock,
    ) -> Result<IndexedBatch, IndexError> {
        IndexedBatch::new(
            network(),
            checkpoint,
            vec![CanonicalBlock::new(network(), interpreted)?],
            Vec::new(),
            FeeSettlement::Charged,
        )
    }

    /// Commits `blocks` more blocks on top of `from`, which stopped at height
    /// `above`.
    fn extend(repository: &impl IndexStore, from: Tip, above: u64, blocks: u64) -> (Duration, Tip) {
        let (mut parent, mut spend) = from;
        let mut worst = Duration::ZERO;
        for height in above + 1..=above + blocks {
            let (interpreted, next) = interpret(height, parent.as_ref(), spend);
            let block = interpreted.block.clone();
            let batch = batch_of(parent.clone(), interpreted).expect("a valid profile block");
            let one = Instant::now();
            block_on(repository.commit(batch)).expect("profile block");
            worst = worst.max(one.elapsed());
            parent = Some(block);
            spend = next;
        }
        (worst, (parent, spend))
    }

    /// One history page of `index`'s address, or the refusal.
    fn page(
        repository: &impl ConfirmedHistory,
        index: usize,
    ) -> Result<crate::CanonicalPage, IndexError> {
        block_on(repository.history(HistoryQuery {
            address: address(index),
            asset: asset(),
            after: None,
            limit: 100,
        }))
    }

    #[test]
    #[ignore = "writes a long network and then removes it"]
    fn sqlite_purges_a_long_network() {
        let directory = TempDir::new().expect("temporary directory");
        let repository = open(&directory, "purge.db");
        let blocks = purged_blocks();
        let (seconds, ..) = commit(&repository, blocks);
        if purge_skipped() {
            println!("  sqlite {blocks} blocks written in {seconds:.1}s, not purged");
            return;
        }
        let started = Instant::now();
        let removed = block_on(repository.purge_network(&network())).expect("purge");
        println!(
            "  sqlite {blocks} blocks written in {seconds:.1}s, purged in {:.3}s: {removed:?}",
            started.elapsed().as_secs_f64(),
        );
    }

    /// What the write-ahead log gives a reader that runs while the writer
    /// commits, and what one shared connection takes back.
    ///
    /// The same reads are timed three ways while a writer commits blocks of
    /// the profile on another thread: against a SQLite file whose pool lends
    /// one connection -- the shared handle the pool replaced -- against the
    /// pool as it ships, and against a second `Database` on the same file,
    /// which is the concurrency the pool is claiming to reach.
    #[test]
    #[ignore = "commits blocks on one thread while another reads"]
    fn a_reader_pays_for_the_connection_it_shares() {
        // A ceiling of one: the reader and the writer take turns on the same
        // handle, which is what this adapter did before the pool.
        let directory = TempDir::new().expect("temporary directory");
        let writer = narrow(&directory, "shared.db", 1);
        let (.., tip) = commit(&writer, SETTLED);
        alongside("sqlite-1", &writer, &writer, tip);

        // The pool as it ships: one `Database`, one `Repository`, and a
        // connection each for the reader and the writer.
        let directory = TempDir::new().expect("temporary directory");
        let writer = open(&directory, "pooled.db");
        let (.., tip) = commit(&writer, SETTLED);
        alongside("sqlite", &writer, &writer, tip);

        // Two `Database`s on one file: the same concurrency reached without
        // the pool, as the control for the line above.
        let directory = TempDir::new().expect("temporary directory");
        let writer = open(&directory, "separate.db");
        let (.., tip) = commit(&writer, SETTLED);
        let reader = open(&directory, "separate.db");
        alongside("sqlite-2", &writer, &reader, tip);
    }

    /// Reads history pages on `reader` for as long as `writer` is committing,
    /// and reports what one page cost while it was.
    fn alongside<W, R>(name: &str, writer: &W, reader: &R, tip: Tip)
    where
        W: IndexStore + Sync,
        R: ConfirmedHistory + Sync,
    {
        let committing = AtomicBool::new(true);
        std::thread::scope(|scope| {
            let running = &committing;
            let handle = scope.spawn(move || {
                extend(writer, tip, SETTLED, SETTLED);
                running.store(false, Ordering::Relaxed);
            });
            let mut worst = Duration::ZERO;
            let mut reads = 0_usize;
            let mut refused = 0_usize;
            let started = Instant::now();
            // Bounded, because `std::sync::Mutex` is not fair: a reader that
            // never pauses can hold the shared connection against the writer
            // for as long as it likes, and this measurement must end.
            while committing.load(Ordering::Relaxed) && reads < 20_000 {
                let one = Instant::now();
                refused += usize::from(page(reader, reads).is_err());
                worst = worst.max(one.elapsed());
                reads += 1;
            }
            let seconds = started.elapsed().as_secs_f64();
            handle.join().expect("the committing thread");
            println!(
                "  {name:9} {reads:6} pages in {seconds:.2}s of committing: \
                 {:6.3}ms/page mean, {:6.2}ms worst, {refused} refused",
                seconds * 1000.0 / reads as f64,
                worst.as_secs_f64() * 1000.0,
            );
        });
    }

    /// Two writers on one file, which is how `apps/server` runs three chains:
    /// one `Database`, one connection each out of its pool.
    ///
    /// This is the pool's cost, and the reason it is worth naming. One shared
    /// connection serialized the two commits in this process, so the file's
    /// write lock was never contended from inside it; two connections contend
    /// for it the way two processes would, and the second `BEGIN IMMEDIATE`
    /// waits on the busy timeout. The question is whether that wait is
    /// absorbed or whether `SQLITE_BUSY` reaches the caller as a `Conflict`.
    #[test]
    #[ignore = "commits from two threads onto one file"]
    fn two_writers_on_one_file_meet_the_busy_timeout() {
        let directory = TempDir::new().expect("temporary directory");
        let elsewhere = Network::new(crate::ChainName::new("other"), "testnet");
        let first = open(&directory, "writers.db");
        let second = Repository::new(first.database.clone());
        let conflicts = std::thread::scope(|scope| {
            let other = scope.spawn(|| count_failures(&second, &elsewhere));
            let mine = count_failures(&first, &network());
            mine + other.join().expect("the other writer")
        });
        println!("  sqlite two writers, 400 commits: {conflicts} refused");
        assert_eq!(conflicts, 0, "the busy timeout absorbed every collision");
    }

    /// Commits 200 blocks of `network` and counts the refusals.
    fn count_failures(repository: &impl IndexStore, network: &Network) -> usize {
        let mut parent: Option<BlockRef> = None;
        let mut failures = 0;
        for height in 1..=200 {
            let block = reference(height, parent.as_ref());
            let facts = CanonicalBlock::new(
                network.clone(),
                InterpretedBlock {
                    block: block.clone(),
                    transactions: Vec::new(),
                    outputs: OutputChanges::default(),
                    credits: Vec::new(),
                },
            )
            .expect("a valid block");
            let batch = IndexedBatch::new(
                network.clone(),
                parent.clone(),
                vec![facts],
                Vec::new(),
                FeeSettlement::Charged,
            )
            .expect("a connected block");
            failures += usize::from(block_on(repository.commit(batch)).is_err());
            parent = Some(block);
        }
        failures
    }
}
