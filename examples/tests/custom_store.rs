//! What the `custom_store` scenario claims, asserted without a node.
//!
//! The store under test is the scenario's own file, included here rather than
//! copied, so a regression in it fails this test: `examples/custom_store/sink.rs`
//! implements exactly two ports and the assertions below are what a third party
//! gets for them. The chain is `indexing`'s scripted one, because none of this
//! is about a chain — blocks that each credit one address, and a confirmed line
//! a few blocks below the tip.
//!
//! Four claims, in the order they matter:
//!
//! 1. a store that implements only `WatchStore` and `IndexStore` drives a real
//!    `indexing::Indexer` — it observes, it commits, and it fills in history;
//! 2. `sync` is bounded: one pass commits at most its `SyncPolicy`'s batch;
//! 3. a restart over the same store restores every observation, its birthday and
//!    its recorded progress, and resumes instead of starting again;
//! 4. the store answers the consumer's own questions, and no read port.

#[path = "../custom_store/sink.rs"]
mod sink;

use std::sync::Arc;

use indexing::{
    Address, AssetId, Birthday, BlockInterpreter as _, BlockPosition, BlockSource as _,
    BlockWithOutputs, ChainName, Checkpoint as _, Decimal, FeeSettlement, IndexErrorKind,
    IndexStore as _, Indexer, Indexing as _, MovementId, Network, Status, Subject, SyncConfig,
    SyncPolicy, Transaction, TransactionRef, ValueMovement, Watching as _,
    testing::{Chain, block},
};
use sink::Sink;

/// Blocks the test chain has produced, at positions `0..HEIGHT`.
const HEIGHT: u64 = 12;

/// How far below the tip this chain will never take a block back, so the
/// confirmed line — and therefore the checkpoint a full catch-up reaches — is
/// `HEIGHT - 1 - DEPTH`.
const DEPTH: u64 = 2;

/// Blocks one pass may read. Smaller than `HEIGHT`, so boundedness is visible.
const BATCH: usize = 4;

/// The address every even block credits; each test chooses its observation start.
const FRESH: &str = "address-fresh";

/// The address every odd block credits, observed from a birthday in the past.
const PAST: &str = "address-past";

/// Where the test chain's value comes from; never observed.
const SENDER: &str = "address-sender";

/// The one network this test indexes.
fn network() -> Network {
    Network::new(ChainName::new("probe"), "one")
}

/// The test chain: [`HEIGHT`] blocks and a confirmed line [`DEPTH`] below its
/// tip. Even positions credit [`FRESH`] and odd ones credit [`PAST`], one unit
/// from [`SENDER`] in one transaction, so each address's history is
/// interleaved with the other's rather than contiguous; a block holds nothing
/// for an address its credit does not reach.
///
/// It counts the blocks it serves, which is how the restart test tells resuming
/// from a restored watch list apart from starting the same work again: a store
/// that lost a checkpoint or a progress row makes the index read blocks it has
/// already read, and nothing in the store's own answers says so.
fn chain() -> Chain {
    let chain = Chain::dense(network(), HEIGHT - 1).confirming(DEPTH);
    for position in 0..HEIGHT {
        let to = address(if position % 2 == 0 { FRESH } else { PAST });
        let draft = Transaction {
            id: TransactionRef::new(network(), format!("tx-{position}")),
            movements: vec![ValueMovement::transfer(
                MovementId("transfer:0".to_owned()),
                AssetId::native(network().chain),
                Decimal::from(1_u64),
                address(SENDER),
                to,
            )],
            fee: None,
            order: 0,
            status: Status::Included {
                block: block(position),
            },
        };
        chain.include(&block(position), draft);
    }
    chain
}

/// One address of the test network.
fn address(text: &str) -> Address {
    Address::new(network(), text)
}

/// One watchable subject: an address and the asset it is read in.
fn subject(text: &str) -> Subject {
    Subject {
        address: address(text),
        asset: AssetId::native(network().chain),
    }
}

