# 085: Wallet and transaction indexers

Status: built, iterations 1–3 (see As built). Reshapes the base indexer
(083) so a sent transaction is indexed the same way a wallet is; replaces
084's iteration 5 (the status read from the sender's wallet index).

084 needs to know, for a sent payment, whether it is mined and how far we
have looked for it. Guessing that from the sender's wallet index (is it
caught up?) is indirect. A transaction can be indexed like a wallet: an
index that starts where it was broadcast, moves forward with every block
read, notes the block it is found in, and has its gaps closed by the
chain when it falls behind. Then the broadcast job only reads its
transaction's index.

## Roles

```
Indexer (one per network): reads and keeps blocks, the line, reorgs
 ├─ WalletIndexer      (WalletIndexStore, wallet_index)           a gap: Balances, or a jump (Bitcoin)
 └─ TransactionIndexer (TransactionIndexStore, transaction_index) a gap: Confirmer, or give up (Bitcoin)
```

- **`Indexer`** processes blocks: the tips, the line, the check of the last
  kept block, the run, keeping it. It does not know what is indexed.
- **`WalletIndexer`** and **`TransactionIndexer`** each own a store and
  close their own gaps.
- **No `watch()`.** A target (`WalletIndexTarget`,
  `TransactionIndexTarget`) becomes an index with the chain's help
  (`IntoIndex`), and its store's `insert` keeps it. Each store is wrapped
  by the notifying wrapper, so an insert wakes the indexer.
  `Indexer::watch` and `Container::watch` go.

## `Indexer`

```rust
/// One kind of index over a network's blocks.
pub trait Indexes: Send + Sync {
    /// The lowest next block of its indexing items, at or above `from` and
    /// at or below `through`: where the next run may start.
    fn next<'a>(&'a self, from: u64, through: Option<u64>) -> Future<'a, Result<Option<u64>>>;
    /// Closes the gaps of its items whose next block is below `line`.
    fn close<'a>(&'a self, line: u64, tips: Tips) -> Future<'a, Result<()>>;
    /// Moves items stuck behind a block the node no longer serves: an
    /// unstarted one starts at `start` (`move_start`), a started one is `no_data`.
    fn skip<'a>(&'a self, below: u64, start: u64) -> Future<'a, Result<Skipped>>;
}

/// Where reading starts and gaps close: the chain's own rule, from its
/// config, read through `Chained::line`.
pub enum Line {
    /// Chains that settle (Ethereum, Solana): the block after the final one.
    Final,
    /// Bitcoin: the tip less `max_depth`.
    Depth(u64),
}

pub trait Chained {
    // tips, block, blocks: as now
    fn line(&self) -> Line;
}

pub struct Indexer {
    chain: Arc<dyn Chained>,
    /// Keeping blocks advances every index of both kinds, in the same write
    /// as the blocks and legs; deleting them rewinds both kinds.
    blocks: Arc<dyn BlockStore>,
    parts: Vec<Arc<dyn Indexes>>, // [WalletIndexer, TransactionIndexer]
    // batch, interval, stats: as now
}
```

One step:

1. Read the tips; the line from `chain.line()`.
2. The last kept block below the line: still on the chain? Gone: reorg (as
   now, 083).
3. `part.close(line, tips)` for each part.
4. The run starts at the lowest `part.next(line, last)`.
5. Read the run and keep it: `BlockStore::insert` advances both kinds in
   one write.
6. A block the node no longer serves: `part.skip(…)` for each part.

## `WalletIndexer`

Today's wallet logic, moved out of `Indexer`:

```rust
pub struct WalletIndexer {
    store: Arc<dyn WalletIndexStore>,
    chain: Arc<dyn Chained>,
    balances: Option<Arc<dyn Balances>>,
}
```

`close`: for each index below the line (`find_behind`, a few per step):

- with `Balances` (Ethereum, Solana): the balance at the final block,
  `store.settle(index, holding, header)`: one adjustment, then on from the
  block after (083);
- without (Bitcoin): `store.restart(index, line)`: its coins archived and
  written off (083).

A settle no longer forgets the wallet's pending sends: each one's
transaction index settles itself. A Bitcoin restart still does, as nothing
can tell whether they mined in the gap.

