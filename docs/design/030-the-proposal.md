# 030: The proposal — the whole SDK, in one document

> "What is weird is that from first sight it's hard to understand how it works,
> it's plenty of 'garbage' put together."

> "Everything we discussed, consolidated into one design proposal."

This supersedes the scattered state across `docs/design/{017,019,021,024,025,026,027,028,029}`.
Those remain the raw material and the evidence; this is the single target, the
usage it produces, and the order to build it.
`029-observation-and-callbacks.md` is being written in parallel and owns
observation, the storage callback and the `Indexer`/`Composer` interface in
detail; this document states its conclusions by reference and does not respell
them.

Paths are repository-relative; line numbers are from the working tree on
`refactor/chain-unification` (986 tests, 0 linter findings, after `027` landed).

**On `engine/` paths.** Sections 0 to 3 diagnose and re-specify the tree *as it
was when this was written*, when `sdk/indexing/src` held an `engine/`
subdirectory. Step 8 flattened it: every `engine/<name>.rs` is now
`sdk/indexing/src/<name>.rs`, and `engine/supervisor/` is
`sdk/indexing/src/supervisor/`. Those sections keep their original paths,
because the layout they cite is the thing they are arguing against; sections 4
and 5, which describe the tree today, have been corrected to the current ones.

---

## 0. Why it reads as garbage — the diagnosis first

The complaint is correct, and it is not a complaint about craftsmanship. Every
piece of apparent garbage is a **reasonable answer to a question that should
not have been asked**. The pattern is always the same:

> A structural decision creates a problem → machinery is built to coordinate
> the problem → the machinery is large, subtle, and correct → a reader meets
> the machinery first and cannot see the decision behind it.

Remove the structural decision and the machinery has nothing left to do. This
is not a hope: **`025` already ran the experiment.** It stopped committing
blocks above each chain's irreversible line, and the rollback journal, both
backends' undo paths, `BlockStore::remove`, `ReorgTooDeep` and `reorg_retention`
all became unreachable and were deleted — no feature lost, no test weakened.
That is the proof of the pattern. There are three more instances of it in the
tree, and the plan in §5 is mostly the work of removing them.

### 0.1 The four causes, measured

| # | Structural decision | Machinery it forces | Measured today | Removed by |
|---|---|---|---|---|
| 1 | **Two owners of one address list.** `wallets` holds the watch list; the index must ask for it *during* a pass | `FilterSource`, `NetworkAdmission`, `SyncPlan` (+ its `revision`, its `detached` constructor), `CommitPermit`, `PublicationPermit`, and the `wallets` side that feeds them | `sdk/indexing/src/admission.rs` **380 production lines** (741 with tests); `engine/service.rs:36-53` the trait; `sdk/wallets/src/selection.rs` **47 production lines**; ~220 more in `wallets.rs` (`admissions` map `:123`, `admission` `:128`, `filters` `:331`, the permit plumbing `:365-373`) | §5 step 3 — the index owns its list |
| 2 | **Committing the tip.** Blocks above the irreversible line were written durably, so they had to be undoable | a rollback journal, both backends' undo code, `BlockStore::remove`, `ReorgTooDeep`, `reorg_retention` | **already deleted by `025`** — the proof of the pattern | done |
| 3 | **A key-value abstraction built for backends that never came** | conditional writes with a global version counter, an owner thread, a command queue, a value-framing codec | `sdk/indexing/src/storage/redb/**`: **2,252 production lines** of 4,347; of those the thread + queue + read/write/error plumbing is **904** (`backend.rs` 324, `write.rs` 182, `read.rs` 137, `path.rs` 87, `support.rs` 84, `format.rs` 59, `probe.rs` 31) and the framing another **349** (`codec.rs` 259, `batch.rs` 90) | `027` took the crate boundary; the queue itself is §7 open question **G4** |
| 4 | **Names that do not say what they hold** | a reader has to open the file to learn what it is | `Indexer` (root trait) vs `Index<R>` (`engine/index.rs:20`, the read side) vs each chain's `Indexer` newtype vs `IndexerParams`; `observation.rs` (vocabulary) vs `observer.rs` (callback port); `NetworkRules` (`network.rs`) — a trait with exactly one implementation, `impl NetworkRules for base::Network`, existing **only** so two functions can be written with a dot, because `IndexError` is in `indexing` and `Network` is in `base` | §5 steps 3 and 8, and **G3** |

**On `SyncPlan`, named specifically.** It is not a plan. It is a snapshot of the
filters, the checkpoint they were read against, and a `revision` counter
(`admission.rs:193-207`) — a *version receipt*, so a commit can prove the list
did not change under it (`admission.rs:165-168`). The receipt is only needed
because someone else owns the list. The trait's own doc comment says so:

> "Synchronization reads this *after* it observes the source tip, and the
> ordering is load-bearing. … A selection captured before the tip was observed
> cannot contain an address registered in between, so the blocks that tip
> admits … would be indexed without it, and nothing rescans them."
> — `sdk/indexing/src/engine/service.rs:23-32`

That reasoning is correct. It stops applying the moment the synchronizer reads
its own set, under its own lock, at the moment it chooses. `026` §3.4 reached
the same conclusion and recorded that its earlier draft had been wrong.

### 0.2 Two things that make it *look* worse than it is

Neither is a design defect, and both mislead a first reading:

| Effect | Measured | Consequence for a reader |
|---|---|---|
| **In-file tests dominate the byte count** | `sdk/indexing/src/storage/redb/**` is 2,252 production lines of **4,347** — 48% tests. `backend.rs` alone is **324 production / 1,097 test** | The storage half looks three times its real size. `linter.toml:70-72` counts production lines, which is why a 1,421-line file passes a 500-line cap |
| **The most important code is invisible to grep** | No chain contains `impl Indexer for …`: all three are generated by `indexing::delegate_indexer!` (`sdk/chains/{bitcoin/src/indexer.rs:89, ethereum:47, solana:57}`), and all three alias the trait on import — `BlockInterpreter as Interpret` (`sdk/chains/{bitcoin,ethereum,solana}/src/interpreter.rs:7-8`) | A reader looking for "how does a chain become an indexer" finds nothing where they look first |

The macro earns its keep — it is 77 lines (`engine/service.rs:247-323`) standing
in for the same forwarding written three times — but §5 step 8 asks that the
crate README name it in the first screen, and that the alias go.

### 0.3 What the diagnosis predicts

If the four causes are real, then fixing them should *delete* code rather than
move it. The budget in §5.4 says: **≈ −1,200 production lines** across the
structural half of the plan, while adding four capabilities the SDK does not
have today (a reachable transaction builder — already landed; index-owned
observation; a storage hook that works beside the bundled store; chains
constructible from a value). That asymmetry is the test of whether this document
is right.

---

## 1. The target SDK, in one page

### 1.1 Crate map

| Crate | Owns, in one line | Change |
|---|---|---|
| `packages/decimal` | an exact decimal with no arithmetic | — |
| `packages/crypto` | curves, keys, signatures, `SecretBytes` | — |
| `packages/json-rpc` | the HTTP/JSON-RPC transport, failover and retry as data | — |
| `packages/http-server` | listener, auth mode, the two health paths | — |
| **`sdk/config`** | the configuration document and its registry: `Registry`, `Chain`, `Sections`, `Chains`, the shared shell (`Rpc`, `Repetition`, `Cadence`), and (behind feature `toml`) `Document::read` | **new** (`028` step 1, moved out of `packages/` by step 11) |
| `sdk/base` | chain-neutral values and capability traits: `Address`, `Asset`, `Network`, `Decimal`, `Signer`, `TransactionBuilder`, `SignedTransaction`, `Draft`, `SubmissionRegistrar` | grows `Claim` |
| `sdk/indexing` | **the index**: what a chain implements, what a store implements, what an application calls, and the engine that drives it | grows observation, balance, history; loses `FilterSource` |
| `sdk/wallets` | **a map of chain factories**, the abstraction over what they produce, and the composition that assembles them | loses history/balance/watching; gains `Composition` |
| `sdk/chains/{bitcoin,ethereum,solana}` | one chain each: `Config`, source, interpreter, wallet factory, outbox | gains `config.rs` |
| `apps/api` | the HTTP contract — routes, wire types, OpenAPI | — |
| `apps/server` | the binary: read a config document, compose, listen, supervise, rescan | shrinks to plumbing |
| `examples` | one directory per scenario, every chain inside it as setup | gains a `config.toml` scenario |

**Two crates disappear from the current tree** (`packages/storage`,
`packages/storage/redb`) and three more already have (`indexing-redb`,
`indexing-postgres`, `indexing-supervisor`) — `027`, landed.

### 1.2 Dependency direction

```
packages/{decimal,crypto,json-rpc,http-server,config}
        │
        ▼
     sdk/base ──────────────► sdk/indexing ──────────► sdk/wallets ──────► sdk/chains/*
                                    │                       │                   │
                                    └──────────► apps/api ◄─┘                   │
                                                     │                          │
                                              apps/server ◄────────────────────-┘
                                              examples
```

One rule, and it is the one the tree breaks today: **every arrow points one
way.** `indexing → wallets` exists right now, as a trait object rather than a
Cargo dependency, which is why `linter.toml:358-361` never caught it: the
synchronizer calls back into `wallets` through `FilterSource`
(`service.rs:36-53`, `sdk/wallets/src/selection.rs:12`). Removing that edge is
step 3 and is the single largest structural change left.

Inside `sdk/indexing` the same rule applies to modules, and it is also broken
today: **the root must not name anything under `engine/` or `storage/`.**

```
sdk/indexing/src/lib.rs          the public surface
  error.rs  network.rs  source.rs  block.rs  observation.rs  output.rs
  indexer.rs  health.rs  status.rs                                 ← root: the contracts
  holder.rs  watch.rs  balance.rs  commit.rs                       ← root: new (steps 1, 3, 6)
  storage.rs + storage/{block,history,output,registry,balance,redb}.rs  ← what a store implements
  engine.rs  + engine/{service,synchronizer,composer,overlay,policy,index,supervisor}.rs
```

(`observer.rs` is deleted by step 1; `commit.rs` replaces it.)

The violation, verbatim:

```rust
// sdk/indexing/src/indexer.rs:52-55 — the root trait names two engine types
fn sync<'a>(
    &'a self,
    selection: &'a dyn crate::FilterSource,                    // engine/service.rs:36
) -> BoxFuture<'a, Result<Vec<crate::SyncStatus>, IndexError>>; // engine/service.rs:74
```

`FilterSource` disappears with step 3. `SyncStatus` is vocabulary a caller
reads, so it **moves to root** (step 8) — it is not "how a pass runs", it is
what a pass reports.

### 1.3 The three seams a user meets

Everything in §2 and §3 is one of these three.

| Seam | You write | The SDK gives you | Where it lives |
|---|---|---|---|
| **1. Implement a chain** | `BlockSource`, `BlockInterpreter`, `WalletFactory` (+ `AddressParser`, `FactoryProfile`), a `Config` with `connect`/`compose` | `Service` + `delegate_indexer!` turn it into an `Indexer`; `Wallets` turns the factory into `Arc<dyn Wallet>` | `sdk/indexing` root, `sdk/wallets` |
| **2. Implement (or accept) storage** | either nothing — take the bundled redb store — or an `index.listen(..)` hook beside it, or the four store ports yourself | durable history, outputs, checkpoint, watch list; a notification after every committed block | `sdk/indexing/storage/`, `sdk/indexing/commit.rs` |
| **3. Drive and read** | `index.watch(..)`, `index.listen(..)`, `index.sync()` (or hand the index to `Supervisor`), `index.balance(..)`, `index.history(..)`, `wallet.transaction()`, `wallet.send(..)` | one object per question, no chain named after the composition root | `sdk/indexing` root, `sdk/wallets` |

The rule that makes seam 3 worth having is enforced already: **after the
composition root, no module may name `chain_bitcoin`, `chain_ethereum` or
`chain_solana`** (`linter.toml:369-397`, `scripts/check-chains.sh` check 8).
Today there is exactly one hole in it, and `026` commit 2 closed it: reaching a
transaction builder used to require an `Any` downcast to the concrete chain
wallet.

---

## 2. Usage, end to end

Every block is real Rust against real signatures. `// today` is what a caller
writes in the current tree; `// target` is what this document asks for. Nothing
is elided and nothing is pseudocode.

### 2.1 Configure the chains — in code, from any serde format, or from a file

```rust
// today — impossible outside apps/server. The config types are private to the
// binary (`apps/server/src/chains/bitcoin.rs:17` is `pub(super) struct Config`),
// they are deserialize-only, and composing them is 532 production lines of
// `apps/server/src/chains.rs` that a library embedder cannot call.
```

```rust
// target — the config types ARE the API (028 §1.1). No file, no parser.
use chain_bitcoin::{Config, Network};
use config::{Retry, Rpc, Sync};

let bitcoin = Config {
    confirmation_depth: 100,                 // this deployment's own safety trade
    sync: Sync { poll_millis: 2_000, batch_size: 250 },
    ..Config::new(Network::Regtest, Rpc::endpoint(&btc_url)?, BTC)
};

let ethereum = chain_ethereum::Config {
    tokens: vec![chain_ethereum::Token::new(usdc, USDC, 6)],
    ..chain_ethereum::Config::new(
        chain_ethereum::Network::new(1, "mainnet"),
        Rpc { endpoints: vec![primary, backup], retry: Retry::default() },
        ETH)
};

// the same types, from the embedder's own format — the SDK supplies no reader:
let bitcoin: chain_bitcoin::Config<MyAsset> = serde_json::from_value(their_json)?;

// or from a TOML file, the convenience, behind feature `toml`:
let document = config::Document::read("payment.toml").await?;
```

`Config::new` fills every default the `#[serde(default = …)]` attributes fill,
by calling the same functions — so the in-code path and the parsed path cannot
drift (`028` §5.1). `Config<A>` is generic over the asset tag because
`AssetTag` lives in `apps/api` and `linter.toml:369-373` forbids a chain crate
from naming it (`028` §7.3).

### 2.2 Compose — identity first, then storage, then the map

```rust
// today — apps/server/src/chains.rs:125-173, private to the binary
let (connections, store) = startup::verify_then_open(verification, || storage.open()).await?;
let mut chains = Vec::with_capacity(connections.len());
for connection in connections { chains.push(connection.chain(&store, &registrar).await?); }
let composer = Arc::new(Composer::new(chains.iter().map(|c| c.index.clone()).collect())?);
let mut wallets = Wallets::new(composer.clone());
let mut tags = Vec::new();
for chain in chains {
    for (tag, factory) in chain.factories {
        wallets.register(tag.clone(), factory, Some(chain.registry.clone()))?;  // 3 args
        tags.push(tag);
    }
}
for tag in &tags { wallets.restore(tag).await?; }                                // per asset, &mut
```

```rust
// LANDED (step 10) — 028 §9.1 + 026 §1.1, callable by any embedder.
// `examples/all_chains/main.rs` is this, compiled.
use wallets::{Composition, Connection};

// PHASE 1 — identity only. No storage opens on any path. The chain crates are
// named exactly here and nowhere below this line.
let verified: Vec<Box<dyn Connection<Asset, Repository>>> = vec![
    Box::new(bitcoin.connect(&secrets).await?),
    Box::new(ethereum.connect(&secrets).await?),
    Box::new(solana.connect(&secrets).await?),
];

// PHASE 2 — open the store, compose each chain over it, assemble once.
let store = open_my_store().await?;
let Composition { index, wallets, schedules } = Composition::build(
    verified,
    |network| store.repository(network),
    &registrar,
).await?;
//  index:     Arc<indexing::Composer>   — every chain, one Indexer
//  wallets:   wallets::Wallets<Asset>   — every factory under its tag
//  schedules: indexing::Schedules       — per-network cadence for the Supervisor
```

