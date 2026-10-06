//! Canonical blocks, atomic commit and offline network rescan.
//! The checkpoint read and its conditional write share one BEGIN IMMEDIATE
//! transaction, excluding intervening writers. A checkpoint coordinate alone is
//! not a mutation version: rescan can reproduce the same position and hash.

use sqlx::SqliteConnection;

use super::{Repository, checkpoint, commit, conflict, found, models};
use crate::{
    BlockRef, BoxFuture, IndexError, IndexStore, IndexedBatch, Network, OutputKey, Progress,
    PurgedRows, Purging,
};

use models::{Block, Ledger, NetworkCheckpoint, Output, Transaction, TransactionAddress, Wallet};

impl IndexStore for Repository {
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            let network = Self::network(network)?;
            let mut connection = self.database.connection().await?;
            checkpoint(&mut connection, &network).await
        })
    }

    fn commit<'a>(
        &'a self,
        batch: IndexedBatch,
    ) -> BoxFuture<'a, Result<Vec<OutputKey>, IndexError>> {
        Box::pin(async move {
            let network = Self::network(batch.network())?;
            let mut write = self.database.write().await?;
            let unknown = Self::apply(&mut write, &network, &batch).await?;
            commit(write).await?;
            Ok(unknown)
        })
    }
}

impl Purging for Repository {
    /// Removes every indexing-owned row of one network inside one
    /// transaction, counts each kind, and resets what every observation of it
    /// claimed. SQL predicates bound deletion memory independently of the
    /// network's row count.
    ///
    /// One transaction, not one per table: a network left with history but no
    /// checkpoint is itself a terminal state, so a purge that stopped halfway
    /// would replace one unrecoverable network with another. Each table goes
    /// in an order a foreign key is happy with, a child before its parent.
    ///
    /// The wallet rows survive and their claims do not. The observations are
    /// application state and a purged network re-indexes from the earliest
    /// registered birthday, while every claim described rows this purge just
    /// removed, and `WatchStore::add` preserves progress on an exact repeat by
    /// contract, so nothing could reset them afterwards.
    fn purge_network<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<PurgedRows, IndexError>> {
        Box::pin(async move {
            let network = Self::network(network)?;
            let mut write = self.database.write().await?;
            let (credits, movements) = Ledger::purge(&mut write, &network).await?;
            let history = TransactionAddress::purge(&mut write, &network).await?;
            Transaction::purge(&mut write, &network).await?;
            let outputs = Output::purge(&mut write, &network).await?;
            let checkpoint = NetworkCheckpoint::purge(&mut write, &network).await?;
            Wallet::purge(&mut write, &network).await?;
            commit(write).await?;
            Ok(PurgedRows {
                credits,
                movements,
                history,
                outputs,
                checkpoint,
            })
        })
    }
}

impl Repository {
    /// Writes one batch's transactions, credits, output changes, balances and
    /// progress, and the line it leaves behind — or nothing at all.
    ///
    /// Each table is written many rows to a statement; a watched total moves
    /// once per holding, by what the batch's written effects add up to.
    ///
    /// The checkpoint is read here, inside the write transaction, rather than by
    /// the caller before it: that is the whole compare-and-set. Whether the line
    /// moves is [`IndexedBatch::checkpoint`]'s answer, not a second method and
    /// not a flag.
    async fn apply(
        transaction: &mut SqliteConnection,
        network: &Network,
        batch: &IndexedBatch,
    ) -> Result<Vec<OutputKey>, IndexError> {
        let current = checkpoint(transaction, network).await?;
        if current.as_ref() != batch.expected_checkpoint() {
            return Err(conflict("checkpoint changed before commit"));
        }
        // Every row of each table is gathered for the whole batch first, so a
        // table is written many rows to a statement; none of the three reads
        // another, so their order is free.
        Transaction::insert_missing(transaction, &Transaction::from_batch(batch)?).await?;
        TransactionAddress::insert_many(transaction, &TransactionAddress::from_batch(batch)?)
            .await?;
        let deltas = Ledger::insert_missing(transaction, &Ledger::from_batch(batch)?).await?;
        let mut unknown = Vec::new();
        // Block by block, creations before spends, in the order the chain built
        // them. An output created in one block of this batch and spent in a
        // later one therefore leaves a row marked spent rather than no row at
        // all — which is what stops a replay of the creating block from offering
        // it again, and is the whole reason the batch does not fold.
        for canonical in batch.blocks() {
            Output::insert_created(transaction, &canonical.outputs.created).await?;
            unknown.extend(Output::update_spent(transaction, network, &canonical.outputs).await?);
        }
        Wallet::update_balances(transaction, network, &deltas).await?;
        for claim in batch.progress() {
            Self::advance(transaction, network, claim).await?;
        }
        if let Some(line) = batch
            .checkpoint()
            .filter(|line| Some(*line) != current.as_ref())
        {
            NetworkCheckpoint::upsert(transaction, &NetworkCheckpoint::try_from((network, line))?)
                .await?;
        }
        Ok(unknown)
    }

