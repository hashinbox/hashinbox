# 053 — The `payments` container

Status: proposal. Scope: new crate `payments` (absorbs `sdk/config`), the
embedder API, one storage decorator, slot-sliced storage wiring. Builds on
042 (no runtime), 044 (RPC cluster), 049 (cuts), 051 (reads are store-owned,
no query traits — amended here: purge joins `IndexStore`), 052 (three jobs),
054 (chain cadence).

## Layers

```
apps/server
payments                (container + its config module; names no chain)
wallets, indexing       (sdk/wallets, sdk/indexing)
chains/*                (impl payments::config::Chain, wallets factories)
base/
packages/
```

Rules:

- A layer imports only layers below it.
- `payments` is generic over chains: it never names `chain_*`. The app (or
  example) names chains at `.with::<chain_bitcoin::Config<_>>("bitcoin")`.
  This keeps `payments` inside `sdk/` and the linter's closed-dir rule intact.
- Crate layout: `sdk/payments/src/config/*` (today's `sdk/config`, moved),
  `src/payments.rs` (the container), `src/networks.rs` (the operational
  handle).
- `sdk/config` as a separate crate disappears; chain crates switch
  `use sdk_config::…` to `use payments::config::…`. Mechanical.

## Storage: 2 required traits, 3 optional, per-slot wiring

A custom store implements:

```rust
// Required — the engine runs on these (`Repository` is their auto-bundle):
pub trait WatchStore: Send + Sync {
    fn add(&self, watches: Vec<Watch>) -> …;
    fn all(&self, network: &Network) -> … Vec<WatchProgress>;   // one snapshot
    fn forget(&self, network: &Network, watch: &Watch) -> …;
}

pub trait IndexStore: Send + Sync {
    fn checkpoint(&self, network: &Network) -> … Option<BlockRef>;
    fn commit(&self, batch: IndexedBatch) -> … Vec<OutputKey>;   // the one write
    fn purge(&self, network: &Network) -> … PurgedRows;          // the one removal
}

// Optional — each named after the one machine that needs it:
pub trait OutputStore: Send + Sync {                             // chain_bitcoin Spendable (inputs)
    fn list(&self, request: OutputRequest) -> … OutputPage;
}
pub trait TransactionStore: Send + Sync {                        // chain_solana sender recovery
    fn locate(&self, transaction: &TransactionRef) -> … Location;
}
pub trait WalletRegistry: WatchStore {                           // app-side keys/registry; engine never calls
    fn register(&self, registration: Registration) -> … Wallet;
    fn wallet(&self, id: &str) -> … Option<Wallet>;
    fn wallets(&self) -> … Vec<Wallet>;
}
```

Purging merged into `IndexStore` (amends 051): purge is not an optional
query, it is the removal half of the write lifecycle and the only recovery
path after a `ConfirmedReorg`. One `DELETE` per table; required, so recovery
can never meet a missing capability. The `Purging` trait is deleted.

**Per-slot wiring.** The container never wraps the store. It holds one
`Arc` of the caller's instance per trait slot — same allocation, typed views
(`Arc<R>` coerces per assignment). The engine receives the two required
views; each optional capability travels down its own channel to its one
consumer; the caller's concrete instance stays outside with its full surface
(`balance()`, `history()`, anything).

The one place a wrapper appears: the container wraps the **index view only**
with `Notifying` so `payments.changes()` can exist. Nothing is hidden by it —
the engine only ever wanted `IndexStore`, and every other slot bypasses it.

## Events: `Notifying` on the index slot

```rust
// sdk/indexing/src/storage/notifying.rs
/// An IndexStore that notifies after every commit it accepts, then delegates.
/// Wraps one view, never the store.
pub struct Notifying<R> { inner: R, tx: … }

impl<R: IndexStore> IndexStore for Notifying<R> {
    fn commit(&self, batch: IndexedBatch) -> … {
        let unmatched = self.inner.commit(batch).await?;
        // after the store returned: one Change per progress claim in the batch.
        // batch.progress() names the watch ids — no address matching needed.
        self.emit(batch.network(), &batch);
        Ok(unmatched)
    }
    // checkpoint, purge delegate verbatim
}

pub struct Change { pub network: Network, pub watch: String, pub block: BlockRef }
```

- `payments.changes()` hands out the stream; the container built the wrapper
  around the index slot during `start()`. You never construct it in the
  normal path.
- `with_index_store(x)` is the advanced override: replace the container's
  wrap with your own (e.g. an emitter inside your transaction).
- Transactional reactions (an event inside the commit tx) belong inside a
  custom store's own `IndexStore::commit` — its type, its transaction. No SDK
  seam needed.
- `Listener`/`BlockCommit` stay for full-power embedders, unchanged. The
  engine gains nothing; no payload change.

## The container

```rust
let repo = Arc::new(sqlite::Repository::open(".index-data/index.db")?);

let payments = Payments::from_toml(CONFIG)?
    .with::<chain_bitcoin::Config<AssetTag>>("bitcoin")
    .with::<chain_ethereum::Config<AssetTag>>("ethereum")
    .with::<chain_solana::Config<AssetTag>>("solana")
    .with_store(repo.clone())?            // required ports: watch + index (+purge)
    .with_output_store(repo.clone())?     // optional slots — same instance is fine
    .with_transaction_store(repo.clone())?
    .with_wallet_registry(repo)?          // omit any the product does not use
    .start().await?;                      // verifies nodes, composes, drives, waits readiness

let mut changes = payments.changes();

// 1. wallet from a private key
let wallet = payments.wallets.from_pk("btc", pk)?;
payments.watch("alice", &wallet, Birthday::Fresh).await?;

// 2. send
let txid = payments.wallets.send(&wallet, to, amount).await?;

// 3. know when the index has seen alice
while let Some(change) = changes.next().await {
    if change.watch == "alice" {
        let balance = repo.balance(&alice);  // the repo is the answer, you just re-query
    }
}

// operations, absorbed from the app's network plumbing
payments.networks.list();                          // configured, served, per-node counters
payments.networks.replace(network, endpoints).await?; // verify candidates, swap inflight
payments.networks.pause(network).await?;
payments.networks.resume(network).await?;
payments.networks.rescan(network).await?;          // the online rescan
payments.networks.health();                        // SyncHealth snapshot

// shutdown
payments.shutdown().await?;
```

Builder rules:

- `with_store(repo)` bounds `WatchStore + IndexStore` — required ports only.
  No provider trait, no trait probing. Omitted required slot → `start()`
  errors.
- `with_output_store` / `with_transaction_store` / `with_wallet_registry` —
  each its own method, its own bound. Omitted → the chain or feature that
  needs it fails at `start()` with a named error ("bitcoin requires an
  OutputStore").
- `with_index_store(x)` overrides the container's default wrap for the index
  slot.
- For engine construction the container pairs the two required views in a
  small private `Split` adapter (`{watches, index}` implementing
  `Repository`) — the chain's `indexer(params)` still receives one value.

Each builder step is one decision. No sqlite assumption — sqlite is just
what the call site constructed.

The manual path (`compose()` with its closures and registrar) stays for
embedders who build chains programmatically. `Payments` is the default path,
not the only one.

## Levels of control

Four documented rungs, top to bottom. Each is a complete program. Drop down
only when the rung above cannot express what you need; nothing at a lower
rung requires going back up.

**Rung 0 — config-driven (default).** TOML + named chains. The SDK parses,
verifies nodes, composes, drives, reports readiness. The snippet in
"The container" above.

**Rung 1 — code-config.** Same builder, no TOML. Each chain's `Config` is a
plain struct (endpoints, network, wallet settings); construct it in code.
The SDK still verifies, composes, drives.

```rust
let payments = Payments::builder()
    .chain(chain_bitcoin::Config { network, rpc, wallet, index })  // constructed, not parsed
    .chain(chain_ethereum::Config { network, rpc, wallet, index })
    .with_store(repo)
    .start().await?;
```

`.with::<C>("bitcoin")` is sugar for `.chain(parse_section::<C>("bitcoin")?)`.
Both end at the same builder step.

**Rung 2 — manual composition.** No container. You build each piece, wrap
what you want notified, and keep the raw repo outside:

```rust
let transport = json_rpc::Endpoint::new(url).connect(&secrets)?;
let rpc = chain_bitcoin::RpcClient::connect(transport, &network).await?;

let notifying = Arc::new(indexing::storage::Notifying::new(repo.clone()));
let indexer = chain_bitcoin::indexer(chain_bitcoin::IndexerParams::new(rpc.clone(), notifying))?;
let source: Arc<dyn chain_bitcoin::sender::UtxoSource> =
    Arc::new(chain_bitcoin::sender::Spendable::new(repo.clone(), indexer.clone(), network));
let factory = chain_bitcoin::WalletFactory::new(wallet_cfg, &rpc, source)?;

let index = indexing::Indexers::of(vec![Arc::new(indexer)])?;
index.watch("alice", &subject, indexing::Birthday::Fresh).await?;
let mut changes = /* the notifying wrapper's stream */;

// your spawn choice: the optional Tokio driver (feature `driver`, renamed
// from `Supervisor` — it drives sync tasks; it is not an actor supervisor).
// Each unit polls on the schedule its chain declared (054); health and
// readiness are built in, `.monitor(...)` adds extra observers.
let sync = tokio::spawn(indexing::Driver::new(index.clone()).run(stop));
// …or a plain loop of `unit.sync().await` per unit.
```

This is the custom-store path (051): manual UTXOs or the `Spendable`
wrapper, your repository type, `compose()` if you want the chain's own
assembly. `examples/custom_store` is this rung.

**Rung 3 — primitives.** Storage ports and bounded sync only. You implement
`WatchStore + IndexStore` (Postgres, whatever), take the chain's
`Indexing`, drive `sync()` in your own loop. No container, no driver, no
`wallets`. The whole contract: two traits + `Listener` + `Indexing::sync`.

```rust
let store = Arc::new(MyPgStore::connect(pg).await?);
let rpc = chain_bitcoin::RpcClient::connect(transport, &network).await?;
let unit = chain_bitcoin::indexer(chain_bitcoin::IndexerParams::new(rpc, store))?;
unit.watch("alice", &subject, indexing::Birthday::Fresh).await?;
loop {
    let outcome = unit.sync().await?;
    if outcome.phase == indexing::Phase::Ready { /* idle */ }
}
```

Relationships: `from_toml` is sugar over `builder`; `builder` is sugar over
the rung-2 assembly; rung 2 is sugar over `sync()`. The docs show all four;
the embedder picks where to stop.

## The operational handle: `payments.networks`

Today this machinery lives in the app: `apps/server/src/api/networks.rs`
(688 lines) — `LiveNetwork` (swappable transport + node rows + a maintenance
lock so one replacement or rescan runs at a time), a `Verifier` closure (the
chain's own node check, type-erased so the app layer names no chain), RPC
counters that follow a replacement, plus `Control` (pause/resume/rescan) and
the health receiver riding beside it in `AppState`.

All of it is container material: it is per-network live state that exists
because the container composed the chains. It moves into
`payments/src/networks.rs` as one handle:

- What moves: `LiveNetwork`, the verifier wiring, node replacement (verify
  candidates → swap transport → counters follow), maintenance lock, pause /
  resume / rescan (the driver `Control`), health.
- What stays in the app: HTTP routes and DTOs over `payments.networks`, and
  the `rpc_endpoints` table rows the models own.
- Chain neutrality holds: the verifier stays a type-erased
  `Arc<dyn Fn(Network, RpcEndpoints) -> BoxFuture<Result<Verified, Rejected>>>`,
  captured from each chain at `.with::<C>(…)` registration. The container
  names no chain; the call site does.
- `Swap` (the transport that retargets callers) is generic RPC mechanics; it
  belongs in `packages/json-rpc`, not in the app and not in the container.

After the move, `AppState` shrinks to what routes genuinely need on every
call (wallets, signers, repo pool, index, networks handle) — or the routes
take `Payments` directly.

## What payments owns vs. delegates

Owned by the container:

- Builder, node verification order, driver spawn, readiness wait, shutdown.
- Wallet collection (`payments.wallets`), watch registration
  (`payments.watch` delegates to the index).
- Operational handle (`payments.networks`).
- The event subscription (`payments.changes()`) and its default index-slot
  wrap.

Delegated:

- Balance/history reads → the repository (inherent methods on the concrete
  store). `payments` deliberately has no `balance()`/`history()`.
- Transactional reactions → inside a custom store's own `commit`, or your own
  `with_index_store` decorator.
- `Indexing::sync()` stays the fundamental primitive; `start()` uses the
  driver internally. Bounded sync without the container is unchanged.

## Wallet verbs

- `Wallets::from_pk(asset, SecretBytes)` — wraps `WalletFactory::create`.
  `import` rejected: it says "bring external data in"; `from_pk` says what it
  is. `generate` stays for fresh keys.
- `Wallets::send(&wallet, to, amount)` — resolves asset/factory from the
  wallet; no asset tag re-stated. `Author` stays for batches/queues.

## Changes (complete list)

1. `sdk/indexing`: `IndexStore` gains `purge(network)`; `Purging` trait
   deleted (amends 051). New `storage::Notifying<R: IndexStore>` +
   `Change` + stream. `Listener`/`BlockCommit` unchanged.
2. Concrete stores: sqlite and memory move their purge impl into `IndexStore`;
   inherent `balance(…)`/`history(…)` on both. No traits.
3. `sdk/wallets`: `Wallets::from_pk`, `Wallets::send(&wallet, …)`.
   (`Author`/`Authorship` unchanged underneath.)
4. New crate `sdk/payments`: absorbs `sdk/config` (as `payments::config`),
   adds `payments.rs` (builder with per-slot `with_*_store` methods, private
   `Split` adapter, default `Notifying` wrap for `changes()`) and
   `networks.rs`. `sdk/config` crate removed; `sdk_config::` imports become
   `payments::config::`.
5. Move from `apps/server/src/api/networks.rs` into `payments/src/networks.rs`:
   `LiveNetwork`, verifier wiring, replacement, maintenance lock, pause /
   resume / rescan, health. Move `Swap` to `packages/json-rpc`.
6. `apps/server` routes become thin over `payments.networks`; its
   `api/networks.rs` keeps only route DTOs.
7. `examples/indexing` rewritten to the snippet above; it becomes the
   facade's compiling usage doc.

Superseded drafts, kept as history: (a) whole-store `Notifying` — a wrapper
hides capabilities from whoever receives it; solved by slot slicing (the
wrapper touches only the index view, and only the engine sees it). (b)
engine-emitted `BlockCommit.touched` — unnecessary once the wrapper sits on
the slot that already sees the batch; the engine stays untouched.

## Rollout

1. Indexing: purge merge + `Notifying` + store reads. Gates.
2. `sdk/wallets`: `from_pk`, `send(&wallet)`. Gates.
3. `sdk/payments` crate: config move + container + slot wiring. Examples
   rewrite. Gates.
4. Networks handle move (payments + json-rpc `Swap` + server routes thin).
   Gates.