No `Deps`. `registrar` is the only dependency composition takes that a chain
cannot build for itself, so a one-field aggregate would be a type to learn for
nothing; it is passed once, to `build`, and forwarded to each chain.
`Composition::verify_then_build(verification, open, repository, &registrar)` is
the same fold with the phase order stated rather than remembered — it never
calls `open` if an identity check fails or the caller drops the future while
they are running — and is what `apps/server/src/chains.rs` uses.

`Wallets<Asset>`, not `Wallets<Id, Asset>`: with no instance store there is no
id to parameterise over (`029` §8.1).

Two phases, not one, and this is the constraint that makes a single
`Chain::from_config(value)` impossible: **every identity check must finish
before any storage is opened** (once `apps/server/src/startup.rs:6-18`, now
`Composition::verify_then_build` with its four ordering tests). One call cannot
straddle that; two can, and do.

### 2.3 Create or import a wallet

```rust
// today — apps/api/src/api/wallet.rs; `generate` and `adopt` return Arc<dyn Wallet>
//         (026 commit 2, landed), but registration still takes three arguments
let wallet: Arc<dyn Wallet> = wallets.generate(id, &BTC).await?;
let secret = SecretBytes::from_hex(text).ok_or_else(Error::invalid_secret)?;   // 026 c4
let wallet = wallets.adopt(id, &BTC, secret).await?;      // &self, birthday = checkpoint+1
wallets.import(id, &BTC, secret, BlockPosition(842_000)).await?;  // &mut self, startup only
wallets.insert(id, &BTC, wallet, BlockPosition(842_000)).await?;  // &mut self, startup only
```

```rust
// target — 029 §1.1: the map makes wallets and keeps nothing. No id, no index,
// no start. Three ways in, one reason each, none startup-only.
const BTC: AssetTag = AssetTag::from_static("btc");   // invalid text fails to COMPILE

let wallet: Arc<dyn Wallet> = wallets.create(&BTC).await?;      // the SDK makes the key
let wallet = wallets.from(&BTC, secret).await?;                 // the caller supplies it
wallets.accepts(&BTC, &*foreign_wallet)?;                       // is this one of mine?

// key-first, which is inexpressible today:
let secret = wallets.secret(&BTC).await?;    // the chain's own curve and format
store_offline(&secret)?;                     // custody is the caller's problem, explicitly
let wallet = wallets.from(&BTC, secret).await?;

println!("{}", wallet.address().value);      // Holder       — no chain named
wallet.sign(request).await?;                 // base::Signer — no chain named
wallet.send(to, amount).await?;              // SingleSender — no chain named
let mut builder = wallet.transaction();      // TransactionFactory — landed, 026 commit 2
```

