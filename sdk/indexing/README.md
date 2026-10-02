# indexing

An embedded indexer: it follows the addresses you watch and writes their
confirmed transactions, balance movements and spendable outputs to a repository
you supply. It is a library you call, not a service you deploy, and it survives
restarts and reorgs.

## Usage

```rust
use std::sync::Arc;

use indexing::{
    Address, AssetId, Birthday, BlockPosition, Index, IndexError, Indexing, Repository, Subject,
    Watching,
};

/// `chain` is a chain crate's constructor, such as `chain_bitcoin::indexer`.
async fn run<R, E>(
    repository: R,
    chain: impl FnOnce(R) -> Result<Arc<dyn Indexing>, E>,
    deposit: Address,
) -> Result<(), IndexError>
where
    R: Repository + Clone,
    E: std::fmt::Display,
{
    // One repository serves every network. Add one `.chain(..)` per chain.
    let index: Index = Index::over(repository).chain(chain)?.build()?;

    // A watch is an id, an address, an asset and the first block to inspect.
    let asset = AssetId::native(deposit.network.chain.clone());
    let subject = Subject { address: deposit, asset };
    index.watch("deposits", &subject, Birthday::At(BlockPosition(0))).await?;

    // One bounded pass per network. Call it in your own loop.
    for status in index.sync().await? {
        println!("{:?}: {:?} at {:?}", status.network, status.phase, status.checkpoint);
    }
    Ok(())
}
```

A repository implements two ports, and nothing else is required:

| Port | Methods | Holds |
| --- | --- | --- |
| `WatchStore` | `add`, `all`, `forget`, `cancel_backfill` | the watches and how far each was inspected |
| `IndexStore` | `checkpoint`, `commit` | the confirmed line and the one atomic write a pass makes |

Two repositories ship with the crate: `sqlite::Repository` (feature `sqlite`, on
by default) and `memory::Repository` (feature `memory`). The `supervisor`
feature, on by default, adds `Supervisor`, which runs `sync` for you with retry
and per-network quarantine. Balance, history and output queries belong to the
repository, not to the index.

## Reads

`History` and `Balances` are the pending-inclusive reads. `PendingJoin` answers
both, and the spendable outputs, for one network: the repository's own answer
joined to the blocks that network's index still holds above its confirmed line.
`Router` holds one read per network and routes each holding by its address.

```rust
use std::sync::Arc;

use indexing::{
    BalanceStore, Balances, ConfirmedHistory, History, Holder, Page, Pending, PendingJoin,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// `index` is the index that commits into `repository`, for the same network.
async fn read<R>(repository: R, index: Arc<dyn Pending>, of: &dyn Holder) -> Result<()>
where
    R: ConfirmedHistory + BalanceStore,
{
    let reads = PendingJoin::new(repository, index);

    let balance = reads.balance(of).await?;
    let page = reads.history(of, Page::first(25)).await?;

    println!("{} settled, {} arriving", balance.amount, balance.pending.incoming);
    println!("{} confirmed, {} pending", page.transactions.len(), page.pending.len());
    for observed in &page.transactions {
        println!("{}: {:?}", observed.transaction.transaction_id.id, observed.status);
    }
    Ok(())
}
```

- The index answers no query; it hands out a sample of its pending blocks
  (`Pending`). The join samples them **before** it reads the repository and
  narrows the sample by the checkpoint the repository's answer came with, so a
  block committed during the read is counted once.
- The settled fold nets a self-transfer to nothing; the pending fold reports
  it as both arriving and leaving. The questions differ: what the total is,
  and what is moving. Do not unify them.
- How much of an address's history is indexed is the repository's claim
  (`Completeness`). The join passes it on unchanged, and a repository's refusal
  to list outputs it cannot prove unspent reaches the caller as made.
- `ObservedTransaction` embeds the indexed `Transaction` with its read-time
  status, keeping the query's movement projection.

## Notes

- `InterpretedBlock` and `CanonicalBlock` directly contain `base::Block<T>`
  plus output changes and credits. Canonical construction validates and seals
  those fields. `ObservationDraft` and `CanonicalTransaction` directly contain
  `Transaction` (identity, movements and fee); each stage keeps its own status
  and native transaction order. `ObservedTransaction` embeds that same value
  with its read-time status and the queried asset's movements.
- A reorg below the confirmed line stops that network with
  `IndexErrorKind::ConfirmedReorg` and is never repaired automatically, because
  stored blocks are treated as final. Recover with `Purging::purge_network`
  while nothing else is running, or online with that network quiesced (see
  `SyncControl`: pause, purge, `Indexing::reset`, resume); it keeps the watches
  and other networks. Inside the window the store reports `Incomplete` and
  refuses spendable outputs rather than answering an empty history.
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
- Shipped repositories keep exact signed totals for watched holdings, updated
  atomically from newly inserted facts. Registration seeds retained history;
  replay adds nothing, and unresolved deficits remain `Overdrawn`. Unwatched
  holdings are computed from retained facts. SQLite migrates existing totals
  transactionally before opening. Concrete repositories also offer `activity`:
  exact credit/debit pages including self-transfer sides, charged fees and
  transactionless withdrawals, with native ordering and snapshot coverage.
  Concrete repositories offer `verify_balance`
  for an explicit independent comparison; verification never repairs rows.
- To retain an external balance attestation, capture one `BalanceEvidence` from
  the query and its settled answer, verify that same capture, then call the
  concrete repository's `record_evidence` only on agreement. It compares the
  checkpoint, signed total and caught-up watch progress atomically; no money is
  changed. `Balance.opening` reports the saved anchor while contiguous inspection
  still supports it, independently of complete-history coverage. Historical
  additions, re-anchoring, unwatch, rescan and a negative block-end total invalidate it.
- The SQLite repository needs a Tokio runtime, because each operation runs on
  `spawn_blocking`.

## Injected events

`Supervisor::with_logger` records state transitions; SQLite and memory repositories'
`with_logger` records newly committed positive net effects on watched assets. Replays,
rollback, self-transfers and Bitcoin change do not produce new receipt notifications.
Historical receipts retain their original block time. Delivery is best effort after
commit; transactional application reactions still belong in a custom repository.
`SyncControl::pause` waits for the active pass; `resume` restarts a paused network.
