# 050: `sdk/indexing` — the full file and structure map

One rule: **every file is one of three kinds — an ORGAN (state and the
mutations that own it), an ACTION (a procedure the engine performs), or a
CONTRACT (the words and ports shared with a neighbor). Nothing else gets a
file.**

## Engine root

### Organs — state the engine owns
- **`indexers.rs`** — the ownership family: `Indexing` (the capability a
  chain provides), `Indexer<S, I, R>` (one network's owner: watches handle,
  pending blocks, chain adapter, store reference, gate handle;
  constructors, `network()`, the listener list, `Checkpoint`),
  `Indexers` (the router: one child per configured network), `Route`.
- **`watches.rs`** — `Watching`, `Watch`, `Birthday`, `Watches`, `Rows`,
  `Owed`, `Reconciliation`.
- **`pending.rs`** — `Pending` (the sample capability), `Sample` (one
  reading + revision), `Blocks` (the private set held above the line).

### Actions — procedures the engine performs
- **`run.rs`** — `advance` and its phases (the `catchup.rs`/`backfill.rs`
  files and names die here): `Window`, `Tally`, `ConfirmedLine`, `Guard`,
  `Backfilling`.
- **`read.rs`** — `Scan`, `Cache`, the fetch methods on `Indexer`.
- **`gate.rs`** — the commit/publication handshake: `Gate`, `Plan`,
  `CommitPermit`, `PublicationPermit`.

### Contracts — words and ports shared with a neighbor
- **`block.rs`** — the block chapter: what a block is, what it carries, and
  the contract under which one arrives. The vocabulary: `BlockWithOutputs`
  (048), `Transaction` (one struct + `Status`), `ValueMovement`,
  `NetworkFee`, `MovementId`, `OutputId`, `OutputKey`, `IndexedOutput`,
  `OutputChanges`, `BlockCredit`, `extends`. The chain contract:
  `BlockSource`, `ChainTips`, `NativeBlock`, `BlockInterpreter`,
  `FeeSettlement`, and the chain-facing validation helpers. (Today's
  `block.rs` + `observation.rs` + `output.rs` + `source.rs` +
  `network.rs` = 963 lines.)
- **`sync.rs`** — the words for runs: `Outcome`, `Phase`, `Backlog`,
  `Condition`, `Health`, `Readiness`, `Budget`, `Config`, `Schedule(s)`.
- **`error.rs`** — `IndexError`, `IndexErrorKind`, `SourceError`.
- **`supervisor.rs`** — the optional driver (feature `supervisor`), one
  file: `Supervisor`, `Worker`, `Control`, `Monitor`, `Report`.
- **`storage.rs`** — the persistence contract root: `Repository`, port
  re-exports.

Root: 24 → 13 files.

## `storage/` — the persistence contract

Rule: **a file is one port (one question a store answers) or one query
family; a backend file is one port side; a model file is one table's row
codec.** All of it is contract.

- **`watch.rs`** — `WatchStore`, `WatchProgress` (the durable list).
- **`commit.rs`** — `IndexStore`, `IndexedBatch`, `Progress` (the one
  write).
- **`reads.rs`** — the optional capabilities: `BalanceStore`,
  `OutputStore`, `Inspection`, `Purging`.
- **`wallet.rs`** — `WalletRegistry`, `Registration`, `Wallet` row.
- **`balance.rs`** — `Balance`, `PendingValue`, `Completeness`, `Fold`,
  `MovementEffect`, `FeeSettlement`.
- **`history.rs`** — `Page`, `HistoryQuery`, `HistoryPosition`,
  `ConfirmedHistoryPage`, `ConfirmedHistory`.
- **`observed.rs`** — the pending-inclusive history read: `History`,
  `TransactionPage`, `ObservedTransaction`, `TransactionStatus`.
- **`join.rs`** — `PendingJoin`, `Router`.
- **`outputs.rs`** — `OutputRequest`, `OutputCursor`, `OutputPage`,
  `Spendable`.
- **`subject.rs`** — `Holder`, `Subject` (from `holder.rs`; moves to
  `base` when the severance task lands).

storage/: 11 → 10 files (`receipts.rs` dies with 049; `activity` and
`verify_balance` are already gone).

## `storage/sqlite/` — one file per table-family

- **`mod.rs`** — pool, file open, schema identity, thin read operations
  (`balance.rs`, `outputs.rs`, `wallets.rs`, `schema.rs` fold in ≈ 680).
- **`commit.rs`** — the one write, BEGIN IMMEDIATE.
- **`watches.rs`** — the durable list operations.
- **`history.rs`** — history paging operations.
- **`models/`** — one codec per table: `block`, `checkpoint`,
  `history_entry`, `ledger`, `membership`, `output`, `transaction`,
  `wallet`, `mod`. A table is a name.

sqlite/: 12 → 8 files.

## `storage/memory/` — one file per port side

- **`mod.rs`** — the map store plus the watch side (today's `mod.rs` +
  `observations.rs` + `wallets.rs` ≈ 820).
- **`commits.rs`** — the write side + balance fold (today's `commits.rs` +
  `balances.rs` ≈ 430).

memory/: 5 → 3 files. The backend stays shipped.

## What disappears or moves

- `catchup.rs`, `backfill.rs`, `pass.rs` → `run.rs` · `scan.rs`,
  `fetch.rs` → `read.rs` · `admission.rs` →
  `gate.rs` · `sync.rs`+`health.rs`+`policy.rs`+`schedule.rs` → `sync.rs` ·
  `supervisor/` → `supervisor.rs` · `source.rs`, `network.rs` →
  `block.rs` ·
  `listeners.rs` → `indexers.rs` · `holder.rs` → `storage/subject.rs` ·
  `observation.rs`, `output.rs` → `block.rs` · `index.rs` + `indexer.rs` →
  `indexers.rs`.
- Gone with 049: `receipts.rs` (and `activity`, `verify_balance` already).
- After 048: `ObservationDraft`, `CanonicalTransaction`,
  `ObservationDraftStatus`, `CanonicalStatus` → `Transaction` + `Status`,
  `BlockWithOutputs`.

Counts: engine 24 → 14, storage 11 → 10, sqlite 12 → 8, memory 5 → 3.
Every file is an organ, an action, or a contract — and says which in its
first line.
