# indexing: reorg-safe, chain-neutral block synchronization

## Why this crate exists

Turns chain-native blocks into canonical transaction history, a checkpoint,
and (for UTXO chains) live outputs, for the addresses it observes. It owns the
algorithm, the list of what is observed, and the contracts around them: what a
chain must provide, what a store must persist, what an application may read,
and what it is told when a block commits.

One network's index is one owner. [`Service`] holds that network's watch list,
the interpreted blocks above its confirmed line, its chain adapter and its
reference to the store; [`Indexing::sync`] is a bounded procedure over those
fields, so no watch set is passed to it and no reader holds a second copy of the
overlay it folds. [`Supervisor`] is the optional half: it schedules passes,
applies retry delays, quarantines and resumes, and joins on shutdown, by calling
[`Indexing::sync`] -- never by reimplementing what it does (`docs/design/030 §8 (ex-032)`).

Nothing it stores is ever undone. Each chain states the newest block it will
call irreversible ([`ChainTip::confirmed_tip`]); the store receives that block
and everything below it, and the blocks above it are held interpreted in
memory, where a reorg is a truncation. So a read can answer both of an
operator's questions at once — what has settled, and what is on its way —
and a reorg costs no storage work at all (`docs/design/030 §8 (ex-025)`).

It is one crate with two parts -- `storage/`, the ports and the two embedded
stores that answer them, and the root -- and four cargo features decide how much
of it a dependent compiles. **Two of those ports are required**
([`WatchStore`] + [`IndexStore`] = [`Repository`]); everything else a store can
answer is a consumer capability, and a store that answers none of them still
indexes. It owns no chain code (`chain-*`) and no wallets
(`wallets`). It is not a service, an event feed, or a raw-block archive.

## What each feature adds

| Feature | Default | Adds | Costs |
|---|---|---|---|
| *(none)* | always | the vocabulary a chain produces, every store port, the reads an application calls, the polling cadence ([`Schedule`], [`Schedules`]), and the one-network owner that runs a pass over its own state: [`Service`], [`Index`], [`SyncConfig`] | `base`, `futures-channel` and `num-bigint` only -- no store, no async runtime |
| `sqlite` | yes | [`sqlite::Database`] and [`sqlite::Repository`]: every port on one embedded SQL file, for every network in it | `rusqlite` with SQLite bundled as C, `tokio/rt` (each operation is one `spawn_blocking`, so a caller needs a Tokio runtime) |
| `memory` | no | `memory::Repository`: every port on ordinary maps, for every network in one process -- the second shipped store, for a process that wants nothing on disk | nothing: no file, no C library, no async runtime |
| `supervisor` | yes | [`Supervisor`], [`SyncControl`], [`SyncMonitor`], [`NetworkEvent`]: one task per [`SyncUnit`] | `tokio` with `rt`, `time` and `macros` |

`apps/api`, `wallets`, `sdk/config` and all three chain crates take
`default-features = false`, so a dependent that asks for no store and no runner
does not get one through them: Cargo unifies features across the graph, and one
crate leaving the defaults on would have re-enabled `sqlite` -- a bundled C
build -- underneath an embedder who declined it. A third-party graph of
`indexing` + the chain crates pulls in no `rusqlite`; the Tokio it does pull in
comes from the chains' own HTTP transport, not from here.

## Concepts

