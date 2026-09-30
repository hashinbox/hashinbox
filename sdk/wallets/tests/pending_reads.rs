//! `PendingJoin` over the two things a chain pairs it from: an index that is
//! really indexing, and the repository it commits into.
//!
//! The join's own tests hold it to a list they change by hand, and the
//! repository contracts hold each store to what it may claim. Neither runs the
//! pair. These do: the blocks above the line are the ones a pass held, the
//! progress below it is what a pass recorded, and the answer is read the way a
//! consumer reads it.

use std::sync::Arc;

use base::Decimal;
use futures_executor::block_on;
use indexing::{
    Address, AssetId, Birthday, BlockHash, BlockHeight, BlockInterpreter, BlockParent,
    BlockPosition, BlockRef, BlockSource, BoxFuture, ChainName, ChainTips, Completeness,
    FeeSettlement, IndexError, IndexErrorKind, IndexedOutput, Indexer, Indexing, InterpretedBlock,
    MovementId, NativeBlock, Network, ObservationDraft, ObservationDraftStatus, OutputChanges,
    OutputId, OutputPage, OutputRequest, OutputStore, SourceError, Subject, SyncConfig, SyncPolicy,
    TransactionRef, ValueMovement, Watching, memory,
};
use wallets::{Balances, History, Page, PendingJoin, TransactionStatus};

/// Block 2 funds the owner with one output, and block 6 spends it and returns
/// the change.
const FUNDED: u64 = 2;
const SPENT: u64 = 6;

fn network() -> Network {
    Network::new(ChainName::new("joined"), "testnet")
}

fn asset() -> AssetId {
    AssetId::native(network().chain)
}

fn address(value: &str) -> Address {
    Address::new(network(), value)
}

fn owner() -> Subject {
    Subject {
        address: address("owner"),
        asset: asset(),
    }
}

fn block(height: u64) -> BlockRef {
    BlockRef {
        position: BlockPosition(height),
        height: BlockHeight(height),
        hash: BlockHash(height.to_be_bytes().to_vec()),
        parent: height.checked_sub(1).map(|parent| BlockParent {
            position: BlockPosition(parent),
            hash: BlockHash(parent.to_be_bytes().to_vec()),
        }),
        timestamp: None,
    }
}

fn transaction(height: u64) -> TransactionRef {
    TransactionRef::new(network(), format!("tx-{height}"))
}

/// The one output the transaction of block `height` leaves the owner.
fn output(height: u64, amount: u64) -> IndexedOutput {
    IndexedOutput {
        id: OutputId {
            transaction: transaction(height),
            index: 0,
        },
        address: address("owner"),
        asset: asset(),
        amount: Decimal::from(amount),
        evidence: Vec::new(),
        created_at: BlockHeight(height),
        coinbase: false,
    }
}

/// A chain of `tip + 1` blocks whose newest `depth` are not confirmed yet.
#[derive(Clone)]
struct Source {
    tip: u64,
    depth: u64,
}

#[derive(Clone)]
struct Block(BlockRef);

impl NativeBlock for Block {
    fn block_ref(&self) -> BlockRef {
        self.0.clone()
    }
}

impl ChainTips for Source {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move { Ok(block(self.tip)) })
    }

    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move { Ok(block(self.tip - self.depth)) })
    }
}

impl BlockSource for Source {
    type Block = Block;

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>> {
        let blocks = (start.0..=end.0.min(self.tip))
            .take(limit)
            .map(|height| Block(block(height)))
            .collect();
        Box::pin(async move { Ok(blocks) })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        let found = (position.0 <= self.tip).then(|| block(position.0));
        Box::pin(async move { Ok(found) })
    }
}

/// Reports the owner's two transactions, and only while the owner is among
/// the addresses a block is inspected for.
struct Interpreter;

impl Interpreter {
    /// One transfer of `amount` between the owner and somebody else.
    fn moved(height: u64, amount: u64, from: &str, to: &str) -> ObservationDraft {
        ObservationDraft {
            transaction_id: transaction(height),
            transaction_order: 0,
            status: ObservationDraftStatus::Included,
            movements: vec![ValueMovement::transfer(
                MovementId("0".into()),
                asset(),
                Decimal::from(amount),
                address(from),
                address(to),
            )],
            fee: None,
        }
    }
}

impl BlockInterpreter for Interpreter {
    type Block = Block;

    fn settlement(&self) -> FeeSettlement {
        FeeSettlement::Embedded
    }

    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[Address],
    ) -> Result<InterpretedBlock, IndexError> {
        let reference = block.block_ref();
        let height = reference.height.0;
        let watched = addresses.contains(&address("owner"));
        let (transactions, outputs) = match height {
            FUNDED if watched => (
                vec![Self::moved(height, 100, "payer", "owner")],
                OutputChanges {
                    created: vec![output(FUNDED, 100)],
                    ..OutputChanges::default()
                },
            ),
            SPENT if watched => (
                vec![Self::moved(height, 10, "owner", "payee")],
                OutputChanges {
                    created: vec![output(SPENT, 90)],
                    spent: vec![output(FUNDED, 100).key()],
                    ..OutputChanges::default()
                },
            ),
            _ => (Vec::new(), OutputChanges::default()),
        };
        Ok(InterpretedBlock {
            outputs,
            ..InterpretedBlock::new(&network(), reference, transactions)?
        })
    }
}

type Index = Indexer<Source, Interpreter, memory::Repository>;