/// One index over `store`, exactly as the scenario builds one over a chain: the
/// store is handed over as `Arc`, because it is not `Clone` and does not need to
/// be.
fn index(store: &Arc<Sink>, chain: &Chain) -> Indexer<Chain, Chain, Arc<Sink>> {
    Indexer::new(
        chain.clone(),
        chain.clone(),
        Arc::clone(store),
        SyncConfig::new(&network(), SyncPolicy::new(BATCH)).expect("a valid policy"),
    )
}

/// The checkpoint position the index reached, or `None` before its first commit.
async fn reached(index: &Indexer<Chain, Chain, Arc<Sink>>) -> Option<u64> {
    index
        .checkpoint(&network())
        .await
        .expect("the store answers its own checkpoint")
        .map(|block| block.position.0)
}

/// Drives `index` until nothing more changes, at most `passes` times.
async fn drive(index: &Indexer<Chain, Chain, Arc<Sink>>, passes: usize) {
    let mut previous = None;
    for _ in 0..passes {
        index.sync().await.expect("a pass over the test chain");
        let current = reached(index).await;
        if current == previous {
            return;
        }
        previous = current;
    }
}

/// Claim 2: one pass commits at most its policy's batch, and returns.
///
/// If `sync` ever became a loop of its own, this is the assertion that fails:
/// the whole chain is `HEIGHT` blocks and one pass may read `BATCH` of them.
#[tokio::test]
async fn one_pass_commits_at_most_its_batch() {
    let store = Arc::new(Sink::new());
    let chain = chain();
    let unit = index(&store, &chain);
    unit.watch("fresh", &subject(FRESH), Birthday::At(BlockPosition(0)))
        .await
        .expect("an observation from genesis");

    unit.sync().await.expect("one bounded pass");

    let batch = u64::try_from(BATCH).expect("a representable batch");
    assert_eq!(
        reached(&unit).await,
        Some(batch - 1),
        "one pass must commit exactly the blocks its batch allows"
    );
}

/// Fresh is after the live tip, even when the durable line is lower. Existing
/// blocks must not become historical credits for a future-only observation.
#[tokio::test]
async fn a_fresh_observation_starts_after_the_live_tip() {
    let store = Arc::new(Sink::new());
    let chain = chain();
    let unit = index(&store, &chain);
    let watch = unit
        .watch("fresh", &subject(FRESH), Birthday::Fresh)
        .await
        .expect("a fresh observation");
    assert_eq!(watch.start, BlockPosition(HEIGHT));
    assert_eq!(chain.fetched().len(), 0);
    assert_eq!(store.observed(&network()), [("fresh".into(), watch.start)]);

    drive(&unit, 8).await;
    assert_eq!(reached(&unit).await, Some(HEIGHT - 1 - DEPTH));
    assert!(store.landings(&network()).is_empty());
}