    /// Records one claim, after comparing what it expected against what is
    /// held.
    ///
    /// It lives on the observation's own `wallet` row, so withdrawing an
    /// observation takes its claim with it and a claim can never be paired
    /// with the wrong observation. The write is a compare-and-set against the
    /// value the claim's author says it read, deliberately **not** a monotonic
    /// rule: a batch redone under a smaller budget legitimately settles less
    /// than the write it is redoing, so what this refuses is a claim whose
    /// author was reading a different value.
    ///
    /// Two refusals, both retryable, and both about the caller's belief rather
    /// than the store's state: a claim naming an observation this file does not
    /// hold, and a claim whose `expected` is not what the row says. One read
    /// answers both.
    async fn advance(
        transaction: &mut SqliteConnection,
        network: &Network,
        claim: &Progress,
    ) -> Result<(), IndexError> {
        let held = Wallet::get_observed_by_id(transaction, network, &claim.watch_id).await;
        let Some(held) = found(held, "failed to read recorded progress")? else {
            return Err(conflict(
                "indexed progress names an observation this store does not hold",
            ));
        };
        if held.progress()?.as_ref() != claim.expected.as_ref() {
            return Err(conflict("recorded progress changed before this commit"));
        }
        let through = Block::try_from(&claim.indexed_through)?;
        Wallet::update_progress(transaction, network, &claim.watch_id, &through).await
    }
}

/// A deliberately insufficient coordinate-only condition, exercised by the ABA
/// regression test. Production commits read and write under one transaction.
#[cfg(test)]
pub(super) async fn proposed(
    transaction: &mut SqliteConnection,
    network: &Network,
    expected: &BlockRef,
    next: &BlockRef,
) -> Result<u64, IndexError> {
    sqlx::query(
        "UPDATE network_checkpoint SET block_position = ?5, block_height = ?6,
             block_hash = ?7
         WHERE chain = ?1 AND network = ?2 AND block_position = ?3 AND block_hash = ?4",
    )
    .bind(network.chain.as_str())
    .bind(&network.name)
    .bind(super::models::stored(expected.position.0)?)
    .bind(expected.hash.0.as_slice())
    .bind(super::models::stored(next.position.0)?)
    .bind(super::models::stored(next.height.0)?)
    .bind(next.hash.0.as_slice())
    .execute(transaction)
    .await
    .map(|done| done.rows_affected())
    .map_err(|error| super::failure(&error, "the proposed condition failed"))
}

/// File-store regressions for atomicity, stale conditions and network isolation.
#[cfg(test)]
mod commit_test {
    use base::Decimal;
    use tempfile::TempDir;

    use super::super::database_test::{network, open};
    use super::*;
    use crate::{
        Address, AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockWithOutputs,
        FeeSettlement, IndexErrorKind, IndexedOutput, MovementId, OutputChanges, OutputId, Status,
        Transaction, TransactionRef, ValueMovement,
    };