/// An index of `source` over `repository`, reading at most `batch` blocks a
/// pass, and the join a chain pairs from the two.
fn joined(
    repository: &memory::Repository,
    source: Source,
    batch: usize,
) -> (Arc<Index>, PendingJoin<memory::Repository>) {
    let index = Arc::new(Indexer::new(
        source,
        Interpreter,
        repository.clone(),
        SyncConfig::new(&network(), SyncPolicy::new(batch)).expect("a valid policy"),
    ));
    let join = PendingJoin::new(repository.clone(), index.clone());
    (index, join)
}

fn spendable(join: &PendingJoin<memory::Repository>) -> Result<OutputPage, IndexError> {
    block_on(join.list(OutputRequest {
        network: network(),
        address: address("owner"),
        after: None,
        limit: 10,
    }))
}

fn listed(page: &OutputPage) -> Vec<String> {
    page.outputs
        .iter()
        .map(|output| output.id.transaction.id.to_string())
        .collect()
}

/// The three reads over one pass's result: the spend a held block made is
/// applied, the change it created is offered at depth one, and the value it
/// moves is pending rather than settled or missing.
///
/// The repository alone still lists the funding output, which is what makes
/// the join's answer the fold of the held block and not the store's own.
#[test]
fn a_held_block_is_folded_into_every_read_of_the_repository_below_it() {
    let repository = memory::Repository::new();
    let (index, join) = joined(&repository, Source { tip: 6, depth: 1 }, 100);
    block_on(index.watch("owner", &owner(), Birthday::At(BlockPosition(0)))).expect("watch");

    let status = block_on(Indexing::sync(&*index)).expect("the pass");
    assert_eq!(status.checkpoint, Some(block(5)), "block 6 is held");

    let stored = block_on(repository.list(OutputRequest {
        network: network(),
        address: address("owner"),
        after: None,
        limit: 10,
    }))
    .expect("the repository's own page");
    assert_eq!(listed(&stored), ["tx-2"], "the store holds no spend yet");
    assert_eq!(
        listed(&spendable(&join).expect("the joined page")),
        ["tx-6"],
        "the funding output is spent above the line and its change is offered"
    );

    let balance = block_on(join.balance(&owner())).expect("the balance");
    assert_eq!(balance.amount, Decimal::from(100_u64));
    assert_eq!(balance.observed_at, Some(block(5)));
    assert_eq!(balance.pending.outgoing, Decimal::from(10_u64));
    assert_eq!(balance.pending.incoming, Decimal::zero());
    assert_eq!(balance.completeness, Completeness::CompleteHistory);

    let page = block_on(join.history(&owner(), Page::first(10))).expect("the history page");
    let confirmed = page
        .transactions
        .iter()
        .map(|entry| entry.status.clone())
        .collect::<Vec<_>>();
    let pending = page
        .pending
        .iter()
        .map(|entry| entry.status.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        confirmed,
        [TransactionStatus::Confirmed {
            block: block(FUNDED),
            confirmations: 5,
        }]
    );
    assert_eq!(
        pending,
        [TransactionStatus::Pending {
            block: block(SPENT),
            confirmations: 1,
        }]
    );
    assert_eq!(page.completeness, Completeness::CompleteHistory);
}

/// An address still being inspected below the line is offered no output and
/// claims no whole history, through the join exactly as from the repository.
///
/// Eight blocks are committed with nothing watched, then the owner is watched
/// from the first block and one pass reads four of them: the funding output is
/// stored, the page would be stamped with block 8, and the spend in block 6 has
/// not been read for this address. Nothing but the repository's own rows says
/// so, and nothing in the join replaces its answer.
#[test]
fn a_partly_inspected_address_is_refused_its_outputs_through_the_join() {
    let repository = memory::Repository::new();
    let source = Source { tip: 8, depth: 0 };
    let (index, _) = joined(&repository, source.clone(), 100);
    block_on(Indexing::sync(&*index)).expect("the pass that reaches the line");
    drop(index);

    let (bounded, join) = joined(&repository, source.clone(), 4);
    block_on(bounded.watch("owner", &owner(), Birthday::At(BlockPosition(0)))).expect("watch");
    let status = block_on(Indexing::sync(&*bounded)).expect("the pass that reads four blocks");
    let owed = status.backfill.expect("the range is not read out");
    assert_eq!(owed.resume, BlockPosition(4), "blocks 0 to 3 were read");
    assert_eq!(status.checkpoint, Some(block(8)));

    let refused = spendable(&join).expect_err("an output nothing can prove unspent");
    assert_eq!(refused.kind, IndexErrorKind::Conflict);
    assert!(
        refused.retryable,
        "it clears as the catch-up reaches the line"
    );
    assert!(
        refused
            .message
            .contains("not readable while this address's own history"),
        "the inspection refused it: {}",
        refused.message
    );
    let balance = block_on(join.balance(&owner())).expect("the balance");
    assert_eq!(balance.amount, Decimal::from(100_u64));
    assert_eq!(balance.completeness, Completeness::Incomplete);
    let page = block_on(join.history(&owner(), Page::first(10))).expect("the history page");
    assert_eq!(page.completeness, Completeness::Incomplete);
    drop(bounded);

    let (straight, join) = joined(&repository, source, 100);
    let status = block_on(Indexing::sync(&*straight)).expect("the pass that closes the range");
    assert!(status.backfill.is_none(), "the range is read out");

    assert_eq!(
        listed(&spendable(&join).expect("the joined page")),
        ["tx-6"],
        "read through the line, the set is offered, and block 6 spent the funding"
    );
    let balance = block_on(join.balance(&owner())).expect("the balance");
    assert_eq!(balance.amount, Decimal::from(90_u64));
    assert_eq!(balance.completeness, Completeness::CompleteHistory);
}
