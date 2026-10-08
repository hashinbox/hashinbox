# 051 — Explicit inputs, cut of the query traits

Status: proposal. Scope: `sdk/indexing` query side, `sdk/chains/bitcoin` send
path, `sdk/config::Composition`, `apps/api` read endpoints. Supersedes the
read-side parts of 040 and 048 where they touch query handles.

## Problem

Two facts, verified in source:

1. The query traits serve only SDK-internal machinery, never the engine.
   `BalanceStore` is consumed only by `PendingJoin` and the api balance route.
   `ConfirmedHistory` only by `PendingJoin` history and the api history route.
   The engine knows `WatchStore + IndexStore` and nothing else.
2. Sending BTC requires the indexer alive today. The chain composer builds
   `PendingJoin::new(repository, indexer.clone())`
   (`sdk/chains/bitcoin/src/config.rs:324`) and the wallet factory takes it as
   `spendable: Arc<dyn OutputStore>` (`wallet.rs:170`). Coin selection reads
   through it at send time (`sender/outbox.rs:233`). No indexer, no sends.

Meanwhile `Builder` is already pure: `BuildContext { network, available:
Vec<Utxo>, change, fee_rate }` (`native/transaction/builder.rs:18-27`). The
wallet never needed a store. It needed `Vec<Utxo>`.

## Decision

- The wallet's input seam is **caller-selected UTXOs by default**. A small
  store-backed wrapper exists for those who want automatic selection.
  Sending no longer requires the indexer, the repository, or `indexing` at all
  on the input path.
- The query traits (`BalanceStore`, `ConfirmedHistory`, `History`,
  `Balances`, `PendingJoin`) are deleted. Reading is the application's
  business. `apps/server` owns SQL against the file it already owns.
- `OutputStore` stays. It is the storage port for live outputs, consumed only
  by the store-backed input wrapper — never by the wallet directly.
- Engine, `WatchStore`, `IndexStore`, `Pending`, `Sample`, watches vocabulary:
  untouched.

## The input seam

`Utxo` is already public and chain-native:
`sdk/chains/bitcoin/src/native/block/output.rs:39`, re-exported as
`chain_bitcoin::native::block::Utxo`. It crosses the seam.

New file `sdk/chains/bitcoin/src/sender/utxo_source.rs`:

```rust
/// Where a send's candidate inputs come from. Selection itself is pure
/// (`Builder`); this only supplies the UTXOs, read once per send at
/// reservation time. Named like `indexing::BlockSource`: one side supplies
/// blocks to the index, the other supplies inputs to the wallet.
pub trait UtxoSource: Send + Sync {
    fn utxos(&self, addresses: &[Address]) -> FutureResult<'_, Vec<Utxo>>;
}

/// Default: the caller selected the outputs. No store, no indexer.
impl UtxoSource for Vec<Utxo> { /* return the list as-is */ }
impl UtxoSource for Arc<Vec<Utxo>> { /* same, shareable */ }
```

New file `sdk/chains/bitcoin/src/sender/spendable.rs` — the wrapper, ported
from today's `UtxoReader` (`sender/utxos.rs`) plus the pending fold currently
inside `PendingJoin::list` (`indexing/src/storage/outputs.rs:196`):

```rust
/// Store-backed input source: lists live outputs, folds the pending blocks
/// above the confirmed line (change spendable at depth one), gates on
/// catch-up.
pub struct Spendable {
    store: Arc<dyn indexing::OutputStore>,
    pending: Arc<dyn indexing::Pending>,
    network: Network,
}

impl UtxoSource for Spendable {
    fn utxos(&self, addresses: &[Address]) -> ... {
        // 1. page OutputStore::list per address, one checkpoint across pages
        //    (today's UtxoReader read, incl. evidence/maturity validation)
        // 2. sample = pending.sampled(); fold sample.above(&page) created/spent
        //    (today's PendingJoin fold)
        // 3. Inspection::spendable() gate while an address is catching up
        //    (today's reads.rs:126 policy)
    }
}
```

Constructor changes, all in `sdk/chains/bitcoin`:

- `wallet.rs:167` — `WalletFactory::new(config, rpc, spendable: Arc<dyn OutputStore>)`
  becomes `WalletFactory::new(config, rpc, source: Arc<dyn UtxoSource>)`.
- `wallet.rs:192` — `UtxoReader::new(config.native, spendable)` becomes
  storage of `Arc<dyn UtxoSource>`. `UtxoReader` (crate-private) is deleted;
  its validation logic moves into `Spendable`.
- `wallet.rs:221-241` — factory fingerprint keeps a `Dependency` on the
  source handle instead of `utxos`.
- `sender/outbox.rs:233` — `self.utxos.read(&addresses)` becomes
  `self.source.utxos(&addresses)`. Reservation filtering of already-reserved
  outpoints is unchanged: explicit inputs pass through the same
  `Reservations::admit` filter (`reservation.rs:98-127`). Recovery replays
  original inputs unchanged.