## `TransactionIndexer`

```rust
pub struct TransactionIndexer {
    store: Arc<dyn TransactionIndexStore>,
    chain: Arc<dyn Chained>,
    confirmer: Option<Arc<dyn Confirmer>>,
}

/// Whether transaction `id` was mined, judged at a final block at or
/// after `at`.
pub trait Confirmer: Networked + Send + Sync {
    fn confirm<'a>(&'a self, id: &'a Id, at: u64) -> Future<'a, Result<Option<Confirmed>>>;
}

pub struct Confirmed {
    /// The final block it was judged at.
    pub position: u64,
    /// The block it is mined in, at or below `position`; `None`: not mined
    /// through it.
    pub found: Option<u64>,
    /// Whether it reverted in `found` (only its fee paid).
    pub reverted: bool,
}
```

| Chain | `Confirmer` |
| --- | --- |
| Ethereum | `eth_getTransactionReceipt` against the `finalized` block; reverted: `status` 0 |
| Solana | `getSignatureStatuses` (finalized), at the finalized slot; reverted: `err` set |
| Bitcoin | none |

`close`: for each index below the line:

- with `Confirmer`: `store.settle(index, found, reverted, header)`: `found`
  set when mined (it is final then), `reverted` kept on the index (the
  found block is kept without transactions), else on from the block after
  the final one;
- without (Bitcoin): `store.give_up(index)`: `no_data`, and the payment is
  forgotten (`jump`), as a wallet jump forgets it.

## Items and stores

```rust
/// How far an index has read, and in what state.
pub struct Progress {
    pub indexed_at: Option<BlockHeader>,
    pub state: IndexState, // indexing, paused, no_data
    pub reason: Option<String>,
}

pub struct WalletIndex {
    pub address: Address,
    pub asset: Asset,
    pub requested_from: u64,
    /// The first block it reads.
    pub birthday: u64,
    pub progress: Progress,
}

pub struct TransactionIndex {
    pub transaction: Id,
    /// The tip when it was broadcast: the first block it reads.
    pub broadcasted_at: u64,
    /// Solana's last valid block height; `None`: it never expires.
    pub expires_height: Option<u64>,
    /// The block it is mined in.
    pub found: Option<BlockHeader>,
    pub progress: Progress,
}

/// What the indexer needs of either kind.
pub trait Indexed {
    /// The first block it reads: a wallet's `birthday`, a transaction's
    /// `broadcasted_at`.
    fn start(&self) -> u64;
    fn progress(&self) -> &Progress;
    /// The next block to read: after `indexed_at`, or `start` while it has
    /// not started. Written once.
    fn next(&self) -> Option<u64> { … }
}
```

`Indexed` is implemented by both items; `next` (and `caught_up`), the
store defaults and `skip` are written once over it.

### Targets

A target is what a caller wants indexed; the chain fills in the rest. A
finished index is its own target, so a store takes either.

```rust
/// Something that becomes an index of kind `I`.
pub trait IntoIndex<I>: Send + Sync {
    fn index(&self) -> Future<'_, Result<I>>;
}

impl IntoIndex<WalletIndex> for WalletIndex { /* itself */ }
impl IntoIndex<TransactionIndex> for TransactionIndex { /* itself */ }

pub struct WalletIndexTarget {
    chain: Arc<dyn Chained>,
    address: Address,
    asset: Asset,
    start: Birthday, // `Fresh` unless set
}

impl WalletIndexTarget {
    pub fn new(chain: Arc<dyn Chained>, address: Address, asset: Asset) -> Self;
    pub fn with_birthday(self, start: Birthday) -> Self;
}

/// The tips; the birthday by `chain.line()`: `Final`, the final block (it
/// settles there next step); `Depth(max_depth)`, the start, refused when
/// older than the tip less `max_depth`. `requested_from` is the start.
impl IntoIndex<WalletIndex> for WalletIndexTarget { … }

pub struct TransactionIndexTarget {
    chain: Arc<dyn Chained>,
    transaction: Id,
    expires_height: Option<u64>, // `None` unless set
}

impl TransactionIndexTarget {
    pub fn new(chain: Arc<dyn Chained>, transaction: Id) -> Self;
    /// Solana: its blockhash's last valid block height.
    pub fn with_expires_height(self, height: u64) -> Self;
}

/// `broadcasted_at` is the tip now: it is made just before the broadcast.
impl IntoIndex<TransactionIndex> for TransactionIndexTarget { … }
```