/// Claims 1, 3 and 4 together, because they are one scenario: observe with a
/// birthday in the past, catch up, then restart over the same store.
///
/// The restart is the assertion that catches the most. Every index holds its
/// watch list in memory — the publication handshake captures it synchronously —
/// so after the drop the store is the only thing left. Each `assert_eq!` below
/// names the way it can go wrong: a birthday the store rounded, progress it
/// dropped, a checkpoint it forgot, or a fold that ran the same blocks twice.
#[tokio::test]
async fn a_restart_over_the_same_store_restores_every_observation() {
    let store = Arc::new(Sink::new());
    let chain = chain();
    let unit = index(&store, &chain);
    unit.watch("fresh", &subject(FRESH), Birthday::At(BlockPosition(0)))
        .await
        .expect("an observation from genesis");
    drive(&unit, 8).await;
    let confirmed = HEIGHT - 1 - DEPTH;
    assert_eq!(
        reached(&unit).await,
        Some(confirmed),
        "a store with two ports must let the index reach the confirmed line"
    );

    // A birthday the network has already passed: this is the case that owes a
    // catch-up, and therefore the only one with progress to restore.
    let past = unit
        .watch("past", &subject(PAST), Birthday::At(BlockPosition(2)))
        .await
        .expect("an observation from the past");
    assert_eq!(
        past.start,
        BlockPosition(2),
        "the birthday is kept as given"
    );
    drive(&unit, 8).await;
    assert_eq!(
        store.catching_up(&network()),
        Vec::new(),
        "the catch-up must reach the line the birthday asked from"
    );

    // Claim 4: the consumer's own questions, answered by its own store. No
    // history page, no balance, no cursor — three plain facts.
    let landed = store.landings(&network());
    assert_eq!(
        landed.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["fresh", "past"],
        "both observed addresses must have had value land on them"
    );
    let credited: Vec<_> = landed
        .iter()
        .map(|(id, landing)| (id.as_str(), landing.blocks))
        .collect();
    assert_eq!(
        credited,
        [("fresh", 5), ("past", 4)],
        "each address is credited once per block that named it, and never twice"
    );

    // Claim 3. Everything above is dropped; the store is not.
    let observed = unit.watched(&network()).await.expect("the watch list");
    let read = chain.fetched().len();
    drop(unit);
    let restarted = index(&store, &chain);
    assert_eq!(
        restarted.watched(&network()).await.expect("the watch list"),
        observed,
        "a restart must read back every observation, with the birthday it was given"
    );
    assert_eq!(
        reached(&restarted).await,
        Some(confirmed),
        "a restart must resume from the checkpoint the store holds"
    );
    assert_eq!(
        store.catching_up(&network()),
        Vec::new(),
        "a restart must not re-open a range the store already recorded"
    );

    // The blocks above the confirmed line are held in memory by design, so a
    // start re-reads exactly those and nothing else. Anything below the line is
    // read again only because the store forgot its checkpoint or a progress row,
    // which is what this arithmetic is here to catch — no answer the store gives
    // back would show it.
    drive(&restarted, 8).await;
    let pending = usize::try_from(DEPTH).expect("a representable depth");
    assert_eq!(
        chain.fetched().len(),
        read + pending,
        "a restart must resume: nothing below the confirmed line may be read twice"
    );
    assert_eq!(
        store.landings(&network()),
        landed,
        "a pass after the restart must land nothing again: the chain has not moved"
    );
}