**No `id` anywhere, and this is the owner's first position applied in full.**
`026` kept an instance store on `Wallets` and an `I` type parameter for it;
`029` §1 removes both, because a factory map that also holds instances is four
things at once. The id→wallet map does not disappear — it becomes the caller's,
and is fifteen lines once publication, persistence, custody and filters are gone
(`029` §3.2's `Signers`, three methods, living in `apps/api` beside `Payments`).
`026`'s `KeyStore` goes with it: its only job was refilling an instance store
that no longer exists.

The one thing this costs, stated rather than smoothed away: **two durable facts
about one id now live in two stores** — the app's key and the index's watch row —
with a crash window between the writes. The rule that contains it is an ordering
rule, and it makes the failure strictly better than today's:

| Order | Crash between the two writes | Result |
|---|---|---|
| key first, then watch | key kept, nothing watched | **bad** — zero balance and empty history forever, and nothing says why |
| **watch first, then key** | watched, no key | **benign** — exactly a watch-only address, visible in `index.watched`, correct history, repaired by re-importing the key |

`Watching::watch` commits before the key is kept. Today's equivalent is worse: a
failed registry write silently un-creates a wallet the caller was already handed
(`sdk/wallets/src/wallets.rs:401-405`).

### 2.4 Register it for observation — the index owns the list

```rust
// today — the collection owns the watch list, and the index calls BACK into it
let address = wallets.parse(&BTC, text)?;
wallets.watch(id.to_owned(), &BTC, &address).await?;   // wallets/persistence.rs:84
// and indexing reads it back through a trait implemented on the collection:
//   impl FilterSource for Wallets              (sdk/wallets/src/selection.rs:12)
//   index.sync(&*wallets).await?               (sdk/indexing/src/indexer.rs:52-55)
//   Supervisor::new(indexer, wallets.clone(), schedules, monitor)
```

```rust
// target — 026 §1.4 + 029 §5.2. The index owns its durable list; wallets is
// never consulted, and the caller supplies the id.
let wallet = wallets.create(&BTC).await?;
let id = uuid::Uuid::now_v7().to_string();
let watch: Watch = index.watch(&id, &*wallet, Start::Fresh).await?;   // FIRST
signers.keep(id.clone(), wallet.clone())?;                           // then the key

// a watch-only address: the same call, no wallet
let address = wallets.parse(&BTC, text)?;                  // the factory parses
let asset   = wallets.asset(&BTC)?.asset.id;               // tag -> AssetId
index.watch("btc-observed", &Holding { address, asset }, Start::At(BlockPosition(842_000)))
    .await?;

// an imported wallet, whose key may have history the index has not seen:
let wallet = wallets.from(&BTC, secret).await?;
index.watch(&id, &*wallet, Start::At(BlockPosition(842_000))).await?;

index.sync().await?;                          // no selection argument
Supervisor::new(index, schedules, monitor);   // no selection argument
```

```rust
/// Where an address begins to be observed. No default and no `Option`: a
/// birthday below the checkpoint used to be accepted silently and index
/// nothing (024 §1.2).
pub enum Start {
    /// This key or address has never been used. An assertion of fact, not a
    /// policy — a caller who does not know cannot honestly write it.
    Fresh,
    /// Observe from this native position. Below the checkpoint it is
    /// backfilled (024 §3.3), never discarded.
    At(BlockPosition),
}
```

A start far in the past is refused **synchronously**, at the `watch` call, by
`024`'s `max_backfill_blocks` check — `422` naming the earliest acceptable
position, so the caller learns immediately rather than after a long scan.

`watch` returns a `Watch { id, address, asset, start, observation }`, and
`watched(network)` lists them in id order. `024`'s `Observation` rides on it:
`WalletInfo` is gone, so the coverage enum lands where the watch row is. **There
is deliberately no `unwatch`** — `Registry` is at `linter.toml:140-142`'s
three-method cap once `advance` lands, and an unwatched address's committed
history has no defined fate short of `purge_network`, which is offline and takes
a whole network.

> **Both grounds are now void, and `unwatch` is built** (G10, §5.5 (b)).
> `Registry::advance` was never built — a backfilling chunk writes its own
> progress — so the third method went to `unregister`. And the fate of the rows
> is stated rather than left open: they are **kept**, which makes the act
> reversible and is why it is safe to expose. `Watching` is
> `watch`/`unwatch`/`watched`, exactly at the cap.

### 2.5 Supply storage — or just a hook

```rust
// today — one shape, and the callback is unreachable through the trait object.
// `observe` is an inherent &mut method on the concrete Service / chain newtype
// (sdk/indexing/src/engine/service.rs:150, and the same in delegate_indexer!
// at :258), so an Arc<dyn Indexer> or a Composer cannot install one at all.
let store = Redb::open(path).await?;
let repository = store.repository(&network);
```

```rust
// target — 029 §1.2. Three cases, in increasing order of effort.

// (a) the bundled store, no callback — unchanged
let database = indexing::redb::Database::open(path).await?;
let repository = indexing::redb::Repository::new(database, &network)?;   // four ports

// (b) the bundled store PLUS a reaction — impossible today on a composed index
index.listen(Arc::new(Credits(tx)));       // &self, on the Indexer trait
index.listen(Arc::new(Metrics::default()));// and it is a LIST, not a slot

// (c) a fully custom store — the four ports, unchanged, with the contract
//     suites that already ship to prove them
impl BlockStore       for MyStore { /* get, add, purge_network — add() may VETO */ }
impl TransactionStore for MyStore { /* list(HistoryQuery)  -> CanonicalPage */ }
impl OutputStore      for MyStore { /* list(OutputRequest) -> OutputPage    */ }
impl Registry         for MyStore { /* register, registered, advance        */ }
indexing::testing::HistoryContract::new(MyStore::empty()).run().await;
```

```rust
/// The hook. Synchronous, cannot fail, cannot veto: hand the work to a queue.
struct Credits(tokio::sync::mpsc::UnboundedSender<indexing::BlockCommit>);

impl Listener for Credits {
    fn committed(&self, commit: &indexing::BlockCommit) { let _ = self.0.send(commit.clone()); }
}
```

**Storage and callback stay two concepts, because only one of them can veto.**
A user-supplied `BlockStore` already *is* a commit callback receiving everything
transactionally, with the right to refuse a block (`storage/block.rs:16-22`) —
that answers case (c) today. What it cannot do is let someone *keep* the bundled
redb store and still react, which is case (b), and which is impossible right now
because `observe` is an inherent `&mut self` slot on `Service` and each chain
newtype (`engine/service.rs:150`, `:258`) that `Composer` does not have at all.

`BlockCommit` carries the network, the block, the matching transactions, the
output changes **and** the unknown spends. Delivery is **at most once and never
retracted** — nothing above the confirmed line is ever announced, which is what
`025` bought. A listener that must not miss a block reads `Checkpoint` when it
starts and catches up through `History`; this is a wake-up, not a log. `029` §6
has the full guarantee table.

### 2.6 Read a balance — pass the wallet, get one object

```rust
// today — sdk/wallets/src/wallets.rs:279. One call, three sources, two units:
let balance: wallets::Balance = wallets.balance(&id).await?;
//  balance.amount      <- the chain wallet's BalanceReader  (DISPLAY units)
//  balance.pending     <- self.history.unconfirmed(..)      (ATOMIC units)
//  balance.observed_at <- self.checkpoint.checkpoint(..) when the chain
//                         returned None, i.e. on Ethereum and Solana
```

```rust
// target — 026 §1.5. The index answers; the subject is the wallet itself.
let at: indexing::Balance = index.balance(&*wallet).await?;

println!(
    "{} settled at block {:?}; {} arriving and {} leaving as of block {:?}",
    at.amount, at.observed_at.map(|b| b.height.0),
    at.pending.incoming, at.pending.outgoing, at.pending.block.map(|b| b.height.0),
);

// watch-only — no wallet object exists, so the subject is a bare holding:
let at = index.balance(&Holding { address, asset }).await?;
```

```rust
// sdk/indexing/src/balance.rs — new
/// One address's holding of one asset, on both sides of the confirmed line.
/// Every amount is in the asset's ATOMIC units: `indexing` never learns an
/// asset's decimals (021 §5.2), so this object is built by a crate that
/// physically cannot produce display units.
pub struct Balance {
    pub amount: Decimal,                 // settled, folded over durable rows only
    pub observed_at: Option<BlockRef>,   // the confirmed line it was folded at
    pub pending: PendingValue,           // { incoming, outgoing, block } — 025's type
}
```

Three defects close with this move, all of them consequences of split ownership
rather than mistakes:

| Defect today | Evidence | Why the move fixes it |
|---|---|---|
| `amount` is display units, `pending` is atomic, side by side on the wire | `sdk/chains/ethereum/src/wallet.rs:349` converts; `sdk/indexing/src/engine/overlay.rs:214` does not | `Balance` is constructible only inside `indexing`, which has no access to decimals — the mistake becomes structurally impossible |
| `observed_height` names a block the amount was **not** read at on two chains | `sdk/chains/ethereum/src/wallet.rs:332-337` says so itself | one owner folds both halves at one line |
| watch-only returns `422` | a balance needed a chain wallet | a fold over indexed movements needs no key → `200` |

The wire body is unchanged: `025`'s five fields, same names, same nullability
(`apps/api/src/api/wallet.rs:206-223`).

### 2.7 List history

```rust
// today — index -> wallets::History -> payment_api::TransactionPage (THREE models
// of the same transaction; sdk/wallets/src/history.rs is 400 production lines)
let page: wallets::History = wallets.history(&id, HistoryRequest::first(50)).await?;
```

```rust
// target — index -> payment_api::TransactionPage (two models)
let page = index.history(&*wallet, Page::first(50)).await?;
let more = index.history(&*wallet, Page::after(cursor, 50)).await?;
let page = index.history(&Holding { address, asset }, Page::first(50)).await?;  // watch-only
```

Paging stays a real second argument — a cursor and a limit are properties of
*this request*, not of the wallet. `HistoryQuery { address, asset, after, limit }`
(`sdk/indexing/src/storage/history.rs:50-59`) is the two questions fused;
splitting them is what lets the subject be a wallet.

### 2.8 Build a transaction, inspect it, sign it, broadcast it

```rust
// today — LANDED (026 commits 2 and 3). No downcast, both inspection points typed.
let wallet: Arc<dyn Wallet> = wallets.wallet(&id)?;
let mut builder = wallet.transaction();                 // TransactionFactory, a supertrait

builder.transfer(wallets.parse_address(&id, "bc1q…")?, Decimal::from(1_u64))?;
builder.transfer(wallets.parse_address(&id, "bc1p…")?, Decimal::from(2_u64))?;
//        ^ Bitcoin takes many; Ethereum and Solana return Unsupported on the second.

let draft: base::Draft = builder.draft()?;              // intent. No RPC, no key.
persist(&draft.snapshot).await?;                        // durable; resumable below
let signed: SignedTransaction = builder.prepare().await?;
println!("id {} costs {} in fees", signed.id(), signed.fee());   // cost, before broadcast
if *signed.fee() > ceiling { return Err(TooExpensive); }         // and simply drop it
let submission = wallet.broadcaster().broadcast(&signed).await?;

// and the restart path:
let mut builder = wallet.resume(&snapshot)?;            // rejects another wallet's snapshot
```

Before `026` this required an `Any` downcast to the concrete chain type
(`sdk/chains/solana/src/builder.rs:228-233`) — which `apps/api` and every SDK
embedder are forbidden to write (`linter.toml:376-378`). It was the one place
"don't import each chain just to send funds" was broken.

Two semantics a caller must be told, both already implemented and neither
obvious:

| Fact | Where | Consequence |
|---|---|---|
| Between `prepare` and `broadcast` the outputs are **released, not locked** | `sdk/chains/bitcoin/src/sender/outbox.rs:126-129` | holding a signed transaction is not a reservation; a concurrent `send()` may take its inputs and the broadcast then fails *definitely*, not ambiguously |
| A Solana `SignedTransaction` **expires** in about a minute | `sdk/chains/solana/src/builder.rs:15-18` | durable data that is also perishable, on one chain only |

What remains here is `Claim` (§5 step 9): what the outbox claimed to build it —
spent outpoints, a nonce, or a blockhash lifetime — so a caller holding two
signed transactions can tell whether they collide.

### 2.9 React to a chain going unhealthy

```rust
// today, and unchanged — 019 landed this and it is the part of the SDK that
// already works the way the rest should.
let (monitor, health) = MyMonitor::new();        // impl indexing::SyncMonitor
let supervisor = Supervisor::new(index.clone(), wallets.clone(), schedules, monitor);
//                                              ^^^^^^^^^^^^^^^ the selection argument
tokio::spawn(supervisor.run(shutdown));

let snapshot: SyncHealth = health.borrow().clone();
for network in snapshot.networks() {
    match network.state {
        NetworkState::Healthy    => {}
        NetworkState::Degraded   => warn(&network.failure),   // retryable, backing off
        NetworkState::Quarantined => page(&network.failure),  // terminal; needs --rescan
    }
}
if !snapshot.ready() { /* GET /health/ready answers 503 */ }
```

```rust
// target — one argument fewer, everything else identical
let supervisor = Supervisor::new(index.clone(), schedules, monitor);
```

One chain's failure does not stop the others: `Composer::units()` are driven on
independent tasks, a `Conflict` keeps the healthy cadence, a retryable error
backs off with jitter, a terminal error quarantines the network *alive*, and a
panicked task is respawned parked (`sdk/indexing/src/supervisor/unit.rs`,
`supervisor.rs`). That is 561 production lines of failure policy that six
callers share, and `027` §5.1 records why deleting it was rejected.

---

## 3. The API surface

Status key: **T** = exists today, unchanged · **C** = exists, changes · **N** = new ·
**D** = deleted.

### 3.1 Implement this — a chain

| Item | Signature | Status |
|---|---|---|
| `ChainTip` | `fn tip(&self) -> BoxFuture<Result<BlockRef, SourceError>>`; `fn confirmed_tip(&self) -> BoxFuture<Result<BlockRef, SourceError>>` | **T** (`025`) |
| `BlockSource: ChainTip` | `type Block: IndexedBlock;` `fn blocks(&self, from, to, limit)`; `fn canonical_at(&self, position)` | **T** |
| `BlockSource::earliest` | `fn earliest(&self) -> BoxFuture<Result<Option<BlockPosition>, SourceError>>` — the availability floor a backfill is checked against | **N** (`024` §7) |
| `BlockInterpreter` | `fn inspect(&self, block: &B, addresses: &[Address]) -> Result<InterpretedBlock, IndexError>` | **T** |
| `WalletFactory: AddressParser + FactoryProfile` | `fn create(&self, secret: SecretBytes) -> FutureResult<Arc<dyn Wallet>>`; `fn generate(&self) -> FutureResult<Generation>`; `fn validate(&self, wallet: &dyn Wallet) -> Result<(), Error>` | **T** (`SecretParser` deleted, `026` c4) |
| `chain::Config<A>` | `fn new(network, rpc, asset) -> Self`; `fn validate(&self) -> Result<(), Error>`; `fn network(&self) -> base::Network`; `async fn connect(self, &dyn Secrets) -> Result<Connected<A>, Error>` | **N** (`028` §8.1) |
| `Connected<A>` | `async fn compose<R>(self, repository: R, deps: &Deps) -> Result<ComposedChain<A>, Error>` | **N** |
| `Wallet` (what a chain's wallet satisfies) | `Holder + SingleSender + TransactionFactory + Signer + Send + Sync + Any` | **C** — `Addresser`+`BalanceReader` → `Holder` |
| `BalanceReader` + three chain impls | — | **D** (`026` §2.2) |
| `delegate_indexer!` | turns a `Service` into an `Indexer` newtype | **T** |

### 3.2 Implement this — storage, and the callback

| Item | Signature | Status |
|---|---|---|
| `BlockStore` | `fn get(&self, network) -> BoxFuture<Result<Option<BlockRef>, IndexError>>`; `fn add(&self, addition) -> BoxFuture<Result<BlockOutcome, IndexError>>`; `fn purge_network(&self, network) -> BoxFuture<Result<PurgedRows, IndexError>>` (`storage/block.rs:107`) | **T** — at the 3-method cap, so anything new needs its own port |
| `TransactionStore` | `fn list(&self, request: HistoryQuery) -> BoxFuture<Result<CanonicalPage, IndexError>>` (`storage/history.rs:124`) | **T** |
| `OutputStore` | `fn list(&self, request: OutputRequest) -> BoxFuture<Result<OutputPage, IndexError>>` (`storage/output.rs:43`) | **T** |
| `Registry` | `fn register(&self, entry) `; `fn registered(&self, network)`; **`fn advance(&self, id, done_through)`** | **C** — gains `advance`; `RegisteredAddress` **loses `material`** and its `asset: String` becomes `AssetId` |
| `BalanceStore` | `fn balance(&self, query: BalanceQuery) -> BoxFuture<Result<Balance, IndexError>>` — the store-side fold | **N** (`021` §5.3) |
| `BackfillStore` | `fn backfill(&self, batch: BackfillBatch) -> BoxFuture<Result<BackfillOutcome, IndexError>>` — writes into blocks already committed, never moves the checkpoint | **N** (`024` §3.4) |
| `Listener` | `fn committed(&self, commit: &BlockCommit)` — synchronous, cannot fail, cannot veto | **N** (`029` §6.2), **replacing `Observer`** |
| `Observer` / `BlockObservation` / `Service::observe` / three `IndexerParams::observer` fields | — | **D** (`029` A) |
| `apps/api::Signers` | `fn keep(&self, id: String, wallet: Arc<dyn Wallet>) -> Result<(), Error>`; `fn get(&self, id: &str)`; `fn ids(&self)` — the caller's id→wallet map | **N** (`029` §3.2) — deliberately **not** in `sdk/wallets` |
| `wallets::KeyStore` (`026` §2.5) | — | **not built** — `029` drops it; its only job was refilling an instance store that no longer exists |
| `indexing::testing::Memory` | an in-memory second implementation of all four ports | **T** (`027`) |

### 3.3 Call this

| Item | Signature | Status |
|---|---|---|
| `Wallets<A>` | `fn new() -> Self` — no index, no keys, no `I` | **C** (`029` §8.1 supersedes `026` §2.1) |
| `Wallets::add` | `fn add(&mut self, asset: A, factory: impl WalletFactory + 'static) -> Result<(), Error>` | **C** — was `register(asset, factory, registry)` |
| `Wallets::create` | `fn create(&self, asset: &A) -> FutureResult<Arc<dyn Wallet>>` | **C** — was `generate(id, asset)`; **no id** |
| `Wallets::from` | `fn from(&self, asset: &A, secret: SecretBytes) -> FutureResult<Arc<dyn Wallet>>` | **C** — was `adopt` + `import`; **no id, no birthday** |
| `Wallets::accepts` | `fn accepts(&self, asset: &A, wallet: &dyn Wallet) -> Result<(), Error>` | **C** — was `insert`; validates `Origin` without storing |
| `Wallets::secret` | `fn secret(&self, asset: &A) -> FutureResult<SecretBytes>` | **T** (`026` c4) |
| `Wallets::send_all` | `fn send_all(&self, asset: &A, transfers: Vec<Transfer>) -> SendFuture` | **C** — the map proves one registered asset; it no longer needs ids to do it |
| `Wallets::{asset,assets,parse}` | unchanged | **T** |
| `Wallets::{balance,history,watch,filters,parse_secret,generate,adopt,import,insert,register,wallet,get,list,send,restore,parse_address}` | — | **D** |
| `Indexer` | `trait Indexer: Checkpoint + History + Watching (+ Balances) { fn networks(&self) -> &[Network]; fn listen(&self, listener: Arc<dyn Listener>); fn sync(&self) -> BoxFuture<Result<Vec<SyncStatus>, IndexError>>; }` | **C** — loses `&dyn FilterSource`, gains `listen`; three methods, at the cap |
| `Watching::watch` | `fn watch(&self, id: &str, subject: &dyn Holder, start: Start) -> BoxFuture<Result<Watch, IndexError>>` | **N** |
| `Watching::watched` | `fn watched(&self, network: &Network) -> BoxFuture<Result<Vec<Watch>, IndexError>>` | **N** |
| `Watching::unwatch` | `fn unwatch(&self, id: &str) -> BoxFuture<Result<Option<Watch>, IndexError>>` | **N** — refused when this was written (`029` §10), built once §5.5 (b) needed it; `Watching` is at the three-method cap |
| `Backfilling::cancel` | `fn cancel(&self, network: &Network) -> BoxFuture<Result<Vec<Watch>, IndexError>>` | **N** — `024` §7's cancellation, a port of its own because it is a durable write and `SyncControl` is a signal |
| `Balances::balance` | `fn balance(&self, of: &dyn Holder) -> BoxFuture<Result<Balance, IndexError>>` | **N** |
| `History::history` | `fn history(&self, of: &dyn Holder, page: Page) -> BoxFuture<Result<TransactionPage, IndexError>>` | **C** — was `history(HistoryQuery)` |
| `History::unconfirmed` | — | **D** — folds into `Balances::balance` |
| `Checkpoint::checkpoint` | `fn checkpoint(&self, network) -> BoxFuture<Result<Option<BlockRef>, IndexError>>` | **T** |
| `Composer` | `fn new(indexers: Vec<Arc<dyn Indexer>>) -> Result<Self, IndexError>`; `fn units(&self) -> &[SyncUnit]`; `impl Indexer for Composer` | **T** — gains `watch`/`watched`/`listen` with the trait. `units()` and `OutputStore::list` stay `Composer`- and `Service`-only, deliberately (`029` §7.3) |
| `Supervisor::new` | `fn new(units: Arc<Composer>, schedules: Schedules, monitor: Arc<dyn SyncMonitor>) -> Self` | **C** — loses the `Arc<dyn FilterSource>` second parameter (`sdk/indexing/src/supervisor.rs`) |
| `Supervisor::{control, run}` | `fn control(&self) -> SyncControl`; `async fn run(self, shutdown: watch::Receiver<bool>)` | **T** |
| `SyncControl::resume` | `fn resume(&self, network: &Network) -> Result<(), IndexError>` | **T** |
| `SyncControl::cancel_backfill` | `fn cancel_backfill(&self, network: &Network) -> Result<(), IndexError>` | **N** (`024` §7) |
| `Composition::{build, verify_then_build}` | see `028` §9.3 | **N** |
| `wallet.transaction() / resume() / broadcaster()` | `TransactionFactory` | **T** (`026` c2) |
| `TransactionBuilder::{transfer, draft, prepare}` | `fn draft(&self) -> Result<Draft, Error>` | **T** (`026` c3) |
| `FilterSource`, `AddressFilter` as a public input | — | **D** |

### 3.4 Receive this

| Item | Shape | Status |
|---|---|---|
| `Holder` | `trait Holder { fn address(&self) -> &Address; fn asset(&self) -> &AssetId; }` | **N** |
| `Holding` | `struct Holding { address: Address, asset: AssetId }` | **N** |
| `Start` | `enum Start { Fresh, At(BlockPosition) }` | **N** (`024` §2.1 spelt `Tip`/`At`; `026` §1.4 refines to `Fresh`/`At`) |
| `Balance` | `{ amount: Decimal, observed_at: Option<BlockRef>, pending: PendingValue, completeness: Completeness }` — all atomic units | **N** (merges `026` §1.5 and `021` §5.3; see §5.3) |
| `PendingValue` | `{ incoming, outgoing, block }` | **T** (`025`) |
| `Completeness` | `enum { Complete, FromBirthday, Overdrawn }` | **N** (`021` §8.1, minus the rejected `transactions_only`) |
| `Page` | `{ after: Option<HistoryCursor>, limit: usize }`, with `first` / `after` constructors | **N** |
| `TransactionPage` | `{ checkpoint, transactions, pending, next }` | **T** (`025`) |
| `Watch` | `{ id: String, address: Address, asset: AssetId, start: BlockPosition, observation: Observation }` | **N** (`029` §5.2) |
| `Observation` | `enum { Complete { start }, Backfilling { start, done_through, anchor }, Unavailable { start, earliest }, Incomplete { since } }` — on `Watch`, not on a wallet | **N** (`024` §6, relocated by `029`) |
| `BlockCommit` | `{ network, block, transactions, outputs, unknown_spends }` | **N** (`029` §6.2), replacing `BlockObservation` |
| `SyncStatus` | `{ network, checkpoint, pending_tip, observed_tip, phase, unknown_spends, backfill }` | **C** — moves to root, gains `backfill` |
| `SyncHealth` / `NetworkHealth` / `NetworkState` / `NetworkFailure` | `019`'s vocabulary | **T** |
| `Draft` / `Payment` | `{ source, transfers, asset, snapshot }` | **T** (`026` c3) |
| `SignedTransaction::fee()` | `const fn fee(&self) -> &Decimal` | **T** (`026` c3) |
| `SignedTransaction::claim()` | `const fn claim(&self) -> &Claim` where `enum Claim { Outputs(Vec<(Id, u32)>), Nonce(u64), Lifetime(BlockPosition) }` | **N** (`026` c10) |
| `PurgedRows` | `{ checkpoint, history, movements, outputs }` | **T** (`019` §5.3) |
| `wallets::{Balance, History, HistoryEntry, HistoryMovement, HistoryFee, HistoryStatus, HistoryRequest, WalletInfo, Addresser}` | — | **D** |

---

## 4. What already landed

Everything in this table is in the working tree on `refactor/chain-unification`.

| # | What | Where | Doc |
|---|---|---|---|
| 1 | **Confirmed vs pending.** The store holds only blocks at or below each chain's irreversible line; everything above lives in memory on the `Service`. No journal, no rollback, no `BlockStore::remove`, no `ReorgTooDeep`, no `reorg_retention`. `SyncPolicy` collapses to one field | `sdk/indexing/src/source.rs:20-36` (`ChainTip`), `sdk/indexing/src/overlay.rs` (`PendingValue`), `sdk/indexing/src/policy.rs`, `error.rs:49` (`ConfirmedReorg`) | `025` |
| 2 | **Bitcoin's depth is configuration**, default 6, range 1..=1000, rejected as an unknown field on the other two chains, documented with its trade-off | `apps/server/src/chains/bitcoin.rs:34-51,93`; `apps/server/src/chains/solana.rs:180`; `docs/API.md:99` | `025` + owner |
| 3 | **The 409 on history paging is gone.** An append-only confirmed store makes ascending keyset pagination stable forever | `025` §6.3 | `025` |
| 4 | **Chain failure isolation.** One supervised task per `SyncUnit`; per-chain cadence, jittered backoff, quarantine-alive, panic respawn parked; `SyncMonitor`/`SyncControl`; `NetworkState`/`NetworkFailure`/`NetworkHealth`/`SyncHealth`; the readiness rule; `state`+`failure` on `GET /v1/status` | `sdk/indexing/src/health.rs`, `sdk/indexing/src/supervisor{.rs,/unit.rs,/schedule.rs,/control.rs,/monitor.rs}`, `apps/server/src/readiness.rs` | `019` §2-4 |
| 5 | **The offline rescan.** `BlockStore::purge_network -> PurgedRows`, atomic per backend, `payment-server <config> --rescan <chain>/<network>`, refusing a bad argument before storage opens; `testing::PurgeContract` | `sdk/indexing/src/storage/block.rs:140`, `storage/redb/blocks.rs`, `apps/server/src/rescan.rs`, `sdk/indexing/src/testing/purge.rs` | `019` §5.3 |
| 6 | **An unknown spend is reported, not fatal.** `BlockOutcome::Applied { unknown_spends }` replaces a terminal `"block spends an unknown indexed output"`; `SpendContract` pins it | `sdk/indexing/src/storage/block.rs:180`, `storage/redb/blocks.rs:129`, `tests/service_contract.rs:1482,1531` | `024` c1 |
| 7 | **One `indexing` crate.** PostgreSQL deleted (crate, migrations, 2,631-line contract, Docker harness, CI pull); `packages/storage` + `packages/storage/redb` + `indexing-redb` + `indexing-supervisor` folded in; redb and supervisor behind default features; the stored format proven by a fixture test; 016-20's `#[cfg(test)]` fault injection replaced by a `Probe` seam | `sdk/indexing/src/{storage,engine}/**`, `sdk/indexing/tests/stored_format.rs`, `storage/redb/backend/probe.rs` | `027` |
| 8 | **A second store, in memory**, running the four shared contracts beside redb | `sdk/indexing/src/testing/` | `027` §4.3 |
| 9 | **A wallet you can hold.** `Wallets::wallet(id)`; `generate`/`adopt` return `Arc<dyn Wallet>`; `TransactionFactory` is a `Wallet` supertrait — the builder is reachable without naming a chain | `sdk/wallets/src/wallet.rs:104-107`, `wallets/persistence.rs:27,48` | `026` c2 |
| 10 | **A builder you can inspect.** `snapshot()` → `draft() -> Draft`; `SignedTransaction::fee()`; `restore` → `resume`; three chain builders updated | `sdk/base/src/transaction.rs:241,264-284,365` | `026` c3 |
| 11 | **One secret parse, in crypto.** `SecretParser` and `Wallets::parse_secret` deleted; the length check moved *down* into `crypto::SecretKey::new` (`k256` left-pads a short slice, so 31 bytes had parsed as a valid key); `Wallets::secret(tag)` added | `sdk/wallets/src/factory.rs:36`, `wallets/registration.rs:94`, `packages/crypto` | `026` c4 |
| 12 | **The submission registrar moved to `base`** — zero wallet content, zero callers inside `wallets` | `sdk/base/src/submission.rs`, re-exported `sdk/wallets/src/lib.rs:11-13` | `026` c1 |
| 13 | **Ethereum traces are required.** `debug_traceBlockByHash` with `prestateTracer` in `diffMode`, one call per non-empty block; residual movements with the fee added back on the credit side; identity checked against the block. **No opt-out**: `validate_tracing` traces the node's own head at startup and fails terminally, naming the method, the tracer and the fix | `sdk/chains/ethereum/src/native/block/trace.rs`, `apps/server/src/chains/ethereum.rs` | `021` §6, §6.5 |
| 14 | **SPL tokens.** `preTokenBalances`/`postTokenBalances` decoded; `Transfer`/`TransferChecked` decoded to recover *both* owners; the balance diff stays the authority and anything undecoded falls out as a one-sided residual; decimals verified against the mint at startup; Token-2022 refused with `Unsupported` | `sdk/chains/solana/src/native/block/`, `native/rpc/accounts.rs` | `021` §4.2.1 |
| 15 | **Three halt fixes.** An unknown spend reports instead of failing (6); a traceless Ethereum quarantines Ethereum alone instead of stopping the process (4+13); a reorg above the line is `Vec::truncate` instead of an undo that could exhaust its journal (1) | — | `024`/`019`/`025` |
| 16 | **The crypto move.** Curves, keys, signatures and `SecretBytes` in `packages/crypto`, re-exported from `base`; `packages/decimal` extracted with no arithmetic | `packages/crypto`, `packages/decimal`, `sdk/base/src/lib.rs:19-25` | `017` |
| 17 | **The transport rewrite.** One URL per node describing it completely (credentials, headers, timeout, size bounds), failover as a list, retry as a property of the list | `packages/json-rpc`, `apps/server/src/chains.rs:176-276` | `023` |
| 18 | **Examples by scenario.** One directory per scenario, every chain inside it as setup; `examples/tests/scenarios.rs` asserts a chain crate is named only in that scenario's own setup | `examples/{wallet,transfer,indexing,watch_only,all_chains}/main.rs` | `017` |
| 19 | **Docs.** `docs/CHAIN_LAYOUT.md`, `docs/INDEXING.md`, `docs/API.md`, `ARCHITECTURE.md`, `AGENTS.md`, per-crate READMEs, ADR policy; 11 stale design documents deleted | `docs/` | `017` |

---

## 5. What remains — one ordered plan

Steps are ordered by dependency, not by appetite. Each leaves `cargo fmt --check`,
`cargo clippy --all-targets --all-features -D warnings`, `cargo test --workspace`,
`sh scripts/check-chains.sh` and `linter check .` green, and the linter baseline
at zero findings.

### 5.1 The sequence

| # | Step | Effort | Needs | Source | What it makes possible |
|---|---|---|---|---|---|
| **1** | **One listener list instead of one observer slot.** `BlockCommit`, `Listener`, `Indexer::listen(&self)`, `Composer::listen`; delete `Observer`, `BlockObservation`, `Service::observe`, the macro's `observe`, three `IndexerParams::observer` fields; Solana's `Progress` registers itself instead of silently displacing a caller's hook | **M** | — | `029` A | §2.5 case (b). Independent of everything, deletes a live hazard, breaks nothing outside `sdk/` — **land it first** |
| **2** | ~~**`packages/config` — the shell.** `Rpc`, `Retry`, `Sync` with public fields and `Rpc::endpoint`, moved from `apps/server/src/chains.rs:176-276` (93 lines); `Document::read` behind feature `toml`; a `cargo tree` guard in CI~~ **LANDED** | **M** | — | `028` s1 | a chain crate can name its own RPC settings; nothing breaks (additive) |
| **3** | **The index owns its watch list.** `Holder`, `Holding`, `Start`, `Watch`, `Observation`, `Watching`; `Service` gains the hot watch map and its `NetworkAdmission`; `Registry` loses `material`, its `asset: String` becomes `AssetId`, and it gains `advance`; `Indexer::sync` and `Supervisor::new` lose their `FilterSource`; delete `FilterSource`, `selection.rs`, `Wallets::{watch, filters}`, the `admissions` map, `Custody::WatchOnly`. **Ships with `--export-keys`** (§5.3 f) | **L** | 1 | `026` c6 + `024` c4 + `029` B | **the whole diagnosis §0.1 cause 1.** `indexing → wallets` disappears; `index.watch(&*wallet, Start)` becomes writable |
| **4** | **The map stops holding wallets.** Delete `values`, `Entry`, `Custody`, `WalletInfo`, `store`, `forget`, `restore`, `import`, `insert`, `get`, `list`, `wallet`, `balance`, `history`, `send`, `parse_address`, `wallets/persistence.rs` (195 lines) and the 36-line dispatcher `wallets/history.rs` (the 400-line model `history.rs` goes in step 7); `Wallets<I, A>` → `Wallets<A>`; `register`→`add`, `generate`→`create`, `adopt`→`from`, `insert`→`accepts`; `send_all(asset, Vec<Transfer>)` | **L** | 3 | `026` c7 + `029` C | the owner's first position, in full. `sdk/wallets` says one thing |
| **5** | **The application keys its own wallets.** `Signers` beside `Payments`; `subject(id)` unions it with `index.watched`; `apps/server` rebuilds it at startup from its key store; `ConfiguredWallet::import` splits into `from` + `keep` + `watch`, in that order | **M** | 4 | `029` D | **must ship with 4** — between them `apps/api` does not compile. The HTTP wire is unchanged, which is the check that the split preserves behaviour |
| **6** | ~~**Balance comes from the index.**~~ **LANDED.** `sdk/indexing/src/balance.rs` (`Balance`, `Completeness`, `Effect`, `FeeSettlement`, `Ledger`, `Balances`), `storage/balance.rs` (`BalanceQuery`, `BalanceStore`), the redb prefix fold, the in-memory fold and `testing::BalanceContract` over both; the fee rule is `BlockInterpreter::settlement`, stated by each chain with no default; `BalanceReader`, `wallets::Balance`, `ErrorKind::Balance`, the three chain impls and `History::unconfirmed` are deleted | **M** | 3 | `026` c8 + `021` W1–W4 | §2.6. Watch-only balance goes 422 → 200; `observed_height` is non-null on all three chains; the display/atomic unit mismatch is structurally impossible |
| **7** | ~~**History is projected at the edge.**~~ **LANDED.** `History::history(&dyn Holder, Page)` with `Page` in the root; `sdk/wallets/src/history.rs` deleted (763 lines, ~404 of them production), its surviving display metadata moved to `sdk/wallets/src/asset.rs`; `apps/api/src/api/history.rs` converts `indexing::TransactionPage` directly | **M** | 3 | `026` c9 | §2.7. Three models of a transaction become two. Wire unchanged; every history assertion in `route_contract.rs` is untouched |
| **8** | **Root, names, and the layering rule.** Move `SyncStatus`/`SyncPhase` to root; `//!` headers on every `sdk/indexing/src/*.rs`; the `ErrorKind` naming decision with `017`'s `U7` (private `kind` + accessor) taken together; drop the `BlockInterpreter as Interpret` aliases; name `delegate_indexer!` in the README's first screen; decide `NetworkRules` (**G3**) | **M** | 3 | `017` 016-16/016-29/`U7`, §0.1 cause 4 | the root stops naming `engine/`; a reader can grep for what they expect |
| **9** | ~~**A chain composes itself** ×3. `sdk/chains/<chain>/src/config.rs` with public fields, `new`, `validate`, `connect`, `compose`; move the two value checks off `deserialize_with`; delete `apps/server/src/chains/<chain>.rs`~~ **LANDED** — `Config<A>`/`Node<A>`/`ComposedChain<A>` in each chain, `apps/server/src/chains/` gone, the zero-address and System-program checks now on `validate`; `packages/config` gains its first consumers | **L** | 2, 4 | `028` s3a-c | §2.1. A library embedder can start a chain from a value |
| **10** | ~~**One composition.**~~ **LANDED** — `sdk/wallets/src/composition.rs`: `ComposedChain` (one declaration replacing the three from step 9), `Connection` (implemented by each chain for its own `Node`), `Repository` (every store port as one bound, so a port added to the index is added once rather than in three chain signatures), `Composition::{build, verify_then_build}`; `apps/server/src/startup.rs` deleted and its four ordering tests moved with it; `chains.rs` loses its `Connection` trait, its `Parts` collection and its three `impl Connection for chain_<name>::Node`. **No `Deps`**: `registrar` is the only dependency composition takes, so a one-field aggregate would be ceremony. `examples/all_chains` now composes through `Config::connect` + `Composition::build` | **M** | 9, 4 | `028` s4 | §2.2. The composition root ships with the library instead of living in a binary |
| **11** | ~~**One TOML document, and both ways in.**~~ **LANDED** — `packages/config` moved to **`sdk/config`** as crate `sdk-config`, a registration-driven reader: `Registry::register::<C>(name)` + `Chain` (the trait each chain implements for its own `Config`) + `Sections` + `Chains`, and `Document::read` (TOML for a `.toml` path, JSON otherwise) gains its first caller at the server's config seam. `chains::Config`'s three `Option` fields, its `validate`, `networks`, `connect` and `announce` arms are gone; `apps/server/src/chains.rs` is 33 lines of which three are `register`. Freed by the layer move: `Sync` → **`Cadence`** (it shadowed `std::marker::Sync`), `poll_millis: u64` → `poll: Duration`, `batch_size: usize` → `policy: indexing::SyncPolicy` — the re-inflation at three chain ends deleted; `config::Retry` → **`Repetition`**, so the value and `json_rpc::Retry` stop sharing one word; the section shape unified (`cadence` nested on every chain, `sync` and the flattened form gone). `examples/` keeps building in code and gained no `config.toml`: the twelve `Setup::{bitcoin,ethereum,solana}` functions became twelve one-expression `fn <chain>() -> Config` | **M** | 10 | `028` s2,5,6 | **breaks every deployment's config file** (`docs/API.md` §Configuration lists the four edits) — the only operator-visible step in the plan |
| **12** | ~~**Backfill.**~~ **LANDED.** `BackfillStore` (`progress` + `backfill`) with `BackfillProgress`/`BackfillBatch`/`BackfilledBlock`/`BackfillOutcome`; the batched backfill phase inside the ordinary pass (`sdk/indexing/src/backfill.rs`); durable progress written **in the chunk's own transaction** rather than through `Registry::advance`; `Completeness::while_backfilling` gating the balance read *and* the history page (`index.rs:94-96,123-125`); cost controls `SyncPolicy::max_backfill_blocks` (synchronous `InvalidRequest` at `watch`, naming the earliest acceptable start), the per-pass budget left by the forward phase, and a per-address anchor fixed at registration. `PageAnchor`/`backfill_epoch` **not built** (G5: the history cursor no longer carries a checkpoint, so there is nothing to invalidate); `BlockSource::earliest` and `SyncControl::cancel_backfill` **not built**. Three more of `024`'s pieces went unbuilt that this row used to omit: §6's `Observation`, §3.6's recent-block cache and §2.3's wire `start` — **§5.5 records all of them item by item**, each as built, abandoned with its reason, or open, together with the two carve-outs §5.3 (b) and (e) already made | **L** | 3 | `024` WI-1,2,4,8 | `Start::At(p)` below the checkpoint is honoured instead of silently indexing nothing — **the last silent-data-loss path in the SDK is closed** |
| **13** | ~~**The remaining balance correctness work.**~~ **LANDED**, minus one deferral. Solana pre/post residual replaces the terminal guard (`native/block/movement.rs`: `residuals`/`token_residuals`, `reconcile` is an assertion). `balanceOf`/`eth_getBalance` pinned to a block hash as a **witness** that never changes the number — `native::rpc::{Witness, Attestation}` + `BalanceWitness` from `WalletFactory::witness`; agreement at the fold's own block is the one thing that constructs `Completeness::Complete`. Ethereum consensus-layer withdrawals are now whole: `InterpretedBlock::credits` carries `indexing::BlockCredit`, `BlockAddition`/`BackfillBatch` validate it, `Ledger::apply_credit` folds it, SQLite keeps it in a `block_credit` table of its own, and ethereum's `interpreter.rs` fills the field (section 5.3 (g), `021` section 4.4.1). **Deferred:** the ERC-20 `eth_getLogs` backfill — design settled in `021` section 7.6, and **not** a correctness gap, because the block backfill of step 12 already recovers ERC-20 history; it is blocked on `BackfillProgress` keying `(address, asset)` rather than `address` | **M**, **M**, **S**, **M** | 6; 12 for the last | `021` W6,W7,W9,W10 | a halt class disappeared (Solana CPI lamport moves); `completeness` can be proven per holding on ethereum; a withdrawal-funded address no longer drifts |
| **14** | **`Claim` on a signed transaction.** `enum Claim { Outputs, Nonce, Lifetime }` and the accessor | **S** | — | `026` c10 | "will this collide with my other in-flight send" |
| **15** | **Housekeeping.** `packages/json-rpc`'s 775-of-809 inline test module moves to `tests/` (`017` 016-19) — **landed**; one `json-rpc` test double behind a `testing` feature replaces the scripted-RPC doubles (`017` `P5`, ≈300 lines) — **landed**; Solana's factory takes a narrowed three-method index port instead of `&Arc<Indexer>` (`017` `U5`/`U6`) — **refused on evidence, not open.** `solana::Indexer::progress` (`sdk/chains/solana/src/indexer.rs:85`) is `pub(crate)` and returns a `watch::Receiver<Option<BlockRef>>` — a notification channel, not a query — so no `indexing` port can supply it. The handle is also what lets `sdk/chains/solana/src/wallet.rs:120` assert that the indexer and the config name the same network, which a narrowed port would have to carry too, and splitting it would admit a receiver from one indexer beside a checkpoint from another. The narrowing that *was* available already happened elsewhere: `solana/src/sender/recovery.rs` holds `Arc<dyn Checkpoint>` and `Arc<dyn History>` separately, which is why the four-trait split of `Indexer` survived review; the two unasserted reservation tests from `025` §5 — **landed** | **S** each | — | `017`, `025` | nothing user-visible; all independent and safe |

**Steps 3–7 are one release.** They are the structural change, they touch the
same callers, and splitting them means moving `apps/api` four times. Within it,
**4 and 5 must be one commit pair**: 4 removes `wallets.get`/`list` and 5 is
what replaces them. Step 11 was the only one an operator notices and shipped
alone.

### 5.2 Dependencies, drawn

```
1 (listener) ──► 3 ──┬── 4 ── 5 ──┬── 9 ── 10 ── 11   (config path)
2 (config) ──────────┘            │
                     ├── 6 ───────┴───────── 13       (balance path)
                     ├── 7                             (history)
                     ├── 8                             (names / layering)
                     └── 12 ─────────────── 13(W10)    (backfill)
14, 15 — independent of everything
```

**Step 3 is the keystone.** Nine of the remaining fourteen steps sit behind it,
and it is the one that removes the structural cause rather than working around
it. Step 1 goes first anyway because it is independent, it is a deletion, and it
closes a hazard that is live today: Solana's own submission recovery installs an
`Observer` (`sdk/chains/solana/src/indexer.rs:50-51`) which a caller's hook would
silently displace.

### 5.3 The known collisions, resolved

**(a) `026` and `021` both specify a `balance.rs`.** They are not the same file.

| | `021` §5.3 | `026` §1.5 | Resolution |
|---|---|---|---|
| Type | `BalanceAt { amount, as_of, checkpoint, confirmations, completeness }` | `Balance { amount, observed_at, pending }` | **`026`'s shape wins.** It is the one the shipped wire already has (`025`, `apps/api/src/api/wallet.rs:208-224`), and `as_of` + `checkpoint` are one fact now that the checkpoint *is* the confirmed line |
| The `confirmations: u64` input | a query parameter | **rejected** — "after `025`, `Confirmed` is a fact, not a threshold" | **rejected in the SDK**, kept at the HTTP edge as `?confirmations=X`, which re-folds |
| `completeness` | a field | absent | **kept**, as a fourth field on `026`'s `Balance` |
| Trait | `BalanceStore` (a store port) | `Balances` (a caller-facing read) | **both, at different layers.** `Balances` is root (`balance.rs`); `BalanceStore` is `storage/balance.rs`. One is what a caller calls; the other is what a store implements |

Final: `sdk/indexing/src/balance.rs` holds `Balance`, `Completeness`, `Balances`
and the pure `effect()` fold; `sdk/indexing/src/storage/balance.rs` holds
`BalanceStore` and `BalanceQuery`. `021`'s name `BalanceAt` is dropped.
`029` §8.3 asked for exactly this to be written down before either document
starts. This is it.

**(b) `024`'s `Start` renames the same functions `026` renames.** `024` step 4
adds a `start: Start` parameter to `generate`, `adopt`, `watch`, `import` and
`insert`; `026` step 7 renames four of those five and deletes the fifth.

**Resolution: `024` step 4 does not happen as written.** Under `026` §1.4 a
start appears in exactly one place — `index.watch` — so the argument **moves**
rather than being added to `wallets` and removed a commit later:

| Call | Takes a `Start`? | Why |
|---|---|---|
| `wallets.create(tag)` | **no** | it does not watch, and it does not know the caller's id |
| `wallets.from(tag, secret)` | **no** | same |
| `wallets.accepts(tag, wallet)` | **no** | it is a validation, not a registration |
| `index.watch(id, subject, start)` | **required** | the only place the question is real |

The variant spelling is `026`'s: **`Fresh` / `At`**, not `024`'s `Tip` / `At`.
`Tip` names a *policy* — "start from now" — which is exactly what someone
writes when they do not know; `Fresh` names a *fact*, so "I don't know" is
inexpressible by accident. Everything else of `024` §2.2 survives, relocated:
`&self` everywhere, one add path, every add taking a publication permit — all of
it now inside `Watching::watch`, which is where `wallets/persistence.rs`'s 195
lines of permit, registry write and rollback end up.

`024`'s `Observation` also moves, twice over: `026` put it on `WalletInfo`, and
`029` deletes `WalletInfo`. It lands on `indexing::Watch`, returned by
`Watching::watch` and listed by `Watching::watched` — which is right, because the
index owns the progress. It does **not** gain `026`'s `Unwatched` variant:
without an id→wallet map inside `wallets` there is no object that could be
unwatched-but-known. "Not watched" is now simply "not in `index.watched`", which
`apps/api`'s `subject(id)` already computes.

**(c) `028`'s composition types want `sdk/wallets` while `026` and `029` shrink
it.** All three are right, and the arithmetic still works — more easily than
`026` projected, because `029` deletes more:

```
sdk/wallets production today                                     1,962
  − history serving                             (step 7)          −436
  − balance                                     (step 6)           −25
  − FilterSource + watch + admissions + filters (step 3)          −267
  − persistence.rs, values, Entry, Custody,
    WalletInfo, get/list/wallet/send/restore    (step 4)          −290
  + secret()/accepts()/Tag                      (steps 3, 4)       +25
  + composition.rs                              (step 10)         +194
                                                                 ------
                                                                 ≈1,060   (12 files → 8)
```

`028`'s addition is to the part every document **keeps** — the factory map — and
not to the parts they delete. `sdk/wallets` is the only legal home:
`Composition::build` must name `indexing::Composer`, `indexing::Registry`,
`wallets::Wallets` and `wallets::WalletFactory` at once, and `linter.toml:363-365`
gives `wallets` exactly `["packages", "base", "indexing"]` while
`packages/config` could name nothing in `sdk/` at all (`:344-346`) — which is
exactly why step 11 moved it to `sdk/config`, where `Cadence` can hold the
`indexing::SyncPolicy` it always meant. `029` also
makes `Composition` simpler: `Wallets<A>` has no index handle and no `I`.
**`028` step 4 was blocked on `026` commit 7; retarget it to step 4 here.**

**(d) `019` §6.3 versus `024`.** `019`'s last unimplemented item — refuse a
birthday below the checkpoint — is **not built and will not be**, because step
12 makes the birthday honoured instead of refused. Refusal survives only for a
range the source cannot serve (`BlockSource::earliest`, `max_backfill_blocks`),
which is step 12's cost-control half.

**(e) `021`'s PostgreSQL work no longer exists.** `027` deleted the backend, so
`0003_block_credit.sql`, the `payments_movement` index, `validate_schema`'s
frozen literals and the whole `revert.rs`/journal half of `021` §8.3 and `024`
§9.2 are void. Step 6's storage work is redb plus the in-memory store; step 12's
is a field on two bincode records.

**(f) Step 3 removes a column that holds every deployed key.**
`RegisteredAddress.material` (`sdk/indexing/src/storage/registry.rs:29`) is where
every wallet `apps/server` has ever generated or adopted keeps its key. Removing
it without a migration turns every wallet in a deployed store watch-only. Step 3
therefore **ships with an offline `payment-server <config> --export-keys <path>`**
in the shape `--rescan` already has: read-only against the index, the last code
permitted to name `material`, and deleted one release later.

**(g) Step 13's withdrawals need a record `indexing` does not have.**
`InterpretedBlock` is `{ block, transactions: Vec<ObservationDraft>, outputs:
OutputChanges }`, and the only thing a balance folds is a
`CanonicalTransaction` (`Ledger::apply`). A consensus-layer withdrawal is
neither: it credits an execution address from the block body with no
transaction, no receipt and no trace entry, and `021` section 4.4 rejected the
cheap way out — a synthetic `TransactionRef` — because `TransactionRef` means
"the chain's transaction identifier" and a reserved string prefix every reader
must know is worse than a field.

So the ethereum half of step 13 splits, and only the chain half is a chain
change:

| Half | Where | State |
|---|---|---|
| Read the credits out of the payload already fetched | `sdk/chains/ethereum/src/native/block/withdrawal.rs`, `RawBlock::withdrawals` | **landed**: `Withdrawal { index, validator_index, address, amount }`, Gwei converted to wei at the parse, indices required strictly increasing so `withdrawal:<index>` is unique, zero-amount entries dropped, zero new RPC calls |
| Carry them to the fold | `sdk/indexing` | **landed**, at the shape proposed and a little wider: `InterpretedBlock::credits: Vec<BlockCredit>`; `BlockAddition::new` **and** `BackfillBatch::new` validate them (so an address watched from its own past gets its credits too, which the proposal did not name and which would otherwise have been a fresh silent gap); `Ledger::apply_credit`; SQLite's `block_credit` table and the in-memory map; `PendingValue::fold` counts one above the confirmed line; `PurgedRows::credits` reports what a rescan removed; `testing::{BalanceContract, BackfillContract, PurgeContract}` each gained a scenario. Ethereum's `interpreter.rs` fills the field, and the pinning test now asserts two ether beside the 42 wei the block's transaction moved to the same address |

Two notes for whoever writes the indexing half. `BlockCredit` is credit-only on
purpose (`021` section 4.4): no chain here debits an address without a
transaction, and a signed block-level record would be speculative. And
historical rows do not gain credits retroactively — a deployment that upgrades
in place is correct only from the upgrade height forward unless it runs
`--rescan`, which is exactly what `019` section 5.3 already built.

Both held. A third, learned while writing it: a credit has no `TransactionRef`,
so it reaches a *balance* and never a *history page* — a withdrawal-funded
address can have a non-zero balance and an empty page. That is the honest
rendering of a transactionless credit rather than a defect, but it is the one
user-visible consequence of choosing (b) over (a), and `021` section 8.2's
`cause: "transaction" | "block"` is where it would be closed.

Disagreement has no home in `Completeness` either, and deliberately so: the
witness reports an `Attestation` beside the balance (`021` section 8.1's
`attested`), rather than a fourth variant. A fold the node contradicts is not a
*kind* of completeness, it is a fact about one reading at one block.

### 5.4 What the plan costs, and what it buys

| | production lines |
|---|---|
| Steps 3, 4, 7 — deleting the two-owner machinery, the instance store and the duplicate history model | **−990** |
| Step 1 — one listener list replaces the observer slot | **−20** |
| Step 5 — `Signers` and the handler rewiring in `apps/api` | **+90** |
| Step 6 — balance moves and the three chain readers go | **−40** |
| Steps 9, 10, 11 — `apps/server/src/chains.rs` (532) and `startup.rs` (100) move into the SDK and shrink | **−250** |
| Step 8 — headers, names, one `ErrorKind` | ≈ 0 |
| **structural subtotal (steps 1, 3–11)** | **≈ −1,200** |
| Step 12 — backfill, a genuinely new capability | **+500** |
| Step 13 — withdrawals, the Solana residual, the witness, the ERC-20 backfill | **+400** |
| Steps 2, 14, 15 | **+200** |
| **net** | **≈ −100** |

The structural half is where the diagnosis is tested, and it pays: **≈ −1,200
production lines while adding index-owned observation, a reachable storage hook,
a balance object that cannot carry the wrong unit, and chains constructible from
a value.** The additions after it are new behaviour, not new plumbing — a
backfill that today silently does nothing, and four correctness gaps that today
either drift or halt a chain.

### 5.5 What step 12 left unbuilt

Step 12's row points here for two omissions. It understated: **five** of `024`'s
pieces went unbuilt, not two — six counting the `PageAnchor` the row does name —
and §5.3 had already carved out two more that the row cites without restating.
So two deliberate abandonments were recorded nowhere. Each is recorded below as
**built**, **abandoned** — deliberately not built, with a reason a later reader
can evaluate and overturn — or **open**, meaning nothing has been decided
against it and the gap is still a gap.

> **Correction (the two open items are now built).** (a) and (e) below are
> closed; the table's verdicts and the paragraphs are rewritten in place, with
> the superseded reasoning kept where it explains the decision. `024` now owns
> exactly one outstanding thing, (b)'s cancellation, and it is blocked on G10
> rather than on `024`.
>
> **Second correction (b) is built too, and so is G10.** `024` has nothing open.
> `unwatch` landed first because cancellation needed the one primitive it
> brought — a registry that can forget a row — and (b)'s paragraph below is
> rewritten with what was actually built and what it gave up.

| `024` | What | Verdict |
|---|---|---|
| §7 | the availability floor | **built** — by *reporting* the owed range, not by predicting it; `BlockSource::earliest` was not needed and was not built |
| §7 | `SyncControl::cancel_backfill` | **built**, and not in the shape `024` gives it: `Backfilling::cancel(&network)`, because it is a durable write |
| §6 | the `Observation` enum, on the wallet, the balance and the history page | **abandoned** — `Completeness` answers it per read, and per read is the right grain; it is now on the wire as `completeness`, on the balance and the history page |
| §3.6 | the recent-block cache, `SyncPolicy::recent_bytes`/`recent_blocks` | **abandoned** — the owner's requirement behind it is met by a different object |
| §2.3 | `start` on the three add bodies | **built** — required on all three, plus `wallets[].start` in the configuration |
| §5.3 (b) | `Start` on five `Wallets` signatures | **abandoned** in §5.3 (b); the tree confirms the resolution |
| §5.3 (e) | everything journal-shaped, and `0003_backfill.sql` | **void** — no journal, no PostgreSQL |
| §5.3, G5 | `PageAnchor` / `backfill_epoch` | **abandoned**, as step 12's row already records |

**(a) The availability floor — built, by reporting the range rather than
predicting it. `BlockSource::earliest` was not needed.**

The question `024` §7 asks is whether an unservable historical range should
stop the API serving. It should not, and the reasoning that closed it is short:
a pruned range is a property of one watch's *request*, not of the chain;
forward indexing is what every client reads and is healthy; and the thing the
gap actually weakens is each read that touches it, which `Completeness` already
states there. So `SyncPhase` follows forward progress alone
(`sdk/indexing/src/synchronizer.rs`'s `status`), `NetworkHealth` gained
`backfill: Option<BackfillStatus>` beside its `failure`
(`sdk/indexing/src/health.rs`) — `failure` now means *forward* indexing only —
and `GET /v1/status` publishes it as `networks[].backfill` with `pending`,
`resume`, `anchor` and the refusal that stopped it
(`apps/api/src/api/sync.rs`). Three states stay distinct, which was the
constraint: never reached its tip is `CatchingUp` and 503; reached its tip and
owes history is `Ready`, 200, with a non-null `backfill`; reached its tip and
owes nothing is `Ready` with `backfill: null`.

The port §7 proposed was **not** built, and the reason is worth recording:
predicting the floor and reporting the refusal answer the same operator
question, and the tree already catches the refusal (`backfill.rs:63-69`). An
`earliest` would let a *watch* be refused at the call instead of failing in the
background — but `SyncPolicy::max_backfill_blocks` already refuses a start too
far down, synchronously, and a start inside that bound on a node that has
pruned inside it is a node problem no port fixes. Reopen `earliest` only for
the narrower job of *narrowing a chunk* to what the node holds, with a
measurement that shows the refused fetches cost something.

The supervisor's bounded backoff stays exactly as it was: a backfill failure is
still counted as an attempt and still waits
(`sdk/indexing/src/supervisor/unit.rs`), so an unservable range costs a
trickle, not a spin. What changed is only that it no longer costs availability.

*The superseded analysis, kept because it is what made the case:*

`BlockSource` declares `blocks` and `canonical_at`
(`sdk/indexing/src/source.rs:50,59`) over `ChainTip`'s `tip` and `confirmed_tip`
(`:23,35`). There is no `earliest` and no port beside it; `024` §7's premise
that `BlockSource` was already at `linter.toml:140-142`'s three-method cap is
stale, so a separate port may no longer even be necessary.

What the tree does instead is catch the failure rather than predict it.
`Synchronizer::backfill` re-raises only a terminal `Store` error and turns
everything else into `BackfillStatus::failure`
(`sdk/indexing/src/backfill.rs:63-69`), so a range the node cannot serve leaves
the pass's forward progress intact. `Unit::stalled` then counts that failure as
an attempt and waits out the same capped, jittered backoff a failed pass gets
(`sdk/indexing/src/supervisor/unit.rs:158-217`); its own comment states the
case exactly — "asking for it again at once re-issues the same refused call for
as long as the range stays unreadable, which on a pruned node is forever".

That was the cost bounded, not the gap closed, and the residue was visible:

| Consequence (before this was closed) | Where |
|---|---|
| A pass that still owes a backfill reports `CatchingUp` however close the checkpoint is to the tip | `sdk/indexing/src/sync.rs:65-72` |
| `ready()` needs every network `Ready` or `Quarantined`, and a backfill failure is deliberately **not** a state change | `sdk/indexing/src/health.rs:126-134`, `unit.rs:209-215` |
| So one watch whose range the node will never serve holds `/health/ready` at 503 for the whole process, indefinitely, with nothing but `failure` on a `CatchingUp` row to say why | — |
| A *configured* wallet is imported before the supervisor starts, so the same watch stops `settled()` ever becoming true and the listener never binds at all | `apps/server/src/main.rs:103-107,126`, `health.rs:140-142` |

Two of the three chains can already answer the floor question: Solana has
`first_available_block` (`sdk/chains/solana/src/native/rpc/blocks.rs:79-81`),
and Bitcoin refuses a pruned node at connect
(`sdk/chains/bitcoin/src/native/rpc/node.rs:67-72`), so its floor is genesis or
there is no source. Only Ethereum's floor is unresearched. That work — one
method, three implementations, a narrowing rule in `Chunk` — is what is left
over, and it is now an optimisation rather than a defect: the reporting
decision above is what the reachable consequence needed, and (c)'s abandoned
`Observation::Unavailable` is replaced by `networks[].backfill` plus each
read's own `completeness`.

**(b) `SyncControl::cancel_backfill` — built as `Backfilling::cancel`, and
`024`'s shape for it never fitted what `SyncControl` became.**

*What was built.* A port of its own, `Backfilling { fn cancel(&self, network) ->
Vec<Watch> }` (`sdk/indexing/src/backfill.rs`), a supertrait of `Indexer` beside
`Watching`, implemented by `Service` over the two halves `Index` already holds —
the watch set that knows which addresses owe a range, and the store that records
that they no longer do. Reached as `POST /v1/status/backfill/cancel`, beside the
resume, because "the SDK method exists" and "an operator can reach it" are
different claims and this section has been wrong about the difference once.

*The semantics, and why.* Every watch that still owes a range is left as **the
watch it would have been had it been created now from `Start::Fresh`**: same id,
same address, same asset, birthday at the checkpoint's successor, coverage
settled. Nothing indexed is deleted — the rows a partial backfill wrote, and the
rows the address collected as somebody else's counterparty, stay and are still
served. The *claim* is what moves: a read weakened by an outstanding backfill
already says `FromBirthday`, and after a cancellation it says the same thing
about a birthday the index has actually read from, which is the only honest thing
to say once the range below will never be read. A bare `resume > anchor` write —
the shape this section first imagined — would have settled the coverage while
leaving the birthday below the hole, i.e. claimed `FromBirthday` for a range
nothing read. That is the reason the birthday moves, and it is why cancellation
needed `unwatch`: rewriting a registration is only expressible as forget plus
re-register, and `Registry` had no way to forget.

*What it gives up, stated.* The registration goes first and the coverage second,
because that order's crash outcome cannot lie: a crash between them leaves a
raised birthday over a coverage row that still owes the old range, so the next
pass reads it again and the operator repeats the call. The reverse order would
leave a settled coverage row under the *original* birthday. There is no durable
record that a range was cancelled rather than completed, by design: an operator
cancels to make the alarm stop, `GET /v1/status` then reports `backfill: null`,
and the `202` body is the record — exactly how `POST /v1/status/resume` already
works. Any future claim stronger than `FromBirthday` derived from *coverage*
would have to add that distinction first.

*Grain.* Per network, not per address, because the evidence an operator has is a
network aggregate: `networks[].backfill` publishes one `resume`, one `anchor` and
one count and names no address, so a per-address cancel would be a request this
API gives nobody the information to make. A cancelled range is resumable, at the
price of reading it: `unwatch` the address and watch it from a lower `Start::At`,
or `--rescan`, which purges `backfill` and preserves `registration`.

*The superseded analysis, kept because it is what made the case:*

`SyncControl` is two `Arc`s over in-memory watch channels
(`sdk/indexing/src/supervisor/control.rs:15-21`), and `resume` does one thing:
bump a counter a parked unit is waiting on (`:44-56`). It holds no store handle
and writes nothing durable. Cancelling a backfill is the opposite kind of act —
outstanding work lives in the `backfill` table as `BackfillProgress { start,
resume, anchor }` (`sdk/indexing/src/storage/backfill.rs:27-42`), so "cancel"
means durably writing `resume` past `anchor` for every affected row. Putting
that behind a handle whose whole design is "a signal, not a write" would make
one type mean two things.

`SyncControl::resume` is nonetheless the precedent for the *other* half of this,
and worth stating because it was live until an hour ago: `resume` existed,
`Supervisor::control()` was dead code, and `apps/api` registered only the read
route, so a quarantined network had no recovery short of restarting the process.
It is now `POST /v1/status/resume` (`apps/api/src/api/sync.rs:223-244`,
`apps/api/src/api/mod.rs:519`, `apps/server/src/main.rs:27-35,119`). "The SDK
method exists" and "an operator can reach it" are different claims, and this
document has been wrong about the difference once.

What bounds a runaway backfill today is admission, not cancellation:
`SyncPolicy::max_backfill_blocks` defaults to 100 000 and is checked
synchronously under the publication permit, naming the earliest acceptable start
(`sdk/indexing/src/policy.rs:14,72-92`, applied at `watches.rs:313-318`). A
mistyped start is therefore refused at the call that made it, which is the case
`024` §7 actually cared about. What has no exit is a start that was *legal* and
is merely expensive: progress is durable, so a restart resumes it; `--rescan`
purges `backfill` while preserving `registration`
(`sdk/indexing/src/storage/sqlite/blocks.rs:34-46`) and so restarts the work
rather than ending it; and there is no `unwatch` (§6, G10). **The honest
statement is that cancellation is unreachable in three directions at once, and
the first one to fix is probably G10, not this.** Open, blocked on G10.
*That last sentence was right: G10 was the first one to fix, and fixing it made
the rest expressible.*

**(c) `024` §6's `Observation` — abandoned. `Completeness` is the answer, and it
is a property of a read, not of a wallet.**

There is no `Observation` anywhere in the tree. `Watch` carries four fields —
`id`, `address`, `asset`, `start` (`sdk/indexing/src/watch.rs:33-43`) — and the
wire `Wallet` carries five, none of them about coverage
(`apps/api/src/api/contract.rs:28-40`).

It is not an omission. `024` §6 put a four-variant state on the *wallet* because
in `024`'s tree the wallet was the thing that knew; step 3 moved the watch list
into the index and step 6 gave the answer a different home. `Completeness`
(`Complete` / `FromBirthday` / `Overdrawn`, `sdk/indexing/src/balance.rs:45-66`)
is returned **on the read**, by both reads, weakened through
`while_backfilling` when the address still owes a range
(`sdk/indexing/src/index.rs:94-96` for history, `:123-125` for balance). Each of
`024`'s four variants maps or dissolves:

| `024` §6 | Today |
|---|---|
| `Complete { start }` | `Completeness::FromBirthday` — and `024`'s own text already conceded this is `021` §8.1's `from_birthday`, not a proof |
| `Backfilling { start, done_through, anchor }` | not a variant: it is the *gate*, `while_backfilling`, applied to whichever claim the fold made |
| `Incomplete { since }` | `SyncStatus::unknown_spends` per pass (`sdk/indexing/src/sync.rs:64`), and `Overdrawn` where the fold proves it |
| `Unavailable { start, earliest }` | nothing — it needs (a) |

The reason to prefer a per-read claim over a per-wallet state is that
`Overdrawn` is a fact about a fold and `FromBirthday` is a claim about a
*start*, and a wallet-level enum would have to pick one grain for both. A read
already carries its own checkpoint; coverage belongs beside it.

**This settled a question a lane was holding open, and that question is now
closed too.** `indexing::TransactionPage` gained `completeness`
(`sdk/indexing/src/history.rs`) and was deliberately kept off the wire, on the
reasoning that `024` §6's `observation` object was the intended shape for
*both* reads. That reasoning was void — `Observation` is not the plan — and the
ordinary answer has since been built: a `Completeness` schema of exactly
`complete` / `from_birthday` / `overdrawn`, required on the wire `Balance` and
on the wire `TransactionPage`, in the same three words
(`apps/api/src/api/contract.rs`). Note what it does **not** claim: `Complete`
is produced only by `chain_ethereum::BalanceWitness`, when an independent node
read pinned to the fold's own block agrees, so Bitcoin and Solana never report
it, and the schema says so rather than implying the absence is an error.

**(d) `024` §3.6's recent-block cache — abandoned. The requirement behind it is
met; the artifact `024` specified is not what met it.**

`SyncPolicy` has two fields, `batch_size` and `max_backfill_blocks`
(`sdk/indexing/src/policy.rs:28-45`). There is no `recent_bytes`, no
`recent_blocks`, and no byte budget anywhere.

A native-block cache does exist — `NativeBlocks<B>` in
`sdk/indexing/src/fetch.rs:31,40-42`, a 16-block window keyed by the whole
`BlockRef`. It is a different object doing a different job, and the distinction
matters because §3.6 could otherwise be marked "built" and its gap would stay
invisible:

| | `024` §3.6 | `NativeBlocks` |
|---|---|---|
| Serves | the **backfill** range `[lo, hi]`, below the checkpoint | `reinspect`, re-interpreting blocks held **above** the confirmed line after an address insert (`synchronizer.rs:298-306`) |
| Read by | the backfill phase | `Synchronizer::recover` only (`fetch.rs:100-106`), reached from `synchronizer.rs:303` |
| Semantics | a resident suffix, read repeatedly | a `take`: one hit and the entry is gone, pinned by its own test at `fetch.rs:168-181` |
| Bound | `recent_bytes` (64 MiB) then `recent_blocks` (`reorg_retention`) | `RETAINED_BLOCKS = 16`, unconfigurable |
| Keyed by | position, truncated on reorg | the whole `BlockRef`, so an unwound block can never be handed back for the hash that replaced it; it ages out unread |

The backfill phase fetches its range with `fetch_blocks` and consults no cache
at all (`sdk/indexing/src/backfill.rs:97-99`). So §3.6, as specified, is
unbuilt.

The owner's actual requirement — *"when we create a new wallet and add a new
address close to the tip, we are not going to reindex everything again"* — is
nevertheless met, by a route `024` could not have written because `025` had not
landed. A `Start::Fresh` watch gets the checkpoint's successor as its birthday
(`watches.rs:389-401`), which is the confirmed line, so the blocks between it
and the tip are exactly the overlay's; `reinspect` re-interprets them against
the new address set, and `NativeBlocks` is what makes that refetch-free.
**Nothing is reindexed and nothing below the checkpoint is fetched.**

What is genuinely given up is the *historical* case: an address added at
`At(p)` with `p` a few blocks below the confirmed line refetches those blocks,
because no cache spans that side. The measured window §3.6 offered there was
honest and small — 2–25 seconds on Solana, 4–20 minutes on Ethereum — and the
miss path is a bounded fetch, never a rescan. A byte-budgeted second cache for
that window is not worth `SyncPolicy` growing two fields and a chain-dependent
eviction story. Reopen only with a measurement, which no benchmark in the tree
currently provides.

**(e) `024` §2.3's wire `start` — built.**

`start` is now a required property of `CreateWallet`, `ImportWallet` and
`WatchAddress`, published as a named `Start` schema:
`oneOf[{"type":"string","enum":["fresh"]}, {"type":"integer","format":"int64",
"minimum":0}]` (`apps/api/src/api/contract.rs`, hand-written `ToSchema` over an
untagged `Deserialize`, as this section predicted). `Payments::registered` and
`Payments::observed` take it instead of hardcoding `Start::Fresh`. The
configuration's mirror-image gap is closed with the same type:
`wallets[].start` is `payment_api::Start`, so a configured wallet can finally
say `"fresh"`, which a bare `start_position: u64` could not.

Three notes for the record. The sentinel is `"fresh"`, as this section said it
would be. The refusal of a start further below the checkpoint than
`SyncPolicy::max_backfill_blocks` allows is **400**, not `024` §7's guessed
`422`: `IndexErrorKind::InvalidRequest` maps to `400` throughout
`apps/api/src/api/error.rs`, and a second mapping for one route would be the
inconsistency. And that refusal is the whole of what stops a caller-supplied
birthday from wedging readiness — it happens synchronously, under the
publication permit that fixed the checkpoint it is relative to, so no
background pass ever inherits a request that cannot finish.

*The gap as it stood:*

`Watching::watch` takes a `Start` (`sdk/indexing/src/watch.rs:80-85`), the
backfill honours `Start::At` below the checkpoint, and the configuration path
passes one through (`apps/server/src/config.rs:148-155`). The HTTP surface does
not. `Payments::registered` and `Payments::observed` both hardcode
`indexing::Start::Fresh` (`apps/api/src/api/mod.rs:222,256`), and none of
`CreateWallet` (`apps/api/src/api/wallet.rs:30-34`), `ImportWallet` (`:93-101`)
or `WatchAddress` (`apps/api/src/api/address.rs:23-29`) carries the field —
confirmed against the committed document, whose three bodies list `asset`,
`secret` and `address` and nothing else (`docs/openapi.json`).

So an API caller cannot state a birthday, which is the entire point of
`Start::At`, and step 12's headline — "`Start::At(p)` below the checkpoint is
honoured instead of silently indexing nothing" — is true of the SDK and of the
config file, and not of the API. The import route's own description already
admits it: *"earlier history needs a startup import with an explicit start
position"* (`wallet.rs:109-110`).

Two shape notes for whoever closes it. `024` §2.3's spelling is `"tip"`; §5.3
(b) renamed the variant to `Fresh`, and `Fresh` is what the type says, so the
string is `"fresh"` and the `oneOf` is otherwise unchanged. And the
configuration path has the mirror-image gap: `ConfiguredWallet.start_position`
is a bare `u64` (`apps/server/src/config.rs:119-124`), so a configured wallet
**cannot** express `Fresh` — `0` is genesis. One change fixes both, and the
`Start` end of it is already exercised: `route_contract.rs:80-81` maps both
variants in its fixture. Budget: three bodies, an untagged `serde` shape with a
hand-written `ToSchema`, the config field, and the five gates `024` §2.3 lists
— **M**, and it is a wire break, so it lands with whatever else is breaking.

**(f) The two carve-outs step 12's row cites.** §5.3 (b) holds: `Start` did not
spread across five `Wallets` signatures; it exists at one call,
`Watching::watch`, and `sdk/wallets` names it only in a doc comment and a test
double (`sdk/wallets/src/wallets.rs:107`, `composition.rs:293`). §5.3 (e) holds more
strongly than when it was written — `031` deleted redb as well, so `024` §5.5's
two-backend disagreement, its `payments_journal_output` foreign key, §9.2's
`0003_backfill.sql` and §11's first row describe a store that no longer exists
in any form. `024` §§3.1, 3.4 and 5.5, the whole of its §9 and §11's first row are
historical for that reason and should be read only for their arguments.

**(g) Two things the per-watch mapping proof settled that `024` got wrong, and
that change what step 12 is allowed to assume.** The proof is
`032` `## Proof of mapping (step 3)`; both corrections are now in `024` §3.5 in
place.

*Coverage is provably asset-independent, so sharing it across assets is correct
rather than a hazard.* `BlockInterpreter::inspect` takes `(&block, &[Address])`
and no asset at all (`sdk/indexing/src/source.rs:79-83`); both account-chain
interpreters select a transaction by address membership and then emit **every**
movement it carries, Ethereum taking all ERC-20 `Transfer` logs regardless of
what is configured (`sdk/chains/ethereum/src/native/block/transaction.rs:118-130`,
`interpreter.rs:112-140`). Inspecting `[a, b]` for an address therefore proves it
for every asset that address holds, which is why `BackfillProgress` has no asset
field and the `backfill` table is keyed by address alone
(`sdk/indexing/src/storage/sqlite/schema.rs:126-133`). A future design that
required a distinct progress row per asset would force a redundant full rescan on
every token addition; `032`'s own caution against inheriting coverage across
selections does not apply to this codebase.

*The same-address multi-asset watch was a new capability, and it is built.* The
durable side always allowed it — the schema's primary key is
`(chain, network, asset, address)` (`storage/sqlite/schema.rs:30-40`) and the
shared contract asserts a native+token pair is accepted
(`sdk/indexing/src/testing/watch.rs`, `accepts_one_address_once_per_asset`). What
refused it was the hot side, where `Set::rows` was keyed by `Address` alone and
`Watches::existing` answered `Conflict("this address is already watched")` to a
second asset — a refusal that also hid `Watches::load` silently dropping the
second durable row on restore. **The refusal was the defect.** `Set::rows` is now
keyed by watch identity and coverage by address
(`sdk/indexing/src/watches/set.rs`), `Watches::existing` refuses only a taken
identity or a taken `(address, asset)` pair, and the scan is deduplicated by
address from the earliest birthday any observation of it names — one inspection,
several observations, which is exactly what makes coverage asset-independent
above. `apps/api/tests/route_contract.rs` was changed deliberately with this
reason. `024` §3.5's worked example is therefore now expressible.

**What this leaves `024` owning.** After (a)–(g), `024` is the record for
exactly three built things — the required-spend rule change (§5.1), the
idempotent history write (§5.2, `Tables::refill`), and the batched-pass
argument (§3.2, §3.3, §3.5) — and one open one: (b)'s cancellation, which is
blocked on **G10** (`unwatch`) rather than on anything `024` specifies.
Everything else in it is built, recorded here, or void. It is not deletable
while (b) stands, and when G10 lands, (b) is the only thing that has to move
with it.

> **Correction (G10 and (b) both landed).** `024` now has **nothing open.**
> G10's `unwatch` was built first, and cancellation on top of it:
> `Backfilling::cancel(&network)` settles every range a network owes by leaving
> each affected watch as one created now would be, reached as
> `POST /v1/status/backfill/cancel`. So what `024` still owns is only the three
> *built* things above — the rationale behind the 28 code comments that cite it.
> **Deletion now waits on nothing but those citations**, which name §5.1, §5.2
> and §3.3/§3.5 and would dangle; rehoming them is a mechanical change of its
> own, and until it happens `024` stays as the citation target rather than as an
> open document.

---

## 6. What is explicitly rejected

Recorded so it is not relitigated. Each row is a decision already argued out.

| Rejected | Reason | Where |
|---|---|---|
| **A closed enum key** — `Wallets::BTC` as a Rust variant | **Tokens are configured at runtime.** `apps/server/src/chains/ethereum.rs:31-33` registers N allowlisted ERC-20 tokens, each under its own tag; an enum needs a variant per *configured deployment*, i.e. a code change per config file. USDC-on-Ethereum and USDC-on-Solana are two registrations. `const BTC: AssetTag = AssetTag::from_static("btc")` gives the owner's call site **verbatim** and still fails to compile on invalid text, because `from_static` is a `const fn` with an `assert!` | `026` §2.1 |
| **A chain-neutral `KeyPair::from_pk`** that produces a wallet | **The same 32 bytes control four different Bitcoin addresses**, depending on network and address type (`sdk/chains/bitcoin/src/native/key.rs:74-84`), and both live in the factory's `WalletConfig`. A chain-neutral `KeyPair` in the middle would carry a key that cannot tell you its own address — a worse object than either end. The split is: the *parse* is universal (`SecretBytes::from_hex`), the *key → wallet* step is the factory's, which is `wallets.from(tag, secret)` | `026` §1.3 |
| **`wallet.private_key()`** | A wallet rebuilt by `restore` never held the caller's bytes, so the accessor would be `None` for half the map. The material is a property of *making* a key — `Wallets::secret(tag)` — not of the wallet | `026` §5 |
| **A bundled `Chain` trait** with associated `Network`/`Source`/`Interpreter`/`KeyPair` types | **No generic function consumes every associated type at once.** `WalletFactory::new` legitimately takes different dependencies per chain, so the trait could only hide them behind another associated type the caller still builds. The required tree, `scripts/check-chains.sh`, `delegate_indexer!` and `examples/tests/scenarios.rs` already catch what a trait cannot; `linter.toml:140-142` caps a trait at three methods. The *network-identity* half did ship, as `base::Networked`. Reopen only if a fourth chain shows the tooling missing something a type could have caught | `017` `U1`, `018` §8 |
| **PostgreSQL** | Its one differentiating capability is **forbidden by the requirements**: `docs/SYSTEM_REQUIREMENTS.md:23-25` puts "HA or multi-writer deployments" under *Out of scope (MUST NOT exist)*, and the rest of the system agrees — in-memory state above the confirmed line, per-process send reservation, no leader election, no lease table. It cost 2,631 lines of contract, its own migrations, `validate_schema`, a Docker harness and 5.5% of the suite as its slowest 5.5%. If SQL inspection is ever wanted, the answer is `--export`, not a second live store | `027` §4 |
| **An Ethereum no-trace mode** | A `"native_balance": "transactions_only"` opt-out is "a way to ship a wrong number with paperwork attached". `AGENTS.md:400`: *reject unsupported meaningful input; never accept ignored configuration*. The probe traces the node's own head rather than asking whether the method exists, because a node that answers `debug_traceBlockByHash` while ignoring `diffMode` returns a bare prestate, which reads as every touched account being emptied | `021` §6.5, owner |
| **Encryption at rest in the SDK** | Custody is the caller's problem. The SDK persists what it is given so a wallet survives a restart; protecting the store and choosing which keys reach the API belong to the operator. The expressible form of the decision is `Wallets::new(index, Some(store))` versus `Wallets::new(index, None)` | `017` item 2, `026` §2.5 |
| **A default `Start`, or `Option<BlockPosition>`** | A birthday below the checkpoint used to be accepted silently and index nothing. `Option` reads as "unspecified", which is the thing being removed; a `u64` sentinel is a magic number in a wire format. `Start::Fresh` is an assertion of fact, so "I don't know" stays inexpressible | `024` §2.1, `026` §1.4 |
| **A fourth method on `TransactionBuilder` or `TransactionFactory`** | `linter.toml:140-142` caps a trait at three and both are at three. Inspection goes **inside the values those methods already return** — `Draft` carries the typed intent *and* the snapshot; `SignedTransaction` carries the fee | `026` §1.8 |
| **A chain-neutral per-transaction fee knob** | `sat/vB` against `max_fee_per_gas` against a compute-unit price. The chain-neutral form is a *ceiling*, and `signed.fee()` plus dropping the builder is one. Revisit only for replace-by-fee, which is a different design | `026` §1.8 |
| **Holding a signed transaction as a reservation** | `Outbox::sign` deliberately **releases** the outputs (`sdk/chains/bitcoin/src/sender/outbox.rs:126-129`); a concurrent send may take them and the broadcast then fails *definitely*. On Solana the blockhash also expires in about a minute | `026` §5 |
| **Rewinding the checkpoint to backfill**, or a second `Service` that merges | Rewind is impossible below retention and destructive above it — it is the offline `--rescan`, the wrong tool for "a user created a wallet". A second service needs a second checkpoint, a second CAS domain and a duplicated durable state machine for a transient job, and its merge step *is* the idempotent write it was trying to avoid | `024` §3 |
| **A stored running balance, or a resulting balance on every history row** | The first is a reorg-unsafe denormalisation maintained on the hot commit path; the second makes page *n* depend on pages `1..n-1` and is actively misleading whenever completeness is not `Complete`. Revisit the first above ~10⁶ movements per address, and then only as a *checkpointed* total below the irreversible line | `021` §5.4, §8.2 |
| **`getSignaturesForAddress` as a Solana completeness proof** | Retention-bound and blind to non-signature effects. Already rejected as a sync mechanism in `docs/adr/0026` | `021` §7.4 |
| **Archiving raw blocks or traces** | `AGENTS.md:352-356` forbids it and nothing needs it. `BlockInterpreter::inspect` is pure and synchronous, so whatever it reads must arrive with the block and may be dropped after | `021` §6.4 |
| **Deleting the supervisor** | 561 production lines of *tested failure policy* with six callers and eight scenarios. `Unit::drive` is a five-state machine over quarantine, retry, park, resume and panic-respawn — every state is a bug someone already hit. Deleting it moves them into a binary where `tests/supervision.rs` cannot reach them | `027` §5.1 |
| **An HTTP rescan endpoint** | Purging a network while HTTP serves its history would hand clients an empty history that looks authoritative, and checkpoint-bound cursors would conflict mid-page | `019` §5.3 |
| **One id space over wallets and watched addresses inside `wallets`** | Once the index owns the watch list, `GET /v1/wallets/{id}` spans two sources and `apps/api` unions them (`Payments::subject`). Keeping a shadow copy in `wallets` reintroduces the second owner the split exists to remove | `026` §5 |
| **A container for the id→wallet map inside `sdk/wallets`** | It would be `Wallets` renamed, which is the objection. Once publication, persistence, custody and filters are gone, the caller's residue is a `RwLock<BTreeMap<String, Arc<dyn Wallet>>>` with three methods, and it belongs beside `Payments`, which already holds both handles. **The map does not disappear — it moves** | `029` §3.2, §10 |
| ~~**`Watching::unwatch`**~~ **BUILT** | The refusal read: `Registry` is at `linter.toml:140-142`'s three-method cap once `advance` lands, and an unwatched address's committed history has no defined fate — still in the append-only store, still returned by `TransactionStore::list`, and the only removal the design has is `purge_network`. Both are answered. `advance` was never built, so the third `Registry` method is `unregister`; and the sentence G10 asked for is *the rows are kept, the claim is not* — `unwatch` stops inspection and deletes no indexed row, so it is reversible, while that address's recorded coverage goes with the watch and a re-watch reads its range again. `docs/SYSTEM_REQUIREMENTS.md` revokes the requirement in place | `029` §5.2, G10 |
| **An async `Listener` the engine awaits** | With no features on, `indexing` names no async runtime at all, which is what lets `apps/api` take it with `default-features = false`. A bounded queue needs a spawner and a drop-or-park policy; the listener writes that in two lines with a channel. `SyncMonitor` is already synchronous *on purpose*, with a comment saying why | `029` §6.3 |
| **A `Listener` that can veto, or at-least-once delivery** | The block is already durable when the listener runs — there is nothing to refuse. The only veto is `BlockStore::add` returning `Err`, and implementing that port is the price of having one. At-least-once means either announcing blocks that changed nothing or putting a durable queue inside the commit | `029` §6.4, §10 |
| **`indexer.register(storage)` as a runtime call** | The first pass captures the store's checkpoint into a `SyncPlan` and the overlay holds blocks validated against it. A store supplied later answers a different checkpoint than both. The store is a constructor argument, as today | `029` §10 |
| **`OutputStore::list` or `units()` on the `Indexer` trait** | A composer spanning Bitcoin and Ethereum would have to answer "list this address's outputs" for a chain that has none — a fourth error variant or a lie. Live outputs are a UTXO chain's own read, taken by that chain's factory at the composition root. `units()` *is* the multi-chain fact; a single chain is supervised by `Composer::new(vec![index])?`, one line, already legal | `029` §7.3 |

---

## 7. Open decisions

Each is a question for the owner, with the trade-off in two lines.

| # | Question | Trade-off |
|---|---|---|
| **G1** | **The `--export-keys` migration.** Step 3 removes `RegisteredAddress.material`, where every deployed wallet's key lives. `029` §9.2 proposes a one-release offline subcommand to lift them out. Is there a deployment that needs it? | Build it: ~80 lines, deleted one release later, and no operator loses a wallet. Skip it: only safe if nothing is deployed — and `AGENTS.md:42` already describes the schema as predeployment, so this is the same question `027` §4.4 asked about `psql` and got answered "nobody". **One word settles it.** |
| **G1b** | **`Listener` delivery is at most once.** A crash between the store commit and the call loses the notification (`029` §6.4); there is no second durable write to make it at-least-once. | Accept it and document the recovery: a listener that must not miss a block reads `Checkpoint` at startup and catches up through `History` — which is what a correct consumer does anyway. Make it at-least-once: a durable queue inside the commit path, which is the machinery §0 exists to avoid. |
| **G2** | **SPL sending.** Solana can *index* SPL tokens today but the send path is native SOL only. Build it? | Building it means associated-token-account creation, rent-exemption funding, and a second fee asset story on one chain. Not building it means `GET /v1/assets` advertises a token the wallet cannot spend — the asymmetry a caller will hit first. |
| **G3** | **The `NetworkRules` wart.** A trait with exactly one implementation (`impl NetworkRules for base::Network`, `sdk/indexing/src/network.rs:29`) existing only so `require_chain` and `select_addresses` can be spelled with a dot, because `IndexError` lives in `indexing` and `Network` lives in `base`. | Keep it: two free functions become `network.require_chain(..)`, which reads better in three interpreters. Delete it: `indexing::require_chain(&network, ..)` is honest and one fewer public trait, at the cost of touching three chain crates. A third option — move `IndexError` to `base` — is worse: `base` would then name an indexing vocabulary. |
| **G4** | **The redb owner thread and command queue.** ~900 production lines, one `tokio::mpsc` channel and one `std::thread` running `blocking_recv` (`storage/redb/backend.rs:162-178,294-295`), through which `Get`, `Scan` **and** `Commit` are all serialized. It buys no parallelism; it exists so synchronous file I/O never touches an executor, and so reads can happen outside the write transaction. | Keep: it is correct, tested, and the alternative (`spawn_blocking` per call, or redb's own `ReadTransaction` held across an await) has its own hazards. Replace: a read path that does not queue behind a commit would remove the queue's only remaining justification and ~300 of those lines. **Measure before deciding** — no benchmark exists. |
| **G5** | **Is the cursor `epoch` worth it before backfill exists?** `024`'s `PageAnchor` is now the *only* reason a history cursor can be refused (`025` deleted the checkpoint half and the 409). | Land it in step 12 with the thing that needs it: one fewer public shape change, but every cursor issued before the deploy decodes as one `400`. Land it early and alone: the `400` happens on a quiet day instead of a busy one. |
| **G6** | **The `ErrorKind` naming decision.** Six types named `ErrorKind` across the graph (`base`, `base::transaction`, `wallets`, `crypto`, `json-rpc`, `http-server`), so every consumer aliases on import — and `017`'s `U7`, making `kind` private behind an accessor, has the same blast radius. | Do both at once in step 8: one wide mechanical change, and the graph stops needing aliases. Do neither: it stays stylistic and nothing breaks. Doing them separately is the only bad answer. |
| ~~**G10**~~ **CLOSED** | **`Watching::unwatch`, revisited.** `029` §5.2 refused it for two reasons: `Registry` hits the three-method cap once `advance` lands, and an unwatched address's committed history has no defined fate. | **Built.** The first reason evaporated — `advance` was never built, so `unregister` took the third slot. The second was answered with the sentence this row demanded: **the rows are kept.** `unwatch` stops future inspection and deletes nothing; deleting would be a second removal path beside the offline `purge_network` and an irreversible one, for an act that is otherwise entirely reversible. An application cannot reach the rows meanwhile, because every HTTP read is by `id`; an SDK caller holding the address still can, because a read answers for a `Holder` and consults no watch list. A re-watch does **not** inherit the recorded coverage: `unwatch` forgets it (`BackfillStore::forget`), because a coverage row claims every block from the birthday to `resume` was inspected for the address and that holds only while the address is observed without interruption — no field in the row separates "covered" from "nothing was watching". So a re-watch reads its range again, bounded by the same `max_backfill_blocks` and idempotent where the rows exist. The *asset*-independence `024` §3.5 argues is untouched and is still why one backfill serves every asset an address holds; what is not inheritable is a claim across an interruption. The first attempt here was to inherit the resume point and re-anchor instead, and `service_contract`'s restart scenario refuted it: for a *continuously* observed address the stored anchor is exactly right, and the two cases are indistinguishable from the row. It takes the same `PublicationPermit` a `watch` takes and is idempotent. |
| **G7** | **A startup deadline for a network stuck `Retrying`.** `019` §4.2 left this out deliberately: readiness waits forever for a network that never settles. | A deadline turns an indefinite hang into a `Quarantined` network the operator can see and resume. Without one, a misconfigured node holds `/health/ready` at 503 with no bound and no alarm beyond `attempts`. |
| **G8** | **Native-ETH and Bitcoin completeness.** Only ERC-20 has a real oracle (`eth_getLogs` from the token's deployment). For native ETH, Bitcoin and Solana, `Complete` can only ever mean `FromBirthday`. | Accept the asymmetry and label it honestly — a wallet can be complete for its ERC-20 asset and not for native ETH at the same address. Or refuse to report completeness at all for those three, which is more honest and less useful. |
| **G9** | **Bitcoin's depth, restated as a product question.** `confirmation_depth = 6` gives ~1 hour to confirmed and quarantines on a deeper reorg; `100` reproduces the old journal's safety envelope at ~16 hours. | The deepest Bitcoin mainnet reorg was 24 blocks, in March 2013, from a consensus bug rather than mining, and there has been no ≥3-block reorg since. The default is already 6; the question is only whether the *documentation* should push operators toward a higher number for large settlements. |

---

## 8. How to read this against the others

| Document | What it still owns |
|---|---|
| `017` | the record of what `016`'s punch list closed, and the `U*` deferrals feeding steps 8 and 15 — **closed** by step 8's naming sweep. Renames: `base::Chain` → `ChainNetwork` (it held one *network*, and collided with `sdk_config::Chain`, a trait), `ChainCollection` → `ChainNetworks`, `CHAINS` → `NETWORKS` ×3, `Asset.chain` → `Asset.network`, `Payment` → `Transfer` with `destination` → `to` (`transfers: Vec<Payment>` disagreed with itself in one line; `destination` survives only on the wire), `KeyTweak` → `ScalarTweak` (defining crate wins), `json_rpc::Decision` → `Trigger`, `Address::new`/`Ref::new` tightened from `impl fmt::Display` to `impl Into<String>`, `SecretBytes::from_hex` → `Result` (its `Option` was *why* both examples reached for `hex::decode` instead of the one documented parse), 11 `//!` headers added. `check-chains.sh`'s `base_items` moved in the same step as each rename. **The `ErrorKind` decision, measured rather than assumed:** all nine bare `ErrorKind`s were candidates but exactly **one** caused every alias in the workspace, so only `wallets::{Error, ErrorKind}` → `{WalletError, WalletErrorKind}` changed — matching its peer `indexing::{IndexError, IndexErrorKind}` and deleting twelve aliased items. The checkable rule is now in `docs/CHAIN_LAYOUT.md` §5: no file imports a workspace error type under an alias. **Three items refused with reasons:** `U7`'s private `kind` (`json_rpc::Error` is a struct literal in four chain files and `Error::new` is `pub(crate)`, so privacy would force a new public constructor, and there is no invariant to protect); `U5`'s Solana factory (`Indexer::progress` is `pub(crate)`, returns a `watch::Receiver`, and is not an `indexing` capability, so no port can supply it); `json_rpc::Repetition` (verified precise). **Four claims were wrong or stale:** `generate_ed25519` already validates the curve, the dead `assets` vector went with the examples rewrite, and `base`'s alleged `Output` fog was false — neither `transaction::Output` nor `SpentOutput` exists. `017` is deleted; this row is its record, and its own §6 held the detail |
| `019` | the failure-isolation design and its vocabulary — **landed in full**, except §6.3, which `024` supersedes ("accept and backfill"). Its last open item, §5.3's **resume**, is now wired: `SyncControl::resume` existed but `Supervisor::control()` was dead code and `apps/api` registered only `read`, so a quarantined network had no recovery short of restarting the process. It is now `POST /v1/status/resume` → `202 {network, cleared}`, beside `GET /v1/status` and resolving the network from the same `SyncHealth` snapshot, so the two cannot disagree. Three decisions the spec never stated: a one-method `payment_api::Resume` port, because `apps/api` takes `indexing` with `default-features = false` and cannot name `SyncControl` behind the `supervisor` feature; **409** for a network that is not quarantined, since `resume` is a no-op there and `202` would claim an action that did not happen; and `cleared` is a record of the failure that was cleared, never a claim the cause is fixed — an unfixed cause re-fails through the ordinary attempt/quarantine path. One documented asymmetry: quarantine is per **unit**, so a resume covers every network sharing that unit (today one unit is one chain). `019` is deleted; this row is its record, and the 12 `docs/design/019` citations in code are rationale pointers |
| `021` | the attribution argument, the per-chain fee rule, and the four correctness gaps of step 13 — **closed**. Solana's residual, the `balanceOf` witness, and Ethereum consensus-layer withdrawals all landed; the withdrawal fold is `BlockCredit { id: MovementId, asset, amount, to }` on `InterpretedBlock`, validated in **both** `BlockAddition::new` and `BackfillBatch::new` (the second was not in §5.3 (g) and was necessary — without it an address watched from its own past silently drops every credit below its anchor), folded by `Ledger::apply_credit`, stored in SQLite schema step 3's `block_credit` table, counted as pending-incoming, and reported by `--rescan`. Its pinning test flipped from "folds to nothing yet" to asserting real balances. **One premise of the plan was wrong**: ERC-20 `eth_getLogs` backfill is *not* a correctness gap — a token holding with a past birthday already recovers its history, because Ethereum's `inspect` reads `Transfer` logs from the receipts `RawBlock` already carries and `Source::blocks` serves a historical range like a forward one. It is a cost path and an enumeration proof, designed in `021` §7.6 and deliberately unbuilt: `BackfillProgress` is keyed by address, not `(address, asset)`, so settling a shared row on a log query would claim native coverage nothing read (internal ETH emits no log). Two smaller deferrals recorded there: §8.2's `cause` on a history row — so **a withdrawal-funded address currently has a balance and an empty history page** — and §7.5's `max_backfill_blocks` relaxation for token holdings. `021` is deleted; this row is its record |
| `024` | the backfill mechanism of step 12, minus its `Start` signatures (§5.3 b) and minus everything journal-shaped (§5.3 e). After step 12 landed, what it still owns is three things: the required-spend rule change (§5.1), the idempotent history write (§5.2), and the batched-pass argument (§3.2, §3.3, §3.5) — the rationale behind 15 code comments that cite it. **Now closed, and deleted; this row and §5.5 are its record.** Both items §5.5 listed as open are built. §2.3's wire `start` reached all three wallet-creating routes as a `Start` schema (`"fresh"` or an integer), and the config side gained the same vocabulary so a configured wallet can say `fresh` — `0` is genesis, which a bare integer could never distinguish. §7's availability floor was resolved by *not* gating: readiness follows **forward** indexing only, because a pruned range is a property of one watch's request rather than of the chain, and an unservable range is reported on `GET /v1/status → networks[].backfill` instead of holding the whole API at 503 indefinitely. §7's cancellation is `Backfilling::cancel(&network) -> Vec<Watch>` → `POST /v1/status/backfill/cancel`, leaving each owing watch re-created from `Start::Fresh` at the checkpoint's successor so every read's `FromBirthday` stays honest once the range below will never be read; registration is rewritten *before* coverage settles, so a crash re-owes the range instead of settling a row under the original birthday. It required **G10, `unwatch`**, now `Watching::unwatch(id) -> Option<Watch>` with one durable half, `WatchStore::forget`, which removes the observation and the coverage no other observation of its address still claims: it deletes nothing indexed, but drops the recorded coverage, because that row claims every block from the birthday to `resume` was inspected and that holds only while the address is observed without interruption — so a re-watch re-reads its range rather than inheriting a claim that spanned a gap. Coordinated by the same `PublicationPermit` as `watch`; idempotent on an unknown id; **no HTTP route deliberately**, since "stop watching this wallet" would make a funded wallet unreachable by id while the process still holds its key. `Watches::covered` → `loaded`, and there is one flag rather than two: `WatchStore::all` returns each observation beside its own coverage, so the list and the progress are restored from one snapshot. §3.5's same-address multi-asset worked example is now expressible; §8's own row above is its record. The 28 `docs/design/024` citations in code are rationale pointers, as `019`'s and `025`'s are. Everything else in it is built, abandoned in §5.5, or void; its two status blocks, its §3.5, and its §10 verdict table now carry corrections against the tree |
| `025` | the confirmed/pending model — **landed in full**, and the proof of §0's pattern: committing the tip grew a ~950-line rollback journal, and storing only what is below the confirmed line deleted the journal rather than fixing it. Its last open item, §9's **second `confirmed_tip` guard**, is now implemented: `ConfirmedLine` holds a per-network high-water mark and `hold_confirmed` refuses a line that moves backwards as a retryable `Source` error, so one stale replica costs a pass and a persistent one quarantines through the ordinary attempt count; the mark resets on restart, because a new process cannot vouch for what a previous one was told. Two deviations its text never recorded: `History::unconfirmed` never existed (the overlay surfaces as `Balance.pending` and `TransactionPage.pending`), and `SyncPolicy` was never collapsed into `SyncConfig`. `025` is deleted; this row is its record, and the ~19 `docs/design/030 §8 (ex-025)` citations left in code comments are rationale pointers, the same way `005`/`010`/`011`/`012`/`014`/`018`/`023` already are |
| `026` | the `wallets` purpose test, and steps 6 and 7 in detail. Its §1.1/§2.1/§2.5/§2.6 are superseded by `029` — **landed in full**, verified against the tree. Three records its §4 never got, kept here because `026` is deleted: (a) `Claim` landed with a different shape than row 10 describes — now `Claim(BTreeSet<String>)`, one rule (two sends collide when they take one scarce resource in common) instead of three chain-specific variants; (b) the `Tag` trait was abandoned in fact, the bound staying inline at `sdk/wallets/src/wallets.rs:58-60`; (c) `Observation::Unwatched` is moot. Its §2.6 "kept" list named `HistoryAsset`/`HistoryAssets`, both since deleted — decimals now live in `base::Unit`/`base::Units`, published per chain by `Config::units()` |
| `027` | the one-crate tree, the features, and the redb format guarantees — **landed**, including §6.1 commit 3, whose earlier refusal is **reversed**. That refusal rested on a `Repository` binding one *network* to a shared file handle, so that collapsing the two steps would reopen the database per network. A repository is no longer bound to a network: `sqlite::Repository::new(database)` is infallible, serves every network in the file, and takes the network from each operation, so `Database::open` is the only step that can fail and the two are one factory in effect. `apps/server/src/storage.rs` opens the file once and hands every chain the same repository. `027` is deleted; this row is its record |
| `028` | steps 2 and 9–11 in detail, and the layer arithmetic that puts `Composition` in `sdk/wallets` — **landed in full**, step 11 included. Five deviations its own text never recorded, kept here because `028` is deleted: `Deps` was dropped for a bare `&Arc<dyn SubmissionRegistrar>` (a one-field aggregate is ceremony); `Connected` became `Node` (`rust/struct-noun-naming` rejects a bare participle); `Composition<A: Ord>`; `Schedules` replaced `Vec<(Network, Duration)>`; `Repository` became a blanket bound so a port added to the index is added once rather than in three chain signatures. Step 11 landed as registration rather than as a static loader table: `sdk/config`'s `Registry` stores `name → Box<dyn Fn(&Value) -> Result<Box<dyn Chain>>>` and names no chain, so `apps/server/src/chains.rs` is 33 lines and a fourth chain is one `register` line |
| `032` | the indexing model: the two-port minimum, queries as consumer capabilities, the bounded pass on its owner, and per-watch contiguous progress — **landed in full**, and it is the record of a mistake worth keeping. An earlier banner on it said three of its mappings were "disproven"; that overstated the proof, which had found **migration obligations**, not impossibilities. The coordinator wrote the overstatement into a brief telling an agent not to attempt the core work. Corrected, then built. What landed: `Repository: WatchStore + IndexStore` (a minimal sink is five methods, no `Clone`, `Arc<Sink>` via blanket port impls); `TransactionStore`/`BalanceStore`/`OutputStore`/`Purging` demoted to consumer capabilities; `Indexing` the minimum surface and `Indexer` the complete one, so a query-less store runs as `Index<dyn Indexing>`; `Synchronizer` deleted and the bounded pass moved onto `Service`'s own state; `Composer` renamed `Index` with `Index::over(store).chain(..).build()`, which launches nothing; `WatchProgress { watch, indexed_through }` and one `IndexStore::commit(IndexedBatch)` that derives fill-or-extend from the blocks, deleting `IndexStore::backfill`, `BlockAddition`, `BlockOutcome`, `BackfillBatch`, `BackfilledBlock`, `BackfillOutcome`, `BackfillProgress`, `Coverage` and seven `Set` methods. The three facts the global checkpoint serves — chain finality, per-watch progress, query snapshot anchors — stayed distinguished rather than collapsed. **The latency trade is one named predicate**, `WatchProgress::caught_up(checkpoint)`, with both behaviours in its doc comment and four callers: a watch below the line no longer joins the forward selection until its history is contiguous, so an imported old wallet waits for catch-up instead of seeing a payment immediately. Flipping that is that line. Three things found only by building it: the `AlreadyApplied` replay path existed for a widened replay contiguity makes impossible; `BackfillBatch::filled` had been validating one check less than `CanonicalBlock::new`, so a backfilled block with two transactions of one identity was accepted where a committed one was refused; and a review caught `Composer::sync` using `?` in its loop, which had silently cost bounded `sync` the per-chain failure isolation `019` exists for. Budget ≈ −155 production lines net, four files deleted, three added, no new lint exception. `032` is deleted; this row is its record, and `033` carries the reduction work it exposed |
| `029` | steps 1, 3, 4 and 5 in detail: the listener, the watch API, what `Wallets` stops holding, and who holds it instead — **landed in full** (commits A–E), verified against the tree. Two cosmetic staleness points in the deleted document, recorded here: `Registry` never gained `advance` (backfill progress went to `BackfillStore` instead, written inside the chunk's own transaction), and `Signers::get` returns `Option`, not `Result`. `029` is deleted; this row is its record |
| `031` | the storage-engine decision — **landed in full**: SQLite is the only backend and redb is deleted (22 files, ~5,860 lines, both format fixtures, `bincode 2.0.1` out of the lock). `031` is deleted; this row is its record, and it records four things the document itself got **wrong**, proven by the two gates it asked for. (a) **§5.2's checkpoint compare-and-set is unsound**: `(position, hash)` is not monotonic across a rescan, so the proposed `UPDATE … WHERE position=? AND hash=?` matches again after exactly the purge-then-rescan it was introduced to refuse — the stale write lands. The design is one `BEGIN IMMEDIATE` per operation, which removes the read-to-write window so no CAS of any kind is needed. (b) The **ABA lives in the port's vocabulary, not either engine's** — both backends accepted a stale `BlockAddition` after purge + rescan, because `expected_checkpoint` is a `BlockRef` *value*; what actually rules it out is that `purge_network` is contractually offline. (c) **The ports do not shrink** — the SQLite `Registry` port is larger than redb's and `BackfillStore` grew 39%; every line of saving was in the engine layer (1,552 → 473). Its ≈−840 estimate was right by two errors cancelling. (d) A **connection pool is mandatory, not optional**: one mutexed `rusqlite::Connection` made a reader 28× slower under write load. Two findings it never contained: redb refused ~3% of history pages while indexing (the payments API's main read path), and a benchmark without `PRAGMA fullfsync` measures nothing on macOS, where `File::sync_data` is `F_FULLFSYNC` and SQLite's `synchronous=FULL` is not. `PurgedRows.movements` now reports real counts where redb always said 0, and the storage config lost its backend wrapper (`{"redb":{"path":…}}` → `{"path":…}`) |

This document is the order and the target. When one of those disagrees with it
on a detail, **they are right about the detail and this is right about the
order** — except where §5.3 names a collision, where this is the resolution.