Usage:

```rust
// watch a wallet (API)
let target = WalletIndexTarget::new(chain, address, asset).with_birthday(Birthday::At(900));
wallet_indexes.insert(&target).await?;

// watch a payment (broadcast job): the model before the record write, its
// row in that same write
let index = TransactionIndexTarget::new(chain, signed.id.clone()).index().await?;
```

```rust
pub trait WalletIndexStore: Send + Sync {
    fn insert(index: &dyn IntoIndex<WalletIndex>), find_all, find_next /* default */, find_behind /* default */,
       move_start, set_state,
       settle(index, holding, header), restart(index, line);
}

pub trait TransactionIndexStore: Send + Sync {
    fn insert(index: &dyn IntoIndex<TransactionIndex>), find_all, find_next /* default */, find_behind /* default */,
       move_start, set_state,
       settle(index, found, reverted, header), give_up(index),
       delete(index); // the broadcast job, once its payment is final or dead
}
```

## Server

- **`transaction_index`:** `transaction`, `broadcasted_at`, `indexed_at`,
  `found`, `expires_height`, `state`, `reason`.
- **Keeping a block** advances both tables by the chain's links (as
  `wallet_index` today) and sets `found` when the block holds a watched
  txid; the block reader keeps a watched transaction even without legs of
  ours, and its legs of a wallet index behind the block (paused, say: a
  pause stops tracking, not sending), numbered as any leg; reading the
  block again writes them again, a settle at or above it counts them. A
  found transaction index stops there: a transaction is mined
  once, so no later block is read for it (`next()` is `None`) and its row
  is not written again until its job deletes it, or a reorg takes its
  block back and it reads on.
- **A reorg** rewinds both tables; `found` on a block taken back is
  cleared.
