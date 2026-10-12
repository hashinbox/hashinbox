# indexing

An embedded indexer: it follows the addresses you watch and writes their
confirmed transactions, balance movements and spendable outputs to a repository
you supply. It is a library you call, not a service you deploy, and it survives
restarts and reorgs.

## Usage

```rust
use std::sync::Arc;

use indexing::{
    Address, AssetId, Birthday, BlockPosition, Indexers, IndexError, Indexing, Subject, Watching,
};

/// `chain` is a chain crate's indexer, such as `chain_bitcoin::indexer`'s,
/// built over the one repository every network shares.
async fn run(chain: Arc<dyn Indexing>, deposit: Address) -> Result<(), IndexError> {
    // One indexer per network, all over one repository.
    let index: Indexers = Indexers::of(vec![chain])?;

    // A watch is an id, an address, an asset and the first block to inspect.
    let asset = AssetId::native(deposit.network.chain.clone());
    let subject = Subject { address: deposit, asset };
    index.watch("deposits", &subject, Birthday::At(BlockPosition(0))).await?;

    // One bounded pass per network. Call it in your own loop.
    for unit in index.units() {
        let status = unit.sync().await?;
        println!("{:?}: {:?} at {:?}", status.network, status.phase, status.checkpoint);
    }
    Ok(())
}
```

A repository implements two ports, and nothing else is required:

| Port | Methods | Holds |
| --- | --- | --- |
| `WatchStore` | `add`, `all`, `forget` | the watches and how far each was inspected |
| `IndexStore` | `checkpoint`, `commit` | the confirmed line and the one atomic write a pass makes |

A `Watch` is one wallet's address observed in one asset from a birthday — one
`wallet_index` row carrying everything indexing reads. Wallets themselves (address,
label, key) are the `wallets` crate's (`WalletRepository`); the shared schema's
foreign key requires a wallet row filed before its watch, and indexing code never
reads that table. A `Watch` carries no key. `forget` (the durable half of `unwatch`)
deletes the observation and, atomically, its indexed facts (ledger rows, outputs,
memberships, transactions nothing else names); facts are only written for watched
(address, asset) pairs, and watching again reads the range from the birthday then given. A purge
resets progress and totals and keeps every watch.

Two repositories ship with the crate: `sqlite::Repository` (feature `sqlite`, on
by default) and `memory::Repository` (feature `memory`). The `driver`
feature, on by default, adds `Driver`, which runs `sync` for you with retry
and per-network quarantine. Balance, history and output queries belong to the
repository, not to the index. The `testing` feature, off by default and for
dev-dependencies only, adds `testing::Chain`: one scripted chain double that is
a `BlockSource`, `ChainTips` and `BlockInterpreter` at once.

## SQLite

`sqlite::Database::open` is async, on `sqlx` and Tokio. It takes an absolute
path and only opens a pool: it applies no schema and checks no version.
Executing `schema/sdk.sql` (at the repository root) on a new file is the
caller's job, together with any tables of the application's own. There are no
migrations. Read and write the application's tables through `Database::pool`;
indexing's tables belong to the repository.

```rust
use indexing::sqlite::{Database, Repository};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Opens a new file and executes `schema`: `schema/sdk.sql`, plus the
/// application's own tables.
async fn create(path: &std::path::Path, schema: &str) -> Result<Repository> {
    let database = Database::open(path).await?;
    sqlx::raw_sql(schema).execute(database.pool()).await?;
    Ok(Repository::new(database))
}
```

## Reads

Balance and history are not ports: the shipped stores answer them with
inherent methods (`sqlite::Repository::balance`/`history`, the same on
`memory::Repository`), each one snapshot that says how far behind its line the
address's inspection is; a custom store brings its own. The one narrow port is for the
SDK's own consumer — `OutputStore` (Bitcoin's coin selection,
`chain_bitcoin::sender::Spendable`) — and an application completes its own
stored reads with the blocks an index holds above its confirmed line.

- The index answers no query; it hands out a sample of its pending blocks
  (`Pending`). Sample them **before** reading the store and narrow the sample
  by the checkpoint the store's answer came with (`Checkpointed`), so a block
  committed during the read is counted once.
- A store refuses to list outputs it cannot prove unspent while an address is
  still catching up (`Inspection::spendable`).

## Notes

- `BlockWithOutputs` is `base::Block<Transaction>` plus output changes and
  credits. An interpreter returns it; `BlockWithOutputs::validate` checks it
  (each `Transaction::validate`, unique identifiers and native orders, valid
  credits, every transaction naming the inspected block) before it is held as
  pending or committed. `Transaction` carries identity, movements, fee, native
  order and `Status` (included or failed, with its block).
- A reorg below the confirmed line stops that network with
  `IndexErrorKind::ConfirmedReorg` and is never repaired automatically, because
  stored blocks are treated as final. Recover with `IndexStore::purge`
  while nothing else is running, or online with that network quiesced:
  block its admissions, then `Control::rescan(indexer, store)` pauses,
  purges, resets the indexer, discards reported progress and resumes; it keeps the watches
  and other networks. Inside the window the network has no checkpoint and the
  store refuses spendable outputs.
- Hand the same repository to every chain. Each operation names its network, so
  one store isolates them; a store per network or per asset is not supported.
- A `Listener` is called after the commit, at most once, and cannot refuse it. A
  crash between the commit and the call loses the notification, so a reaction
  that must not be missed belongs inside your own `IndexStore::commit`.
- A watch with a birthday below the checkpoint reads its old blocks first. Until
  it reaches the checkpoint it is not inspected for new blocks, so a payment
  arriving now appears only after the catch-up finishes.
- `Birthday::Fresh` states that the address was never used. Blocks before the
  watch are not inspected, so earlier payments to it are never indexed.
- A custom `commit` must write the blocks, the watch progress and the checkpoint
  in one transaction and accept the same blocks twice. A cancelled or failed
  call does not prove the write was rolled back.
- A repository that also answers queries reports incomplete inspection itself,
  from the snapshot it answers from. While any watch of an address is behind the
  checkpoint, it claims no whole history for that address and refuses to list
  its spendable outputs. `Inspection` is the rule both shipped repositories ask.
  The index never sees a query's answer, so it cannot correct one.
- Shipped repositories keep exact signed running totals on ledger rows of watched
  holdings, written atomically with them; the balance is the latest row's total.
  History is forward-only per holding; replay adds nothing, and unresolved
  deficits remain `Overdrawn`.
- The SQLite repository needs a Tokio runtime with its timer. Each pooled
  connection does its SQLite work on a worker thread of its own, never on the
  async executor.

## Injected events

`Driver::with_logger` records state transitions. Transactional application
reactions belong in a custom repository.
`Control::pause` waits for the active pass; `resume` restarts a paused network.
`Control::rescan` fails as `RescanError::NotStarted` (nothing purged, network
resumed) or `RescanError::Interrupted` (purged, network left paused; rescan again).