    /// One batch carrying `interpreted` alone, extending `checkpoint`.
    fn batch_of(
        checkpoint: Option<BlockRef>,
        interpreted: BlockWithOutputs,
    ) -> Result<IndexedBatch, IndexError> {
        IndexedBatch::new(
            network(),
            checkpoint,
            vec![interpreted],
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

    fn draft(height: u64) -> Transaction {
        Transaction {
            id: TransactionRef::new(network(), format!("block-{height}")),
            movements: vec![ValueMovement::mint(
                MovementId(format!("mint-{height}")),
                AssetId::native(network().chain),
                Decimal::from(5_u64),
                address("owner"),
            )],
            fee: None,
            order: 0,
            status: Status::Included {
                block: block(height),
            },
        }
    }

    /// The batch of `height`, creating one output, on top of `height - 1`.
    fn addition(height: u64) -> IndexedBatch {
        batch_of(
            (height > 1).then(|| block(height - 1)),
            BlockWithOutputs {
                outputs: OutputChanges {
                    created: vec![output(height)],
                    ..OutputChanges::default()
                },
                ..BlockWithOutputs::new(&network(), block(height), vec![draft(height)])
                    .expect("unique transaction ids")
            },
        )
        .expect("a connected fixture block")
    }

    /// Commits `heights` in order.
    async fn index(repository: &Repository, heights: std::ops::RangeInclusive<u64>) {
        for height in heights {
            repository
                .commit(addition(height))
                .await
                .expect("fixture block");
        }
    }

    /// How many rows one table holds, whatever network wrote them.
    async fn rows(repository: &Repository, table: &'static str) -> u64 {
        let statement = format!("SELECT count(*) FROM {table}");
        let count: i64 = sqlx::query_scalar(&statement)
            .fetch_one(repository.database.pool())
            .await
            .expect("row count");
        u64::try_from(count).expect("a count")
    }

    /// A rescan can recreate identical checkpoint coordinates. A coordinate-only
    /// condition therefore cannot detect a delete/recreate ABA.
    #[tokio::test]
    async fn the_proposed_checkpoint_condition_cannot_see_an_aba() {
        let directory = TempDir::new().expect("temporary directory");
        let repository = open(&directory, "proposed.db").await;
        index(&repository, 1..=5).await;
        let read = repository
            .checkpoint(&network())
            .await
            .expect("checkpoint")
            .expect("five blocks are committed");

        repository
            .purge_network(&network())
            .await
            .expect("the offline rescan");
        assert_eq!(
            repository.checkpoint(&network()).await.expect("purged"),
            None
        );
        index(&repository, 1..=5).await;

        let mut write = repository.database.write().await.expect("a write");
        let affected = proposed(&mut write, &network(), &read, &block(6))
            .await
            .expect("the proposed statement runs");
        commit(write).await.expect("commit");

        assert_eq!(
            affected, 1,
            "the condition was satisfied by a row written after the row it read \
             was deleted: exactly the ABA section 5.2 claims it forbids",
        );
        assert_eq!(
            repository
                .checkpoint(&network())
                .await
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
    #[tokio::test]
    async fn a_rescanned_network_accepts_a_stale_addition() {
        let directory = TempDir::new().expect("temporary directory");
        let repository = open(&directory, "stale.db").await;
        index(&repository, 1..=5).await;
        let held = addition(6);
        repository.purge_network(&network()).await.expect("purge");
        index(&repository, 1..=5).await;
        repository.commit(held).await.expect("the stale addition");
        assert_eq!(
            repository
                .checkpoint(&network())
                .await
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
    /// terminal state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_purge_racing_a_commit_never_leaves_history_without_a_checkpoint() {
        let mut refusals = 0;
        for attempt in 0..20_u64 {
            let directory = TempDir::new().expect("temporary directory");
            let writer = open(&directory, "race.db").await;
            let purger = open(&directory, "race.db").await;
            index(&writer, 1..=3).await;

            // Alternate which side is spawned: an unfair lock plus a task
            // that is always started second gives one ordering every time, and
            // one ordering is not a race.
            let committing = {
                let writer = writer.clone();
                async move { IndexStore::commit(&writer, addition(4)).await }
            };
            let purging = {
                let purger = purger.clone();
                async move { purger.purge_network(&network()).await }
            };
            let (committed, purged) = if attempt % 2 == 0 {
                let purging = tokio::spawn(purging);
                let committed = committing.await;
                (committed, purging.await.expect("the purging task"))
            } else {
                let committing = tokio::spawn(committing);
                let purged = purging.await;
                (committing.await.expect("the committing task"), purged)
            };
            purged.expect("a purge is never refused");
            refusals += u64::from(committed.is_err());

            let checkpoint = writer.checkpoint(&network()).await.expect("checkpoint");
            let history = rows(&writer, "transaction_address").await;
            let outputs = rows(&writer, "output").await;
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
    #[tokio::test]
    async fn a_refused_commit_leaves_every_table_as_it_was() {
        let directory = TempDir::new().expect("temporary directory");
        let repository = open(&directory, "atomic.db").await;
        index(&repository, 1..=1).await;
        let tables = [
            "transaction_address",
            "ledger",
            "output",
            "network_checkpoint",
        ];
        let mut before = Vec::new();
        for table in tables {
            before.push(rows(&repository, table).await);
        }

        let again = IndexedOutput {
            created_at: BlockHeight(2),
            ..output(1)
        };
        let recreated = batch_of(
            Some(block(1)),
            BlockWithOutputs {
                outputs: OutputChanges {
                    // The block's own history and movements are written first
                    // and this is refused after them.
                    created: vec![again],
                    ..OutputChanges::default()
                },
                ..BlockWithOutputs::new(&network(), block(2), vec![draft(2)])
                    .expect("unique transaction ids")
            },
        )
        .expect("a valid block that the store must refuse");
        let error = repository
            .commit(recreated)
            .await
            .expect_err("a re-created output");

        assert_eq!(error.kind, IndexErrorKind::Store);
        assert!(!error.retryable, "one outpoint cannot have two contents");
        let mut after = Vec::new();
        for table in tables {
            after.push(rows(&repository, table).await);
        }
        assert_eq!(
            after, before,
            "a refused commit wrote a row into some table"
        );
        assert_eq!(
            repository.checkpoint(&network()).await.expect("checkpoint"),
            Some(block(1)),
        );
    }
}

/// Ignored measurements of rescan memory and concurrent-reader latency.
#[cfg(test)]
mod throughput_test {
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::{Duration, Instant},
    };

    use base::Decimal;
    use tempfile::TempDir;

    use super::super::database_test::{narrow, network};
    use super::*;
    use crate::{
        Address, AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockWithOutputs,
        ConfirmedHistory, FeeSettlement, HistoryQuery, IndexedBatch, IndexedOutput, MovementId,
        OutputChanges, OutputId, OutputKey, Status, Transaction, TransactionRef, ValueMovement,
    };

    /// The measurements drive the store from plain threads, as two processes
    /// would, so each thread blocks on one shared Tokio runtime.
    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("test runtime")
            })
            .block_on(future)
    }

    /// One repository over a fresh file named `name`.
    fn open(directory: &TempDir, name: &str) -> Repository {
        block_on(super::super::database_test::open(directory, name))
    }

    /// `examples/bench.rs`'s default profile, narrowed to the shape these
    /// measurements commit.
    const TXS: usize = 40;
    const MOVEMENTS: usize = 2;
    const ADDRESSES: usize = 16;
    const CREATED: usize = 40;
    const SPENT: usize = 30;
    /// Number of fixture blocks used to observe rescan memory as network size grows.
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

    fn transactions(block: &BlockRef) -> Vec<Transaction> {
        let height = block.height.0;
        (0..TXS)
            .map(|index| Transaction {
                id: identity(height, index),
                movements: (0..MOVEMENTS)
                    .map(|ordinal| movement(height, index, ordinal))
                    .collect(),
                fee: None,
                order: index as u64,
                status: Status::Included {
                    block: block.clone(),
                },
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
    ) -> (BlockWithOutputs, Vec<OutputKey>) {
        let created = outputs(height);
        let next = created.iter().take(SPENT).map(IndexedOutput::key).collect();
        (
            BlockWithOutputs {
                outputs: OutputChanges {
                    created,
                    spent: spend,
                    tracked_spends: Vec::new(),
                },
                ..{
                    let block = reference(height, parent);
                    let transactions = transactions(&block);
                    BlockWithOutputs::new(&network(), block, transactions)
                        .expect("unique transaction ids")
                }
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
        interpreted: BlockWithOutputs,
    ) -> Result<IndexedBatch, IndexError> {
        IndexedBatch::new(
            network(),
            checkpoint,
            vec![interpreted],
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
            let block = interpreted.block.reference().clone();
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
    ) -> Result<crate::ConfirmedHistoryPage, IndexError> {
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
        let writer = block_on(narrow(&directory, "shared.db", 1));
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
            let canonical = BlockWithOutputs::new(network, block.clone(), Vec::new())
                .expect("unique transaction ids");
            let batch = IndexedBatch::new(
                network.clone(),
                parent.clone(),
                vec![canonical],
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