| Concept | Type | Implemented or provided by |
|---|---|---|
| network (what an index is partitioned by) | [`Network`] (`(chain, network)`), accepted through [`Networked`], checked with [`require_chain`] | values from `base` |
| whose history, balance or observation | [`Holder`] (address + asset, plus the provided [`Holder::validate`]: one asset of the address's own chain, checked once for every query over a holding); [`Subject`] is one with no wallet behind it | `wallets::Wallet` is one; the caller builds the other |
| what is observed | [`Watching::watch`]`(id, &dyn Holder, `[`Start`]`)` -> [`Watch`], read back by [`Watching::watched`]; [`AddressFilter`] is what one pass needs of a row, deduplicated by address | this crate owns the hot list; [`WatchStore`] is its durable mirror, keyed `(address, asset)` so one address may be observed once per asset |
| chain block | [`IndexedBlock`] | chain crate (native block type) |
| fetch | [`BlockSource`], with its two lines [`ChainTip`] | chain crate (`chain_*::Source`) |
| interpret | [`BlockInterpreter`] -> [`InterpretedBlock`] | chain crate |
| how a chain settles its fee | [`FeeSettlement`], stated once by [`BlockInterpreter::settlement`]: a UTXO fee is already inside `Σ inputs − Σ outputs`, an account chain's is charged beside the value | chain crate |
| a settled holding | [`Balances::balance`] -> [`Balance`] ([`Completeness`]), folded by [`Ledger`] over the movements the store holds | [`BalanceStore`]; the overlay adds [`PendingValue`] |
| persist (required) | [`Repository`] = [`WatchStore`] (`add`, `all`, `forget`, `reanchor`) + [`IndexStore`] (`checkpoint`, `commit`) | [`sqlite::Repository`] (feature `sqlite`), `memory::Repository` (feature `memory`), or a third party's own sink |
| persist (consumer capabilities) | [`BalanceStore`], [`OutputStore`], [`Purging`] -- separate traits, so needing none of them is a legal store rather than a stubbed one. Querying history is not a port: it is the repository's own [`ConfirmedHistory`] | the same two stores; `tests/minimum_contract.rs` proves a sink with none of them indexes |
| how far each observation has been inspected | [`WatchProgress::indexed_through`] -- one contiguous, hash-verified block per observation -- advanced by [`Progress`] inside the commit that proves it. [`WatchProgress::caught_up`] is the one predicate behind both the pass's selection and the [`Completeness`] gate | this crate's pass; the store compares and records |
| history an address was watched too late to see | the same [`IndexStore::commit`], carrying blocks at or below the checkpoint: the affected observation is simply further behind. Reported as [`SyncStatus`]`::backfill` | this crate's pass; the store writes the batch |
| policy | [`SyncPolicy`] -> [`SyncConfig`] | chain `IndexerParams` defaults |
| one chain | [`Service`] -- that network's watches, its overlay of blocks above the confirmed line, its source and interpreter, and its store reference, with the bounded pass as a procedure over them. **A chain returns this, unwrapped**: each chain's `Indexer` type alias is this type, so nothing forwards a capability to a field, and its three query readers ([`ObservedHistory`], [`ObservedBalance`], [`ObservedOutputs`]) are handed out by it rather than implemented on it | this crate |
| what a caller drives | [`Indexing`] (`network`, `listen`, `sync`, plus [`Checkpoint`], [`Watching`], [`Backfilling`]) -- one network, one [`SyncStatus`], answerable by a [`Repository`] alone. **No query is on it**: [`ObservedHistory`] joins a repository's [`ConfirmedHistory`] to that index's overlay, [`ObservedOutputs`] does the same for the spendable set, and [`ObservedBalance`] does it for the fold | [`Service`], which is what each chain's own constructor returns |
| many chains | [`Index`] -- the one many-network value: every unit is asked on every [`Index::sync`], so one chain's failure never costs the others their pass, with or without a supervisor. Nothing nests: an `Index` is not an [`Indexing`] | this crate |
| unit of failure isolation | [`SyncUnit`] ([`Index::units`]) | this crate; one per composed chain |
| what a network is doing, and why | [`NetworkState`], [`NetworkHealth`] (carrying the [`IndexError`] that holds *forward* indexing back, and the [`BackfillStatus`] it still owes below its checkpoint), [`SyncHealth`] | [`Supervisor`] fills them; an HTTP layer reads them. Readiness follows forward indexing: an owed range is reported, never gating (`docs/design/024` section 7) |
| watch <-> commit handshake | private to [`Service`]: a running watch and the pass on the supervisor's task are two actors on one list | this crate |
| scheduling, retry delays, quarantine, joining on shutdown | [`Supervisor`] (feature `supervisor`), which calls [`Indexing::sync`] and holds no second scan or commit | this crate; optional, and the crate names no async runtime without it |
| what changed just now | [`Listener::committed`]([`BlockCommit`]), registered with [`Indexing::listen`] | application |
| where the facts live, with a veto | [`IndexStore`], and whichever consumer capabilities the caller reads | the bundled [`sqlite::Repository`], or the application's own store |
| reads | [`Checkpoint`], [`Balances`] (`&dyn Holder` -> [`Balance`]) on [`ObservedBalance`], [`OutputStore`] on [`ObservedOutputs`], and [`History`] (`&dyn Holder` + [`Page`] -> [`TransactionPage`]) on [`ObservedHistory`] | one overlay is folded into all four -- the same blocks the next pass will commit, because there is one collection of them; `Index` routes none of the three queries, only [`Checkpoint`]; stores hold the confirmed half |

Every trait method returns [`BoxFuture`] instead of being an `async fn`: the
traits stay object-safe (`Arc<dyn Indexing>`, `&dyn Holder`), and with no
features on, the crate names no async runtime at all. [`Listener::committed`]
is the exception and is synchronous on purpose: a hook that cannot await
cannot slow a pass by awaiting, and queueing on its behalf would need a
spawner this crate deliberately does not have.

SQLite is synchronous, so the store hands **one whole operation** -- never one
row -- to `tokio::task::spawn_blocking`, and one writer and many readers run at
once: the write-ahead log allows it and a bounded connection pool takes it. A
read port holds one snapshot for its whole operation, which is what makes a page
consistent with the checkpoint it carries without reading the checkpoint twice
and refusing the page when it moved. A block commits inside one
`BEGIN IMMEDIATE` transaction, which is also why there is no compare-and-set:
the checkpoint read and the writes it authorizes cannot be separated.

## Public API map

| Group | Items |
|---|---|
| construct | [`Index::over`] (the assembly flow: `Index::over(store).chain(..)?.chain(..)?.build()?`), [`Index::of`], [`Service::new`], [`SyncConfig::new`], [`SyncPolicy::new`] |
| use | [`Watching::watch`], [`Watching::watched`], [`Watching::unwatch`], [`Indexing::listen`], [`Indexing::sync`], [`Indexing::network`], [`Index::networks`], [`Checkpoint::checkpoint`], [`History::history`], [`Balances::balance`], [`OutputStore::list`] |
| implement (chain) | [`IndexedBlock`], [`ChainTip`], [`BlockSource`], [`BlockInterpreter`] (including [`BlockInterpreter::settlement`], where the chain states whether its [`NetworkFee`] is already inside its movements), producing [`InterpretedBlock`], [`ObservationDraft`], [`ValueMovement`], [`NetworkFee`], [`OutputChanges`] |
| implement (store, **required**) | [`WatchStore`] (atomic batch `add`, one-snapshot `all` -> [`WatchProgress`], `forget`) and [`IndexStore`] (`checkpoint`, plus **one** write: [`IndexStore::commit`] consumes one [`IndexedBatch`] -- the blocks, the checkpoint they were planned under and the [`Progress`] they prove -- and answers the unmatched required spends rather than refusing them. Whether the line moves is [`IndexedBatch::checkpoint`]'s answer, derived from the blocks, so a batch below the line is the same call) |
| implement (store, optional) | [`ConfirmedHistory`] ([`HistoryQuery`] -> [`CanonicalPage`], joined to an overlay by [`ObservedHistory`]), [`BalanceStore`] ([`BalanceQuery`] -> [`Balance`], folded with [`Ledger`]), [`OutputStore`] ([`OutputRequest`] -> [`OutputPage`]), [`Purging`] ([`Purging::purge_network`] -> [`PurgedRows`], the offline rescan) |
| implement (app) | [`Holder`] (`address` + `asset`; [`Holder::validate`] comes provided, so no query restates the chain check -- a wallet already is one), [`Listener`] |
| values | [`Watch`], [`Start`], [`Subject`], [`BlockCommit`], [`CanonicalBlock`], [`CanonicalTransaction`], [`ObservedTransaction`], [`TransactionStatus`], [`Balance`], [`Completeness`], [`FeeSettlement`], [`PendingValue`], [`Page`], [`HistoryPosition`], [`IndexedOutput`], [`OutputId`], [`OutputKey`], [`WatchProgress`], [`Progress`], [`IndexedBatch`], [`BackfillStatus`], [`SyncStatus`], [`SyncPhase`], re-exported `base` values |
| health | [`NetworkState`], [`NetworkHealth`], [`SyncHealth`] ([`SyncHealth::ready`], [`SyncHealth::settled`], [`SyncHealth::apply`]), [`SyncUnit`], [`Index::units`] |
| errors | [`IndexError`], [`IndexErrorKind`], [`SourceError`] |
| store (feature `sqlite`) | [`sqlite::Database::open`] (one file), [`sqlite::Repository::new`] (every port, every network in that file) |
| driver (feature `supervisor`) | [`Supervisor::new`], [`Supervisor::run`], [`Supervisor::control`], [`SyncControl::resume`], [`SyncMonitor`], [`NetworkEvent`] |
| store (feature `memory`) | `memory::Repository::new` (every port, every network, in this process's memory and nowhere else) |

## Lifecycle

1. Open the file once with [`sqlite::Database::open`] and bind a repository to
   each network with [`sqlite::Repository::new`] (it takes `&impl Networked`,
   so a chain's own network type is accepted directly).
2. Assemble one index over that store: `Index::over(repository)`, one
   `.chain(|store| chain_*::indexer(..))?` per chain — the `?` is the chain's
   own refusal, and nothing after it is constructed — then `.build()?`, which
   refuses a duplicate network **without launching a task**. A caller that composed its chains some
   other way -- asynchronously, or through `wallets::Composition` -- hands the
   finished indexers to [`Index::of`] instead.
3. Watch what you want observed: [`Watching::watch`] takes an id, any
   [`Holder`] and a [`Start`], writes the durable row, and returns before the
   caller files a key for it.
4. Call [`Index::sync`] in a loop, or hand the [`Index`] to
   [`Supervisor`] so one chain's failure cannot stop the others. There is no
   selection argument: each pass reads the source's two tips, then its own
   watch list (in that order, so an address watched meanwhile is never
   skipped), drops any held block the source no longer calls canonical, reads
   at most `batch_size` new blocks into the overlay, and commits everything at
   or below the confirmed line. A new address starts at its birthday; blocks
   before it are not inspected, and blocks already held above the line are
   inspected again so the new address is not missed.
5. Read [`Checkpoint`] (the confirmed line), [`Balances`] (the settled fold,
   plus what is still above the line), and [`History`] on an
   [`ObservedHistory`] over this index (confirmed transactions, plus the
   pending ones on the first page). Register a
   [`Listener`] to be told what each commit changed.

A reorg above the line changes no stored byte. A reorg *below* it means the
chain took back a block the source promised was irreversible: that is
[`IndexErrorKind::ConfirmedReorg`], it quarantines the network, and only an
offline rescan repairs it. It is impossible on a chain with finality.

That rescan is [`Purging::purge_network`]: it removes one network's checkpoint,
history, movements, live outputs and recorded watch progress in one atomic step,
reports the counts as [`PurgedRows`], and touches neither the observations
themselves nor any other network. The next pass then finds no checkpoint and
re-indexes from the earliest observed birthday. Run it with nothing else running --
`payment-server <config.json> --rescan <chain>/<network>` is the server's
offline subcommand for exactly that -- because a network emptied under a live
reader serves an empty history that looks authoritative.

## Usage

Plugging in a chain: a source, an interpreter, and a store become an indexer.
Chain crates do this inside their own `indexer(params)` constructor, which
returns this `Service`: each chain's `Indexer` type alias is one.

What the store has to answer is [`Repository`] -- [`WatchStore`] plus
[`IndexStore`], registration and committing -- and nothing else. A store that
answers no balance, no history page and no output still indexes through it; the
consumer capabilities below are what *this* workspace's own consumers read, and
each one is a separate trait so that needing none of them is a legal store
rather than a stubbed one.

```rust
use indexing::{
    BlockInterpreter, BlockSource, IndexError, Networked, Repository, Service, SyncConfig,
    SyncPolicy,
};

fn chain_indexer<S, I, R>(
    source: S,
    interpreter: I,
    repository: R,
    network: &impl Networked,
) -> Result<Service<S, I, R>, IndexError>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
    R: Repository + Clone,
{
    // One number is left: how many blocks one pass reads. Where the confirmed
    // line sits is the source's answer, not a setting.
    let config = SyncConfig::new(network, SyncPolicy::new(100))?;
    // One store, every network: the same value is handed to every chain, and
    // each operation names the network it is about. A store that cannot be
    // cloned is passed as `Arc<MyStore>`, which implements each port by
    // forwarding to the one instance.
    Ok(Service::new(source, interpreter, repository, config))
}
```

Using one chain's indexer. Every read and watch below is the same call on a
multi-chain [`Index`], which routes it to the unit that owns the network; what
differs is progress, because a child owns one network and an `Index` owns every
one of them: `Indexing::sync` answers one [`SyncStatus`], `Index::sync` answers
one per unit.

A query is not on the indexer at all. A page is the repository's own
[`ConfirmedHistory`] query joined to the blocks one index holds above its line,
which is [`ObservedHistory`]; a balance is that store's fold completed by the
same blocks, which is [`ObservedBalance`]; a spendable set is
[`ObservedOutputs`]. A consumer holds one of
each per network and asks the one that owns the subject.

```rust
use std::sync::Arc;

use indexing::{
    Address, AssetId, Balances, BlockCommit, BlockPosition, History, IndexError, Indexing,
    Listener, Page, Start, Subject,
};

/// The reaction hook: synchronous, cannot fail, cannot veto. A veto belongs in
/// an `IndexStore`, which is the only thing that runs inside the commit.
struct Credits;

impl Listener for Credits {
    fn committed(&self, commit: &BlockCommit) {
        println!(
            "{} at {}: {} transaction(s), {} new output(s), {} unknown spend(s)",
            commit.network.chain,
            commit.block.height.0,
            commit.transactions.len(),
            commit.outputs.created.len(),
            commit.unknown_spends.len(),
        );
    }
}

async fn follow(
    indexer: &dyn Indexing,
    history: &dyn History,
    balances: &dyn Balances,
    address: Address,
) -> Result<(), IndexError> {
    let asset = AssetId::native(address.network.chain.clone());
    let subject = Subject { address: address.clone(), asset: asset.clone() };
    // A start below the checkpoint is honoured, not discarded: the pass fills
    // those blocks in below the line, bounded by `SyncPolicy`, and until it
    // has, `sync` reports `CatchingUp` and no balance claims `Complete`.
    indexer.watch("deposits", &subject, Start::At(BlockPosition(0))).await?;
    indexer.listen(Arc::new(Credits));

    let status = indexer.sync().await?;
    println!("{:?}: {:?} at {:?}", status.network, status.phase, status.checkpoint);
    for watch in indexer.watched(&address.network).await? {
        println!("{} from {:?}", watch.address.value, watch.start);
    }

    let page = history.history(&subject, Page::first(50)).await?;
    for transaction in page.transactions.iter().chain(&page.pending) {
        println!("{:?} {:?}", transaction.transaction_id, transaction.status);
    }

    // The subject is the same object, and so is a `wallets::Wallet`: a fold
    // over indexed movements needs no key. Every amount is atomic.
    let balance = balances.balance(&subject).await?;
    println!(
        "{} settled at {:?} ({:?}); {} arriving, {} leaving",
        balance.amount,
        balance.observed_at.map(|block| block.height.0),
        balance.completeness,
        balance.pending.incoming,
        balance.pending.outgoing,
    );
    Ok(())
}
```

## The three notification surfaces

Storage and callback stay two concepts, because only one of them can veto.

| Surface | Question it answers | Timing | Can veto | Count |
|---|---|---|---|---|
| [`WatchStore`] + [`IndexStore`], and whichever consumer capabilities the caller reads | where do the facts live | inside the commit | **yes** — returning `Err` aborts the pass | exactly one |
| [`Listener::committed`] | what changed just now | after the commit | no | zero or more |
| [`SyncMonitor::network_changed`] | is this chain healthy | between passes | no | exactly one |

A caller who wants **fully custom storage** implements [`WatchStore`] and
[`IndexStore`] -- six methods, `add`/`all`/`forget`/`reanchor` and
`checkpoint`/`commit` -- and holds the result against
`WatchContract` and `BackfillContract` from `sdk/indexing/tests/contracts/`, the
same scenarios both bundled stores run; each consumer capability it also chooses
to offer brings its own contract (`HistoryContract`, `BalanceContract`,
`SpendContract`, `PurgeContract`). Those scenarios are integration-test code
rather than crate API, so a third party reads the shape there rather than
linking them. A store that cannot be cloned is passed as `Arc<MyStore>`. A store is a constructor argument, never a
runtime registration: the first pass captures the store's checkpoint, and a
store supplied later would answer a different one than the plan and the overlay
already agreed on.

A caller who wants to **keep the bundled store and still react** registers a
[`Listener`]. Delivery is **at most once, and never retracted**: one call per
committed block, oldest first within a network, after the store returned;
nothing above the confirmed line is ever announced, so a reorg above it
announces nothing; and a crash between the commit and the call loses the
notification. A listener that must not miss a block reads [`Checkpoint`] when
it starts and catches up through [`History`] — this is a wake-up, not a log.

## Related crates

| Layer (`linter.toml`) | Crates | Relation |
|---|---|---|
| base | `base` | canonical values re-exported here ([`Address`], [`Network`], [`BlockRef`], [`AssetId`], [`BoxFuture`], ...) |
| wallets | `wallets` | its `Wallet` is a [`Holder`]; its `Composition` collects one [`History`] reader per network. Nothing here calls back into it |
| chains | `chain-bitcoin`, `chain-ethereum`, `chain-solana` | implement source and interpreter; alias [`Service`] as their `Indexer` |
| api | `payment-api` | takes it with `default-features = false`; serves [`SyncHealth`] and the read traits |
| server | `payment-server` | composes one indexer per chain into a [`Index`], opens the SQLite file, and runs the [`Supervisor`] |

Design notes: [`docs/INDEXING.md`](../../docs/INDEXING.md) (semantics, reorgs,
pagination, the stored layout), [`ARCHITECTURE.md`](../../ARCHITECTURE.md).