Both variants, side by side:

```rust
// Manual: caller selected (sweep, consolidation, offline selection).
let source: Arc<dyn UtxoSource> = Arc::new(vec![utxo_a, utxo_b]);
let factory = chain_bitcoin::WalletFactory::new(config, &rpc, source)?;

// Automatic: store-backed wrapper (built-in server, examples).
let source: Arc<dyn UtxoSource> =
    Arc::new(chain_bitcoin::sender::Spendable::new(store, indexer.clone(), network));
```

`chain_bitcoin::Config::compose` (`config.rs:302-337`) switches to
`Spendable` — the built-in product keeps today's behaviour, including
depth-one change.

## Where the read types move

Deleted from `sdk/indexing`:

- `storage/reads.rs` — `BalanceStore`, `BalanceQuery`, `Inspection` (moved,
  see below), `OutputStore` (moved to `storage/watch.rs` sibling
  `storage/outputs.rs` port file or stays as its own `storage/outputs_port.rs`;
  it survives), `Purging` (moved beside it; behaviour unchanged).
- `storage/balance.rs` — `Balances` handle, `Balance`, folding (`Fold`).
- `storage/history.rs` — `ConfirmedHistory`, `HistoryQuery`, `HistoryPosition`,
  `Page`, `ConfirmedHistoryPage`, `Completeness`.
- `storage/observed.rs` — `History` handle, `ObservedHistory`,
  `TransactionPage`, `ObservedTransaction`.
- `storage/join.rs` — `PendingJoin`, `Router`.
- `storage/outputs.rs` — the `PendingJoin` OutputStore impl and its fold.
  (`OutputPage`, `OutputRequest`, `OutputCursor` survive next to the
  `OutputStore` port.)
- sqlite: the `BalanceStore` / `ConfirmedHistory` / query impls; their SQL
  moves to `apps/server`. The tables stay — `commit` still writes them.
- memory: the same three query impls.
- Contract suites: balance, history, observed-read suites under
  `sdk/indexing/tests/contracts/`; the matching scenarios in
  `repository_contract.rs`, `service_contract.rs`, `pending_reads.rs`.

Kept, moved:

- `Inspection` (the `caught_up`-based read gate) moves beside `WatchProgress`
  in `storage/watch.rs`. It is a pure predicate over progress + checkpoint;
  the `Spendable` wrapper and server SQL both call it. Re-exported.
- `OutputStore` stays a store port: one method `list(OutputRequest) ->
  OutputPage`, `Arc` forwarding impl. Sole SDK consumer is
  `chain_bitcoin::sender::Spendable`.

Born in `apps`:

- `apps/api` defines its own read handle traits (today it imports
  `indexing::{History, Balances}`). Same shapes, app-owned names, implemented
  by `apps/server` over its sqlite file with its own SQL:
  confirmed rows + `Pending::sampled()` / `Sample::above` for the pending
  first page — the same two sources `PendingJoin` merged, now app code.
  `Inspection` gates both.

## Composition changes

- `sdk/config::Composition` / `ComposedChain`: `history` and `balance` fields
  are deleted. What remains: `index`, `wallets`, `schedules` (+ chain-specific
  parts).
- Chain `config.rs` composers stop building `PendingJoin`; they build
  `Spendable` for the factory and expose no read handles.
- `examples/indexing`, `examples/transfer`: drop the `history`/`balance`
  bindings; transfer example is otherwise unchanged (`wallets.author(...).send(...)`
  works on both source variants).

## Test plan

- Rework: `sender/utxos.rs` tests become `Spendable` tests (same scripted
  pages; add the pending-fold cases from `indexing/tests/pending_reads.rs`).
  `sender/stub.rs` — the test `Node` implements `UtxoSource` instead of
  `OutputStore`. `wallet.rs` fingerprint tests. Outbox/reservation/recovery
  suites: same scenarios, source injected.
- Keep: `native/transaction/builder.rs` pure tests untouched; server HTTP
  tests (`wallet_api.rs`, `acceptance.rs` sweep) must pass unchanged —
  they prove the store-backed variant preserves behaviour.
- Delete: indexing query-side suites listed above.

## Non-goals

- `Purging` and `--rescan` stay as today (049 decides their fate separately).
- Engine invariants untouched: confirmed line, pending in memory only,
  one atomic commit, `caught_up` as the single catch-up predicate.
- Ethereum/Solana send paths are account-based and store-free already;
  no change.

## Rollout

1. `UtxoSource` + `Spendable` in `chain_bitcoin`; factory/outbox switch;
   `UtxoReader` deleted. Tests reworked. Gates.
2. Query-trait deletion in `sdk/indexing` (types, impls, suites).
   Gates.
3. `apps/api` local read traits + `apps/server` SQL reads.
   Gates.
4. `Composition` field deletion + example updates. Gates.