- **Prune** keeps every block either table names.
- **The broadcast job** inserts its `transaction_index` row in the same
  write as the payment. Each check reads only its rows (the current
  version's and the ones it replaced):
  - `found` at or below the final block: done; above it: wait;
  - not found, indexed `bump_after` past its `broadcasted_at`: bump (084), the new
    version with its own row;
  - Solana, indexed past `expires_height`, not found: dead;
  - `no_data`: forgotten;
  - not indexed far enough: wait.

  When the job ends, its rows are deleted.

## Removed

- `Indexer::watch`, `Container::watch`, and `Indexer`'s own settle and
  jump code (now `WalletIndexer`).
- From 084: `Broadcaster::alive`, `is_displaced`, the `Indexer::status`
  sketch, `WalletIndexStore::find`, `TransactionStore::find_depth`, and the
  check that the sender's wallet index is caught up.

## Iterations

| # | What | Where |
| --- | --- | --- |
| 1 | Split `Indexer`: `Indexes`, `Line`, `WalletIndexer` (today's settle and jump), `Progress` and `Indexed`; `Chained::line`, `IntoIndex` and `WalletIndexTarget`; store `insert` replaces `watch` | `sdk/base`, server, tests |
| 2 | `TransactionIndex`, `TransactionIndexTarget`, `TransactionIndexStore`, `TransactionIndexer`, `Confirmer` (Ethereum, Solana); `transaction_index` table; blocks advance it | `sdk/base`, chains, server |
| 3 | The broadcast job reads its transaction index; `alive` and `is_displaced` removed; a wallet settle stops forgetting pending sends | server, chains |

All three are built.

## As built

Decisions beyond the design:

- **No line:** `Line::Depth(u64::MAX)`, the tip less everything, is 0.
  It is `Chained::line`'s default (test chains) and Bitcoin's without
  `max_depth` (`Chain::with_max_depth`). `Line::at(tips)` is the position.
- **No wake, no `watched`:** the index job steps its network every
  interval and `find_next` sees a new index at the next step; nothing needs
  waking, so there is no notifying wrapper. `Stats::watched`,
  `network_index.watched` and its API field are dropped.
  `Service::max_depth` goes too: the line is the chain's.
- **`Indexer::chain()`** hands out the chain a target is made with (the
  wallet index API, the broadcast job). A start older than the depth line
  fails as `WalletErrorKind::InvalidStart` (400, the message as before).
- **A close never fails the step:** a part tries every item and answers
  its first failure, which the indexer counts in `Stats::failures` and
  goes on. Before, a failed jump failed the step.
- **`skip`:** a part needs the gap when `next(0, Some(gap + 1))` answers;
  each part skips with `skip(start, start)`. The skip loop is written in
  each indexer, as the two stores are separate traits (`Stores` implements
  both, so one generic store trait would make every call ambiguous).
- **`transaction_index`:** `id` is the transaction's, with `chain`,
  `network` and `txid` (as `transaction_forgotten`), and no foreign key to
  `"transaction"`: an index outlives a forgotten transaction as `no_data`.
  `indexed_at` and `found` name blocks. Its store leaves `find_next` and
  `find_behind` to the trait's defaults (few rows).
- **Keeping a block** moves the transaction indexes it applies to (the
  wallet rule: indexed through its parent, or unstarted with its parent
  below `broadcasted_at`) and sets `found` on those whose txid it holds.
  Such a transaction is kept without a leg of ours; mining it frees its
  pending legs.
- **`TransactionIndexStore::settle(index, found, reverted, header)`** takes
  the found block's header: the `TransactionIndexer` reads both blocks from
  the chain, and the store keeps both bare. `reverted` (the `Confirmer`'s
  outcome) goes on `transaction_index.found_reverted`, which the view's
  `reverted` reads (`docs/design/085-migrate-5.sql`).
- **Forgetting gives up:** `pending::forget` sets the transaction indexes
  of what it forgets to `no_data` (reason the why), so a wallet jump that
  forgets a send reaches its job. `give_up` is that, for its own
  transaction, with `jump`.
- **The broadcast job:** "final" is the network's confirmed position
  (`network_index.confirmed`), as before; "indexed far enough" is
  `indexed_at` at or above it. A check reads, in order: index gone →
  `forgotten`; found at or below final → done (reverted there: failed
  `reverted`), above → again; `no_data` →
  `forgotten`; read past `expires_height` (by block height) and not found
  → `dead`; not far enough (or paused) → again; else the same bytes again.
- **The job's end**, one write: its index deleted, and its transaction
  forgotten while still pending: refused, dead, given up or gone, and done
  with a transaction found inside a gap the indexer never read (its legs
  pending, the settle's adjustment already counting it), as `jump`.
- **Not built:** the bump rows of the Server list (`bump_after`, the
  replaced versions' rows): 084 iterations 6–7.
- **`Transaction::expires_height`** lives on the signed transaction and on
  the index; the `"transaction"` row does not keep it.
- **`docs/design/085-migrate-1.sql`** adds the table, one index per
  pending transaction (from its network's last kept block; a Solana one
  without `expires_height`, so let those jobs finish first), and drops
  `network_index.watched`. Tried on a scratch file made by the previous
  `init.sql`: its schema then matches the new one.
- **Cascades do the deleting:** `"transaction".block`, `leg.block` and
  `leg."transaction"` are `ON DELETE CASCADE`, `block.parent` `ON DELETE
  SET NULL`, so `block::db::prune` is one `DELETE FROM block` and
  `transaction::db::delete_pending` deletes only the transaction rows.
  Every other foreign key stays plain: deleting a block an index names
  still fails. `docs/design/085-migrate-2.sql` rebuilds the three tables;
  tried as `085-migrate-1.sql` was (rows and `foreign_key_check` kept).

Tests: `sdk/base` (the target's start rules), the chains' `Confirmer`
over scripted nodes and Solana's `expires_height`, the server's stores
(an index reads on, finds, is cleared by a reorg, keeps its blocks from
prune, settles through a scripted `Confirmer`, is given up on Bitcoin; an
alt wallet settle leaves its pending send) and the broadcast job's checks
(final `found`, `dead` past `expires_height`, `forgotten` when given up or
forgotten by a jump, waiting while not read far enough).