/// The idempotency rule of the second required port, for the store that makes it
/// hardest.
///
/// `IndexStore::commit` is "idempotent by insert-or-verify": a re-presented batch
/// is checked against the rows already held. A store that holds no history row has
/// nothing to check, so it owes the same guarantee by other means or a retried
/// batch counts twice. This store remembers which `(address, block)` pairs it has
/// folded, and this is the test of that — the only way to present one batch twice
/// is to call the port directly, which is also the most a third party has to be
/// told.
///
/// The batch carries **no progress claim**, which is the case that is idempotent
/// verbatim. A batch that advances an observation makes its own `expected` stale
/// the moment it lands, so re-presenting *that* one is refused retryably and the
/// producer rebuilds it against the progress the store now holds
/// (`IndexStore::commit`, "Recovery is reload-and-redo").
#[tokio::test]
async fn one_batch_presented_twice_is_folded_once() {
    let store = Arc::new(Sink::new());
    let chain = chain();
    let unit = index(&store, &chain);
    unit.watch("fresh", &subject(FRESH), Birthday::At(BlockPosition(0)))
        .await
        .expect("an observation from genesis");
    drive(&unit, 8).await;
    let landed = store.landings(&network());

    let checkpoint = unit
        .checkpoint(&network())
        .await
        .expect("a checkpoint")
        .expect("a committed block");
    let genesis = chain
        .blocks(BlockPosition(0), BlockPosition(0), 1)
        .await
        .expect("the genesis block");
    let block = chain
        .inspect(&genesis[0], &[address(FRESH)])
        .expect("one interpreted block");
    let canonical = block;
    let batch = || {
        indexing::IndexedBatch::new(
            network(),
            Some(checkpoint.clone()),
            vec![canonical.clone()],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("a valid batch below the line")
    };

    store.commit(batch()).await.expect("the first batch");
    store.commit(batch()).await.expect("the same batch again");

    assert_eq!(
        store.landings(&network()),
        landed,
        "a batch a store already wrote must not be folded a second time"
    );
}

/// The one refusal the required ports owe, and the reason the index can trust
/// the list it reads back: an address already observed cannot be claimed by a
/// second identity, because accepting it would retarget the first one's
/// progress.
#[tokio::test]
async fn a_second_identity_cannot_claim_an_observed_address() {
    let store = Arc::new(Sink::new());
    let chain = chain();
    let unit = index(&store, &chain);
    unit.watch("fresh", &subject(FRESH), Birthday::Fresh)
        .await
        .expect("a fresh observation");

    let refusal = unit
        .watch("other", &subject(FRESH), Birthday::Fresh)
        .await
        .expect_err("a second identity for one address");

    assert_eq!(refusal.kind, IndexErrorKind::Conflict);
    assert_eq!(
        store.observed(&network()),
        [("fresh".to_owned(), BlockPosition(HEIGHT))],
        "a refused registration must leave the store exactly as it was"
    );
}

/// Two credits in one block to one observed address sum, and the pair is still
/// folded once when the batch is presented again.
///
/// The two rules pull against each other, which is why they are asserted
/// together: `BlockCredit::id` is an identity *within* a block, so a block may
/// credit one address more than once, while the store's `(address, block)` guard
/// is what makes a re-presented batch harmless for a store that keeps no
/// history row to verify. Deduplicating per credit loses the second amount;
/// deduplicating per pair without aggregating first loses it too.
#[tokio::test]
async fn every_credit_a_block_makes_to_one_address_is_summed_once() {
    let store = Arc::new(Sink::new());
    let chain = chain();
    let unit = index(&store, &chain);
    unit.watch("fresh", &subject(FRESH), Birthday::At(BlockPosition(0)))
        .await
        .expect("an observation from genesis");
    drive(&unit, 8).await;
    let before = store
        .landings(&network())
        .into_iter()
        .find(|(id, _)| id == "fresh")
        .expect("the observed address was credited")
        .1;

    let checkpoint = unit
        .checkpoint(&network())
        .await
        .expect("a checkpoint")
        .expect("a committed block");
    let credited = |name: &str, amount: u64| indexing::BlockCredit {
        id: MovementId(name.to_owned()),
        asset: AssetId::native(network().chain),
        amount: Decimal::from(amount),
        to: address(FRESH),
    };
    let canonical = indexing::BlockWithOutputs {
        credits: vec![credited("credit:0", 3), credited("credit:1", 5)],
        ..BlockWithOutputs::new(&network(), checkpoint.clone(), Vec::new())
            .expect("unique transaction ids")
    };
    let batch = || {
        indexing::IndexedBatch::new(
            network(),
            Some(checkpoint.clone()),
            vec![canonical.clone()],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("a valid batch at the line")
    };

    store.commit(batch()).await.expect("the first batch");
    let folded = store
        .landings(&network())
        .into_iter()
        .find(|(id, _)| id == "fresh")
        .expect("the observed address was credited")
        .1;
    assert_eq!(
        folded.credited,
        Decimal::total([&before.credited, &Decimal::from(8_u64)]),
        "both credits of one block must be summed, not just the first"
    );

    store.commit(batch()).await.expect("the same batch again");
    assert_eq!(
        store
            .landings(&network())
            .into_iter()
            .find(|(id, _)| id == "fresh")
            .expect("the observed address was credited")
            .1,
        folded,
        "a batch the store already folded must not be folded a second time"
    );
}
