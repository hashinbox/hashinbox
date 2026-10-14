# indexing

An embedded indexer: it follows the addresses you watch and writes their
confirmed transactions, balance movements and spendable outputs to a repository
you supply. It is a library you call, not a service you deploy, and it survives
restarts and reorgs.

## Usage

```rust
use std::sync::Arc;

use indexing::{
    Address, Asset, Birthday, Indexers, IndexError, Indexing, Watching,
};

/// `chain` is a chain crate's indexer, such as `chain_bitcoin::indexer`'s,
/// built over the one repository every network shares.
async fn run(chain: Arc<dyn Indexing>, deposit: Address) -> Result<(), IndexError> {
    // One indexer per network, all over one repository.
    let index: Indexers = Indexers::of(vec![chain])?;

    // A watch is an address, an asset and the first block to inspect.
    let asset = Asset::native(deposit.network.clone());
    index.watch(&deposit, &asset, Birthday::At(0)).await?;

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
| `WatchStore` | `insert`, `find_all`, `delete` | the watches and how far each was inspected |
| `IndexStore` | `checkpoint`, `commit`, `purge` | the confirmed line and the one atomic write: `Changes` |

A `Watch` is one wallet's address observed in one asset from a birthday — one
`wallet_index` row carrying everything indexing reads. Wallets themselves (address,
label, key) are the application's (`base::Wallet` built by `base::WalletFactories`); the shared schema's
foreign key requires a wallet row filed before its watch, and indexing code never
reads that table. A `Watch` carries no key. `delete` (the durable half of `unwatch`)
deletes the observation and, atomically, its indexed facts (ledger rows, outputs,
memberships, transactions nothing else names); facts are only written for watched
(address, asset) pairs, and watching again reads the range from the birthday then given. A purge
deletes everything about the network and keeps every watch and its birthday.

The crate ships no repository: implementations live with the application
(the server's SQLite storage, `apps/server/src/storage`). The pure helpers a
store derives its rows with ship here — `Changes` validation, `LedgerEntry`,
`Probe` — so every store computes them identically. The `driver` feature, on
by default, adds `Driver`, which runs `sync` for you with retry and
per-network quarantine. Balance, history and output queries belong to the
repository, not to the index.

## Commits

One commit is `Changes`: a validated batch of `Change`s of one network —
`Block`, `Transaction` (expected, mined or failed), `Reward` and `Output`
(an output row created, or an output spent). Blocks come first and in chain
order, each followed by its facts. A store writes it in one transaction:
blocks at or below its checkpoint are verified or inserted and the first
above must name the checkpoint as its parent (`Changes::line`); every fact is
inserted or verified (expected → mined, expected → failed and failed → mined
are the allowed transitions); ledger rows derive from the output rows
(`LedgerEntry::of_changes`); and every watch whose progress is the first
block's parent, or that has none and was born in the range, advances to the
last block (`Changes::advances`). An identical retry is a no-op.

## Reads

Balance and history are not ports: a store answers them its own way, each in
one snapshot that says how far behind its line the address's inspection is.
The one narrow port is for the SDK's own consumer — `OutputStore` (Bitcoin's
coins, `chain_bitcoin::coins::Spendable`) — and an application completes its
own stored reads with the blocks an index holds above its confirmed line.

- The index answers no query; it hands out a sample of its pending blocks
  (`Pending`). Sample them **before** reading the store and narrow the sample
  by the checkpoint the store's answer came with (`Checkpointed`), so a block
  committed during the read is counted once.

## Notes

- An interpreter returns one block's `Changes`, the block first; the index
  checks it names the inspected block before holding it as pending or
  committing it.
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
- A custom `commit` must write the blocks, the facts, the watch advance and the
  checkpoint in one transaction and accept the same batch twice. A cancelled or failed
  call does not prove the write was rolled back.
- A repository that also answers queries reports incomplete inspection itself,
  from the snapshot it answers from. While any watch of an address is behind the
  checkpoint, it claims no whole history for that address.
  The index never sees a query's answer, so it cannot correct one.
- A store keeps exact signed running totals on ledger rows of watched
  holdings, written atomically with them; the balance is the latest row's total.
  History is forward-only per holding; replay adds nothing.

## Injected events

`Driver::with_logger` records state transitions. Transactional application
reactions belong in a custom repository.
`Control::pause` waits for the active pass; `resume` restarts a paused network.
`Control::rescan` fails as `RescanError::NotStarted` (nothing purged, network
resumed) or `RescanError::Interrupted` (purged, network left paused; rescan again).
