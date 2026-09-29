# 024: An address must be added with a start height, and the indexer must honour it

> "before you can add address to observer you must add block height to iterate to"

> "If we add 10 addresses upfront, the lowest block should win, and we should iterate
> and resolve all addresses at once — not resolve blocks separately for each address.
> We add addresses to the observer in batch and resolution starts. We keep X last
> blocks in memory, so when we create a new wallet and add a new address close to the
> tip, we are not going to reindex everything again."

Today an address can be added with no start point at all (the HTTP routes),
or with one the indexer silently discards (the config path). Neither is
acceptable. This document makes the start point **mandatory on every add path**
and makes the indexer **honour it below the checkpoint** by a batched backfill
pass that runs inside the ordinary sync pass, writing history and output facts
into blocks the scope already committed without moving the shared checkpoint.

> **Status.** §9.5 commit 1 has landed: `BlockOutcome::Applied` carries
> `unknown_spends`, both backends report an unmatched required spend instead of
> raising `InvalidBlock`, `SyncStatus::unknown_spends` surfaces every miss a
> pass committed, and `indexing::testing::SpendContract` pins the rule on both
> backends. Nothing else in this document is implemented: there is no `Start`
> type, no backfill phase, no `BackfillStore`, no `PageAnchor`, no recent-block
> cache, and no `Observation` on a wallet — so the reported miss is visible to
> an SDK caller of `Indexer::sync`, not yet recorded per wallet or on the wire
> (§6, commit 7 and 8). Commit 1 needed no schema change on either backend.

It supersedes `019` §6.3, which recommended refusing a birthday below the
checkpoint. That recommendation was correct given no backfill; the decision
above replaces "refuse" with "accept and backfill", and `019`'s refusal survives
only as the *unservable-range* case of §7.

Paths are repository-relative. Line numbers are from the working tree on
`refactor/chain-unification`. `019` is mid-flight: `sdk/indexing/src/health.rs`
exists, `sdk/indexing/supervisor/src/lib.rs` is still the pre-`019` `SyncState`
loop, and `SyncMonitor` does not exist yet (zero hits repo-wide). Where this
document names a `019` type it names `019`'s target shape and says so.

---

> **Status (materially simplified by `025`, not implemented).** §5.5's central
> problem — redb's backfilled rows surviving a revert, orphaned, so each chunk
> would have to append its keys to the `JournalRecord` of every affected
> retained height — **is gone**: there is no revert, and no journal. §5.5's
> retention-boundary table (journalled tail vs unjournalled body, driven by
> `payments_journal_output`'s foreign key) is gone with it, and §11's
> "reversing a backfilled spend below retention" stops being a limitation.
> §3.6's recent-block cache is already built, as the overlay `025` adds: the
> same list over the same range, truncated at the same point. Its interpreted
> facts cover every block above the confirmed line unconditionally; its native
> blocks are a bounded suffix kept only so a re-inspection after an address
> insert usually costs no refetch. `recent_blocks` is therefore not a
> configuration value and `reorg_retention` is not its default. §5.3's
> `PageAnchor`/`backfill_epoch` **stays**, and is now the only reason a history
> cursor can be refused, because the checkpoint half of that cursor and its
> HTTP 409 are deleted. §5.1's `unknown_spends` rule is untouched. Backfill
> still writes below the checkpoint, which is now the confirmed line and so
> slightly lower.

## 1. What is true today

### 1.1 Three add paths, three different birthday rules

| Path | Entry | Birthday | Caller control |
|---|---|---|---|
| `POST /v1/wallets` | `apps/api/src/api/wallet.rs:59-71` → `Wallets::generate` (`sdk/wallets/src/wallets/persistence.rs:23-33`) | checkpoint + 1 | none |
| `POST /v1/wallets/import` | `apps/api/src/api/wallet.rs:126-142` → `Wallets::adopt` (`persistence.rs:42-59`) | checkpoint + 1 | none |
| `POST /v1/addresses` | `apps/api/src/api/address.rs:52-68` → `Wallets::watch` (`persistence.rs:71-93`) | checkpoint + 1 | none |
| config `wallets[]` | `apps/server/src/config.rs:72-102` → `Wallets::import` (`sdk/wallets/src/wallets.rs:163-185`) | `start_position: u64`, verbatim | full, **and ignored** |
| restart | `Wallets::restore` (`persistence.rs:106-139`) → `Registry` row | stored `start_position` | — |

`CreateWallet` (`wallet.rs:27-34`), `ImportWallet` (`wallet.rs:91-102`) and
`WatchAddress` (`address.rs:20-29`) all carry `#[serde(deny_unknown_fields)]`
and no birthday field; a body carrying one is a `400`
(`apps/api/tests/route_contract.rs:1138-1142`).

### 1.2 A birthday below the checkpoint is discarded

`catch_up` reads `plan.earliest_position()` **only** when there is no checkpoint:

```rust
// sdk/indexing/src/synchronizer.rs:137-140
let mut applied = 0_usize;
if checkpoint.is_none() {
    (checkpoint, applied) = self.initialize_checkpoint(observed_tip, plan).await?;
}
```

`earliest_position` is called from exactly one place,
`initialize_checkpoint` (`synchronizer.rs:179`). Once a checkpoint exists,
`start_position` survives only inside `SyncPlan::active_addresses`
(`sdk/indexing/src/admission.rs:225-231`), where it *excludes* an address from
blocks below its birthday. Nothing ever indexes below the checkpoint.

And the explicit-birthday branch of `Wallets::store` takes **no publication
permit at all**, so the birthday is never compared with the checkpoint by anyone:

```rust
// sdk/wallets/src/wallets.rs:322-340
let publication = if start_position.is_none() {
    let persisted = self.checkpoint.checkpoint(&factory.network).await?;
    let admission = self.admission(&factory.network)?;
    Some(admission.publication(persisted).await?)
} else {
    None
};
```

`apps/server/tests/acceptance.rs:19-27` configures `"start_position": 1` and
passes vacuously on any restart.

### 1.3 The terminal failure is already armed, today, with no backfill involved

Bitcoin classifies a spend as **required** when the spent output's owner is in
the current selection, and as **tracked** otherwise:

```rust
// sdk/chains/bitcoin/src/interpreter.rs:177-182
if self.selected.contains(address) {
    relevant = true;
    self.spend(input.outpoint, key)?;
} else if previous.script().is_p2wpkh() || previous.script().is_p2tr() {
    self.track(input.outpoint, key)?;
}
```

A required spend whose output the index does not hold is a non-retryable
`InvalidBlock` in both backends
(`sdk/indexing/redb/src/repository/blocks.rs:196-203`,
`sdk/indexing/postgres/src/projection.rs:216-222`):

```
IndexError { kind: InvalidBlock, message: "block spends an unknown indexed output", retryable: false }
```

This does **not** require a historical birthday. Watch or adopt any
already-funded Bitcoin address at checkpoint + 1: its pre-birthday outputs were
never indexed, so its first spend after the birthday raises the error and — per
`019` §1 — stops every chain today and quarantines Bitcoin after `019`. `019`
§6.1 described this as a consequence of a past birthday; it is in fact the
normal outcome of `POST /v1/addresses` on a funded address. §5.1 fixes it.

### 1.4 History rows already exist for addresses that were never selected

A history row is written once per `(transaction, address)` for **every**
endpoint of every movement plus the fee payer:

```rust
// sdk/indexing/src/observation.rs:225-241
pub fn addresses(&self) -> BTreeSet<Address> { /* every movement from/to + fee.payer */ }
```

`write_history` iterates that set (`redb .../blocks.rs:123-128`;
`postgres/src/projection.rs:12-21` with `payments_history` keyed
`(chain, network, address, height, transaction_id)`,
`sdk/indexing/postgres/src/schema.rs:171`). So a newly added address routinely
**already has rows** in `[start, checkpoint]` — every time it was the
counterparty of an already-watched wallet. Both backends today treat a second
write of such a row as a retryable `Conflict`
(`redb .../blocks.rs:136-143`, `postgres/src/projection.rs:254-264`). §5.2.

---

## 2. API shape

### 2.1 The type

`BlockPosition(0)` is genesis, so a bare integer cannot express "from now"
without colliding with "from the beginning". A sentinel is therefore required,
and a sentinel that is not a number is the only one that cannot be typed by
accident.

```rust
// sdk/indexing/src/service.rs — beside AddressFilter
/// Where an address begins to be observed.
///
/// `Tip` anchors at the scope's current checkpoint successor and claims no
/// history. `At` names a native position; below the checkpoint it is
/// backfilled (024), never discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    Tip,
    At(BlockPosition),
}
```

It is a **native position**, not a produced height, whatever the owner's phrasing:
`docs/INDEXING.md:55-62` pins birthdays to `BlockPosition` ("never pass a height
to RPC as a position"). On Bitcoin and Ethereum `position == height`; on Solana
it is a slot. `AddressFilter.start_position` (`service.rs:13-19`) keeps its type
and its name; `Start` exists only at the *add* boundary, and `Wallets::store`
resolves it to a `BlockPosition` under a permit.

Rejected alternatives: `Option<BlockPosition>` (`None` meaning tip) — it is the
shape that produced today's two-branch `store` and reads as "unspecified", the
exact thing the decision forbids. A plain `u64` with `u64::MAX` as the sentinel —
a magic number in a wire format.

### 2.2 SDK signatures

| Today | After |
|---|---|
| `Wallets::generate(&self, id, asset)` `persistence.rs:23` | `generate(&self, id: I, asset: &A, start: Start)` |
| `Wallets::adopt(&self, id, asset, secret)` `persistence.rs:42` | `adopt(&self, id, asset, secret: SecretBytes, start: Start)` |
| `Wallets::watch(&self, id, asset, target)` `persistence.rs:71` | `watch(&self, id, asset, target: &dyn Addresser, start: Start)` |
| `Wallets::import(&mut self, id, asset, secret, BlockPosition)` `wallets.rs:163` | `import(&self, id, asset, secret, start: Start)` — **`&self`** |
| `Wallets::insert(&mut self, id, asset, wallet, BlockPosition)` `wallets.rs:200` | `insert(&self, id, asset, wallet: Arc<dyn Wallet>, start: Start)` — **`&self`** |
| `Wallets::restore(&mut self, asset)` `persistence.rs:106` | unchanged signature; start comes from the registry row |

`import` and `insert` lose `&mut self`. Their exclusivity existed for exactly one
reason — "historical selection … must not be introduced once synchronization is
running" (`wallets.rs:165-167`, `persistence.rs:98-101`) — which is the thing this
document removes. Dropping it also deletes the `store()` fork
(`wallets.rs:322-340`): **every** add now takes a `PublicationPermit`, reads the
checkpoint under it, and resolves `Start` once. `apps/server/src/main.rs:59-62`'s
`&mut Wallets` window before `Arc::new(wallets)` stops being load-bearing.

`restore` keeps `&mut self` only because it inserts in a loop before the
collection is shared; it could also be relaxed, out of scope here.

`Registry` gains one method (it has two — `register`, `registered`,
`sdk/indexing/src/registry.rs:57-67` — and `linter.toml:140-142` caps traits at 3):

```rust
/// Records how far this address's backfill has durably progressed.
fn advance<'a>(&'a self, id: &'a str, done_through: BlockPosition)
    -> BoxFuture<'a, Result<(), IndexError>>;
```

`Registry` is application-owned (`AGENTS.md`: "Sync repository code never reads,
writes, or issues DDL for the wallet table"), so the synchronizer **reports**
progress and `Wallets` **persists** it. That ownership line is why progress lives
here and not in a new indexing-owned table.

### 2.3 Wire

`start` becomes a required property of `CreateWallet`, `ImportWallet` and
`WatchAddress`, encoded as a string sentinel or a non-negative integer:

```json
POST /v1/wallets          { "asset": "btc", "start": "tip" }
POST /v1/wallets/import   { "asset": "btc", "secret": "…", "start": 842000 }
POST /v1/addresses        { "asset": "btc", "address": "bc1…", "start": 0 }
```

```json
"start": { "oneOf": [ { "type": "string", "const": "tip" },
                      { "type": "integer", "format": "int64", "minimum": 0 } ],
           "description": "Native position to begin observing from, or \"tip\"." }
```

`#[serde(untagged)]` with a hand-written `ToSchema`; `apps/api` already
hand-writes schemas (`AssetTag`, `asset.rs:142-145`). Required, so an omitted
`start` is a `400` — the decision's "must" is enforced by the parser, not by a
default.

Config: `ConfiguredWallet.start_position: u64` (`apps/server/src/config.rs:72-81`)
becomes `start: Start`. `deny_unknown_fields` already rejects `start_height`
(`config.rs:244-252`); it now rejects `start_position` too. Pre-release, so the
field is replaced, not aliased (`AGENTS.md:31-34`).

`Wallet` (`apps/api/src/api/contract.rs:25-40`) gains `observation` (§6).

Gates this trips, all of which must be updated in the same commit:
`apps/api/tests/openapi_contract.rs:335-357` (committed document equality),
`.github/workflows/check.yml:47-53` (the same diff in CI),
`apps/api/src/api/mod.rs:301-316` (`additionalProperties: false` on all three
bodies), `apps/api/src/api/mod.rs:355-401` (no chain names, `asset` stays a
patterned string), `openapi_contract.rs:197-303`
(`required_properties_match_what_serde_emits`). No new route, so the
`assert_eq!(ids.len(), 15, …)` at `openapi_contract.rs:133` is unchanged.

---

## 3. The mechanism

### 3.1 `BlockStore::add` cannot write a historical block — exactly how

Three independent refusals, in order:

| # | Guard | Code | Result |
|---|---|---|---|
| 1 | `height == checkpoint.height + 1`, `position > checkpoint.position`, `parent == checkpoint` | `sdk/indexing/src/block.rs:20-46`, via `BlockAddition::new` `block.rs:86` | `CannotConnect "block does not connect to the checkpoint"`, before storage is touched |
| 2a | a journal entry already exists at that height (inside retention) | redb `.../blocks.rs:67-73`; pg `write.rs:87-93` | `CannotConnect "another retained block exists at this height"` |
| 2b | below retention the journal was pruned, so instead `current != expected_checkpoint` | redb `.../blocks.rs:74-80`; pg `write.rs:94-100` | `Conflict "checkpoint changed before block commit"` |
| 3 | even if 1–2 passed, the commit moves the checkpoint **backwards** | redb `.../blocks.rs:97-101`; pg `write.rs:106` + `MOVE_CHECKPOINT` `write.rs:51-59` | the scope loses all forward progress |

The checkpoint CAS forbids out-of-order writes by construction. A backfill
therefore needs a distinct store operation — and `BlockStore` already has three
methods against `linter.toml:140-142`'s cap of 3, so it must be its own port.

### 3.2 Options, re-evaluated for a batched pass

The owner's batching changes the arithmetic: a per-address backfill and a
scope-wide rewind fetch the *same* blocks, so "per-address cost" no longer
separates them. What separates them is what they destroy.

| | Shape | Verdict |
|---|---|---|
| (a) | rewind the checkpoint to the oldest new start, re-index everything | **impossible, then destructive** |
| (b) | one batched backfill pass per scope over `[min(starts), checkpoint]`, writing rows without moving the checkpoint | **recommended** |
| (c) | a second `Service`/scope for the backfill that merges | rejected |
| (d) | refuse below-checkpoint and require an offline rescan (`019` §6) | fallback only, for the unservable range (§7) |

**(a) is impossible as a rewind.** `BlockStore::remove` unwinds *one tip* from the
journal (`sdk/indexing/src/block.rs:180-186`); the journal retains
`reorg_retention` heights (`sdk/indexing/src/sync_policy.rs:12-14`, pruned at
redb `.../blocks.rs:102-108` and pg `WRITE_JOURNAL`'s `$15`
`write.rs:40-49`), and a request below it is
`ReorgTooDeep "rollback journal is not retained"`
(redb `.../blocks.rs:241-247`, pg `revert.rs:102-108`). Rewinding past retention
is a purge, as `019:479-487` already states. As a purge it deletes the scope's
entire history, breaks every open cursor, empties
`GET /v1/wallets/{id}/transactions` while it runs, and re-interprets every block
for every address. That is `019` §5.3's offline `--rescan`, and it is the wrong
tool for "a user created a wallet".

**(c) is rejected.** A second `Service` needs a second checkpoint, a second
journal, and a second CAS domain per scope — a duplicated durable state machine
for a transient job — and its "merge" step is precisely (b)'s idempotent write
plus a bulk copy. Nothing is saved.

### 3.3 The pass

Backfill is a **phase of the existing sync pass**, not a second loop.
`Synchronizer::sync` (`synchronizer.rs:80-129`) becomes:

```
sync(selection):
  guard  = enter()                       # unchanged, synchronizer.rs:212-216
  tip    = source.tip()
  ckpt   = repository.get(Tip)
  plan   = selection.plan(network, ckpt) # unchanged; revision + filters + checkpoint
  reconcile if the stored tip is no longer canonical           # unchanged
  ckpt   = catch_up(tip, ckpt, plan)     # FORWARD phase, unchanged, bounded by batch_size
  back   = backfill(ckpt, plan)          # NEW, bounded by batch_size
  return SyncStatus { …, backfill: back }
```

Forward first, deliberately: the tip is the thing every existing client reads,
and a backfill must never delay it. Both phases are bounded by
`SyncPolicy::batch_size` (`sync_policy.rs:15-16`), so a pass stays bounded as it
does today.

The backfill phase, for one scope:

1. **Pending set.** `plan.pending()` — the addresses in the captured plan whose
   `start_position <= ckpt.position` and whose recorded `done_through` has not
   reached their anchor. Captured in the same snapshot as `filters`, so it is
   revision-consistent with them.
2. **Range.** `lo = min(pending starts, clamped to done_through + 1)`,
   `hi = ckpt`. **The lowest block wins, once, for the whole batch.**
3. **Blocks.** Take `[lo, hi]` from the recent-block cache where it is resident
   (§3.6) and fetch the rest with `source.blocks(lo, cache_lo - 1, remaining)` —
   the same bounded, ordered, validated fetch forward sync uses
   (`synchronizer.rs:256-286`).
4. **Interpret once per block, for all pending addresses at once.**
   `BlockInterpreter::inspect(&block, addresses)` takes a *slice*
   (`sdk/indexing/src/source.rs:57-61`), and
   `SyncPlan::active_addresses(position)` already returns only the addresses
   whose `start_position <= position`
   (`sdk/indexing/src/admission.rs:225-231`). Reuse both verbatim: an address
   whose start is above the batch minimum simply contributes nothing below its
   own start, at no extra cost. One fetch and one `inspect` per block,
   independent of how many addresses are pending.
5. **Write** a chunk of blocks through the new port (§3.4), under a
   `CommitPermit` (§5.4).
6. **Report** `done_through` per address to `Wallets`, which persists it through
   `Registry::advance` and updates the wallet's `Observation` (§6).

### 3.4 The port and its transaction boundaries

```rust
// sdk/indexing/src/backfill.rs — new file
// (observation.rs is 475 lines against linter.toml:70-72's 500-line cap, and
//  block.rs is 572; neither can absorb this.)

/// One already-committed block's facts for addresses that were not selected
/// when it was first committed.
pub struct BackfilledBlock {
    pub block: BlockRef,
    pub transactions: Vec<CanonicalTransaction>,
    pub outputs: OutputChanges,
}

/// One chunk of ascending, already-committed blocks and the checkpoint they
/// were read under.
pub struct BackfillBatch {
    pub network: Network,
    /// The checkpoint observed when the chunk was planned. Every block is at
    /// or below it.
    pub expected_checkpoint: BlockRef,
    /// How many recent heights the journal retains, so spends inside the
    /// window are journalled and spends below it are not (§5.5).
    pub retention: u64,
    pub blocks: Vec<BackfilledBlock>,
}

/// What one chunk did.
pub struct BackfillOutcome {
    /// The highest position durably written.
    pub done_through: BlockPosition,
    /// Required spends whose output the index does not hold: each names an
    /// address whose history begins after the output was created (§5.1).
    pub unknown_spends: Vec<OutputKey>,
}

/// Writes facts into blocks a scope already committed. Never moves the
/// checkpoint, never writes a journal entry of its own.
pub trait BackfillStore: Send + Sync {
    fn backfill<'a>(&'a self, batch: BackfillBatch)
        -> BoxFuture<'a, Result<BackfillOutcome, IndexError>>;
}
```

**One store transaction per chunk**, containing, in order:

| Step | PostgreSQL | redb |
|---|---|---|
| serialise against commits and reverts | `lock_network` then `locked_checkpoint … FOR UPDATE` (`write.rs:135-169`), held for the whole transaction | the single-owner thread plus one conditional `WriteBatch` (`packages/storage/redb/src/backend.rs`) |
| reject a moved checkpoint | observed checkpoint `!= batch.expected_checkpoint` → retryable `Conflict` | `Self::expect(batch, checkpoint_key, Some(observed))` |
| reject a block at or above the tip | `block.position > checkpoint.position` → `InvalidRequest` | idem |
| history | read the `(address, height)` range once, diff, insert only what is missing, `Store` on a field mismatch (§5.2) | `expect(key, Some(existing))` and skip, or `expect(key, None)` and put |
| created outputs | `WRITE_CREATED` as today (`projection.rs:35-44`); a pre-existing row is impossible (§5.2) | `create_outputs` as today (`.../blocks.rs:156-182`) |
| spends | inside retention: `SPEND_OUTPUTS` as today (`projection.rs:47-63`). Below retention: delete the live row with **no** `payments_journal_output` insert (§5.5) | idem, appending `RestoredOutput` to the retained `JournalRecord` only inside retention |
| journal amendment (redb only) | — | append the written history and created-output keys to each affected retained `JournalRecord` (§5.5) |
| epoch | `UPDATE payments_checkpoint SET backfill_epoch = backfill_epoch + 1` (§5.3) | bump the field on the checkpoint record |

The checkpoint is **read and locked but never written**. That is the whole
difference from `write_block`, and it is why the two can share the same lock
without either corrupting the other.

**Why the store cannot verify a historical block's hash.** Below retention the
store holds no block identity at all — "Durable state per network: checkpoint,
history …, live outputs, bounded journal. Nothing else"
(`docs/INDEXING.md:89-91`). So canonicality is verified where it already is: the
synchronizer calls `source.canonical_at(position)` immediately before the write,
exactly as `apply` does today (`synchronizer.rs:299-304`). The pair
*(source canonical now) + (checkpoint unchanged under the lock)* is the same
guarantee forward sync has, minus the parent-linkage check, which is meaningless
for a non-contiguous write.

### 3.5 Coalescing, batching and late arrivals

- The pending set is keyed by **address**, not by wallet, because
  `Wallets::filters` already collapses duplicate addresses to the earliest
  birthday (`sdk/wallets/src/wallets.rs:287-305`). Two wallets on one Ethereum
  account (native + token) share one backfill; each reaches `Complete` when the
  pass passes its own start, so completeness stays per-wallet as `021` §7.4
  requires.
- **A late arrival joins the next pass, not the in-flight one.** Publishing a
  wallet bumps the admission revision (`admission.rs:353-362`), so the running
  plan's next `SyncPlan::begin()` fails with retryable `Conflict`
  (`admission.rs:165-169`), the pass ends, and — because the phase is
  `CatchingUp` — the next pass starts immediately with the new address included.
  Work discarded: at most one chunk.
- **No collection window, no debounce timer.** The revision bump *is* the
  debounce. A timer would delay the common case (one wallet, resolved from cache
  in milliseconds) to marginally cheapen the rare one, and ten wallets created in
  the same second already collapse into one pass because planning plus fetching
  takes longer than a second.
- An address leaves the pending set only when `done_through >= anchor` is
  durable. The anchor is the checkpoint **position** captured when it was
  enqueued, not a live value, so the range cannot chase the tip forever.

### 3.6 The recent-block cache

**What is cached.** The scope's own `I::Block` — the chain's `RawBlock`, which is
already `Clone` and byte-backed:
`sdk/chains/bitcoin/src/native/block/mod.rs:31-36` (enriched `getblock`
verbosity-2 JSON), `sdk/chains/ethereum/src/native/block/mod.rs:32-37` (block
JSON plus one receipt per transaction),
`sdk/chains/solana/src/native/block/mod.rs:34-38` (`getBlock` JSON). Raw bytes,
not parsed blocks: `inspect` is address-set dependent and must re-run per pass,
while the bytes are not. Held as `Arc<I::Block>` so caching costs a refcount, not
a copy.

**Where.** A `Mutex<Recent<I::Block>>` on `Synchronizer`, beside `running`
(`synchronizer.rs:46-53`). Private; nothing in the public API changes.

**Bound: bytes first, blocks second.** Block sizes differ by three orders of
magnitude between chains and within one chain, and `json_rpc`'s
`max_response_bytes` defaults to 64 MiB
(`packages/json-rpc/src/http.rs:124`) — a single enriched Bitcoin block can
approach it. So `SyncPolicy` gains two fields, and both are enforced:

```rust
/// Bytes of recent block evidence kept in memory per scope. `0` disables the
/// cache; nothing but latency changes.
pub recent_bytes: usize,        // default 64 MiB
/// Hard cap on cached blocks, whatever their size.
pub recent_blocks: u64,         // default: reorg_retention
```

`recent_blocks` is tied to `reorg_retention`, not to `minimum_confirmations`:
below retention a reorg is unrecoverable anyway (`synchronizer.rs:454-460`), so
caching deeper buys nothing a reorg could invalidate, while
`minimum_confirmations` is a read-time display threshold
(`sync_policy.rs:9-11`, `indexer.rs:111-121`) with no bearing on how far back an
address may be added.

**Honest window, per chain, at 64 MiB:**

| Chain | Typical evidence per block | Blocks cached | Wall-clock window |
|---|---|---|---|
| Bitcoin | 5–15 MiB (verbosity-2 JSON with resolved prevouts) | ~4–12 | 40–120 min |
| Ethereum | 0.5–3 MiB (block + receipts; `021` §6 traces would multiply this) | ~20–100 | 4–20 min |
| Solana | 1–10 MiB, ~2.5 blocks/s | ~6–60 | **2–25 s** |

So on Solana the cache serves a wallet used immediately after creation and
little else. That is stated rather than hidden: the cache is a latency
optimisation whose miss path is a bounded fetch, never a rescan.

**Eviction.** Oldest-first when either bound is exceeded, on insert.

**Reorg.** `reconcile` walks tips back (`synchronizer.rs:391-419`) calling
`CommitPermit::persist` per removed block (`admission.rs:294-299`). The cache
truncates to the restored checkpoint at the same point. Because entries are only
ever inserted for blocks *this process committed in this run*, and truncated on
every revert, **cache-resident implies canonical as of the current checkpoint** —
no hash re-verification is needed for a cache hit.

**Restart.** The cache is in-process and starts empty. Consequence: an address
added just after a restart is resolved by a fetched backfill over at most the
same window, i.e. one small fetch. Nothing durable depends on the cache, and its
absence changes no result — only latency.

**The "close to tip" rule, exactly.** There is no tunable closeness threshold.
The cache contributes the contiguous suffix it holds:

```
cache_lo = cache.oldest_position          # None when empty
fetched  = [lo, min(hi, cache_lo - 1)]    # empty when lo >= cache_lo
cached   = [max(lo, cache_lo), hi]
```

A wallet created at the tip has `lo = ckpt.position`, `fetched` is empty, and
the backfill completes in one pass with **no refetch and no reindex** — the
owner's requirement, met by a one-line predicate.

---

## 4. Worked example

Scope at checkpoint 845 112. Three addresses are added in the same second:
`A` at `At(845_000)`, `B` at `At(844_500)`, `C` at `Tip`.

| Step | What happens |
|---|---|
| add | all three take a `PublicationPermit` in turn; `C` resolves to 845 113 and is not pending; `A` and `B` enter the pending set with anchor 845 112 |
| pass *n* | forward phase applies 845 113…845 212 for `A`, `B`, `C` (all three are in `filters` already); backfill phase computes `lo = 844_500`, `hi = 845_112` |
| blocks | the cache holds 845 105…845 112 (8 blocks); 844 500…845 104 is fetched, 100 per chunk |
| interpret | 844 500…844 999: `active_addresses` yields `[B]`. 845 000…845 112: yields `[A, B]`. One fetch, one `inspect` per block |
| write | 7 chunks; each bumps `backfill_epoch` once, so open history cursors conflict at most 7 times |
| finish | `Registry::advance` records `done_through = 845_112` for both; both leave the pending set; both report `Complete { start }` |

Cost: 613 block fetches and 613 interpretations — the same as for one address.
Adding seven more addresses at starts above 844 500 adds zero fetches.

---

## 5. The hard cases

### 5.1 Bitcoin UTXO state

**Case 1 — funded at 10, spent at 50, both inside `[start, checkpoint]`.** The
backfill interprets in ascending order, so block 10 produces
`OutputChanges.created` for the address and block 50 produces
`OutputChanges.spent`. Within one chunk, or across chunks in order, the create
lands before the spend, so `spend_outputs` finds the output and deletes it
(`redb .../blocks.rs:184-215`, `pg projection.rs:205-228`). **Both appear, in
order.** The spend was invisible to the original commit of block 50 — at that
time the address was not in `selected`, so `interpreter.rs:177-181` classified it
as a tracked spend (P2WPKH/P2TR) or dropped it entirely, and tracked spends are a
no-op on a missing output (`redb .../blocks.rs:197-204`,
`pg repository_contract.rs:1099`). So there is nothing to undo.

**Case 2 — created *before* `start`, spent inside or above the range.** The
backfill (or, for a spend above the checkpoint, forward sync) sees the address as
selected, emits a **required** spend, and the output is not there. Today that is
the terminal `InvalidBlock` of §1.3.

> **The required-spend rule must change, and this is the single most important
> correctness change in this document.**

Required-ness was a corruption detector, written when every indexed address was
assumed to have been watched since its first output. Birthdays make that
assumption false, and §1.3 shows it is false *today*, with no backfill involved.
The honest rule:

| Condition | Today | After |
|---|---|---|
| required spend, output absent | `InvalidBlock`, terminal, stops the chain | the key is returned in `unknown_spends`; the live-output table is unchanged; the wallet becomes `Observation::Incomplete { since }` (§6); indexing continues |
| required spend, output present under **another** address | passes (the key is address-qualified, so it simply misses) | unchanged — still a miss, still reported |
| tracked spend, output absent | silent no-op | unchanged |

`BlockOutcome::Applied` (`sdk/indexing/src/block.rs:190-197`) gains the missed
keys so forward sync reports them the same way the backfill does. Both backends
delete the `"block spends an unknown indexed output"` error
(`redb .../blocks.rs:196-203`, `pg projection.rs:216-222`); the existing tests
(`redb .../lib.rs:441-474`,
`pg tests/repository_contract.rs:1099-1140`) invert.

This is strictly better than today on every axis: it removes a live terminal
failure, it produces the fact `021` §5.2 wants (a provable incompleteness signal
rather than a guess from a negative fold), and it means a caller who adds a
funded address at `Tip` gets a wallet that says so instead of a quarantined chain.

**Case 3 — a spend inside the range whose output is created inside the range but
in a chunk not yet written.** Impossible: chunks are ascending and each is one
transaction, so a create at height `h` is durable before any chunk containing
`h' > h` begins.

### 5.2 Idempotent history writes

Per §1.4 the new address may already have rows in `[start, checkpoint]`. Both
backends currently refuse (`Conflict`, retryable), which would loop forever.

The backfill's history write is **insert-if-absent, verify-if-present**:

| | Read | Write | Mismatch |
|---|---|---|---|
| PostgreSQL | one range query over `payments_history` + `payments_movement` for `(address, height ∈ chunk)`, inside the same transaction | insert only the missing rows; the PK `(chain, network, address, height, transaction_id)` (`schema.rs:171`) makes the diff exact | terminal `Store "backfilled transaction disagrees with the stored one"` |
| redb | the same `get` the write path already does (`.../blocks.rs:136`) | `expect(key, Some(existing))` and skip, or `expect(key, None)` and put | idem |

A mismatch is real corruption: the same `(address, height, transaction_id)`
cannot have two different statuses, blocks, fees or movement sets. It must be
loud, and terminal, and it must not be confused with a benign duplicate.

**Created outputs cannot pre-exist.** Bitcoin creates an output only for a
selected address (`interpreter.rs:184-192`), and the address was not selected, so
`create_outputs`' "created output already exists" (`redb .../blocks.rs:164-171`)
is unreachable on the backfill path. Left as-is.

### 5.3 History ordering and cursors

Pages are keyset-ordered ascending by `(height, transaction_id)` and bound to the
first page's checkpoint (`sdk/indexing/src/history.rs:39-44`,
`indexer.rs:39-52`, `redb .../transactions.rs:22-32`, `pg read.rs:81-96`). A
backfill inserts rows **behind** an open cursor. The checkpoint did not change,
so nothing conflicts, and the paginating client silently never sees them — the
page set it assembles mixes two completeness states with no signal.

**A backfill is, for pagination, a reorg. Make it say so.** Bind the cursor to
one more value:

```rust
// sdk/indexing/src/history.rs
/// What a page was read at: the checkpoint, and how many backfills have
/// committed under it. Either changing invalidates an open cursor.
pub struct PageAnchor { pub checkpoint: Option<BlockRef>, pub epoch: u64 }

pub struct HistoryCursor { pub at: PageAnchor, pub position: HistoryPosition }
pub struct OutputCursor  { pub at: PageAnchor, pub position: Vec<u8> }
```

Both backends compare the whole anchor where they compare the checkpoint today
(`redb .../transactions.rs:23-32` and `.../outputs.rs:15-24`,
`pg read.rs:82-86` and `:95-96`), and `Index::history`'s existing check
(`indexer.rs:44-52`) is unchanged in shape. The client sees the error it already
handles:

```
IndexError { kind: Conflict, message: "history changed between pages; restart from the first page", retryable: true }
```

Costs: one integer per scope — `payments_checkpoint.backfill_epoch bigint NOT NULL
DEFAULT 0` and a field on redb's `BlockRecord`
(`sdk/indexing/redb/src/record.rs:42-48`). **No schema change on the wire**: the
API cursor is a base64 JSON blob clients treat as opaque
(`apps/api/src/api/cursor.rs:5-13,22-30`), so it simply gains an `epoch` field.
Because that struct is `deny_unknown_fields` and the new field is required, every
cursor issued before the deploy decodes as a `400` once — the same class of
one-time invalidation a checkpoint move already causes. Steady-state churn is
bounded: one invalidation per committed chunk, and chunks exist only while
addresses are pending.

Rejected: descending pages (changes the documented order for everyone), and
"only ever append above the cursor" (which is the feature being built).

### 5.4 Admission

**A backfill chunk takes a `CommitPermit`, never a `PublicationPermit`.**

```rust
let mut permit = plan.begin()?;   // sdk/indexing/src/admission.rs:252-257
permit.start();
let outcome = self.repository.backfill(batch).await?;
permit.complete(plan.checkpoint().cloned())?;   // checkpoint unchanged
```

Consequences, each from the existing code:

- It **cannot run concurrently with forward sync on the same scope.** `begin`
  refuses while a commit or a publication is in progress
  (`admission.rs:160-164`), and the phases live in one pass behind
  `Synchronizer::running` (`synchronizer.rs:212-216`) anyway. **Interleaved, never
  concurrent** — forward chunk, backfill chunk, forward chunk.
- It cannot starve forward sync: both phases are bounded by `batch_size`.
- It is **independent across scopes**: there is one `NetworkAdmission` per network
  (`sdk/wallets/src/wallets.rs:123`), so a Bitcoin backfill never delays Ethereum.
- A wallet published mid-backfill makes the next `begin()` conflict
  (`admission.rs:165-169`) — which is exactly the coalescing mechanism of §3.5.
- A permit dropped after `start()` without `complete` sets `recovery`
  (`admission.rs:318-331`), forcing a repository reload before the next plan.
  Correct for a backfill too: the chunk's outcome is unknown, and idempotency
  (§5.2) makes the redo safe.

In-process permits do not bind another PostgreSQL process, which is why the chunk
also takes `lock_network` + `FOR UPDATE` on the checkpoint row (§3.4) — the same
pair `write_block` uses (`write.rs:70-71`).

### 5.5 Reorgs, and a backfill spanning the retention window

The anchor is a `BlockRef`, not a bare height. A reorg during a backfill moves
the checkpoint *down* via `reconcile`; the next chunk's CAS on
`expected_checkpoint` fails with retryable `Conflict`; the pass ends; the next
pass re-anchors and clamps `done_through` to `min(done_through, new checkpoint)`.

What happens to rows the backfill already wrote at a height that is then
reverted? **The two backends disagree today, and one of them is wrong:**

| Backend | Revert mechanism | Effect on backfilled rows |
|---|---|---|
| PostgreSQL | delete by height predicate — `DELETE FROM payments_history WHERE chain = $1 AND network = $2 AND height = $3` (`revert.rs:24-25`), same for movements (`:21-22`) and created outputs (`:27-28`) | **removed correctly, for free** |
| redb | delete only the keys listed in `JournalRecord.history_keys` / `remove_output_keys` (`.../blocks.rs:292-341`, record `record.rs:134-141`) | **survive the revert, orphaned** |

So redb's backfill chunk must **append its written history and created-output keys
to the `JournalRecord` of each affected retained height**, in the same conditional
batch. Postgres needs nothing.

**The retention boundary is a hard constraint on spends, in PostgreSQL:**

```sql
-- sdk/indexing/postgres/migrations/0001_init.sql:215-216
FOREIGN KEY (chain, network, height)
    REFERENCES payments_journal (chain, network, height) ON DELETE CASCADE
```

A backfilled spend at a height below `checkpoint.height - reorg_retention` has no
`payments_journal` row to reference — it was pruned by `WRITE_JOURNAL`'s folded
`DELETE` (`write.rs:40-49`) — so inserting the `payments_journal_output` row
**violates the foreign key**. Hence the split in §3.4:

| Where the spend is | PostgreSQL | redb | Reversible? |
|---|---|---|---|
| height > `checkpoint.height - retention` | `SPEND_OUTPUTS` as today, journal row exists | append a `RestoredOutput` to the retained `JournalRecord` | yes |
| height <= `checkpoint.height - retention` | delete the live row, **no** `payments_journal_output` insert | delete the live row, no `RestoredOutput` | no — and it need not be |

Irreversibility below retention is not a regression: a reorg that deep is already
`ReorgTooDeep` (`synchronizer.rs:454-460`), whose only remedy is `019` §5.3's
purge and rescan, which discards the journal with everything else.

A backfill spanning the window therefore writes journalled rows for its recent
tail and unjournalled rows for its body, with the boundary recomputed per chunk
from `BackfillBatch::retention`.

### 5.6 Restart and durable progress

| Question | Answer |
|---|---|
| Where is progress stored? | the registry row: `payments_wallet` gains `start_position bigint NOT NULL`, `backfill_anchor bigint NULL`, `done_through bigint NULL`; redb's `RegistrationRecord` (`record.rs:10-16`) gains the two nullable fields |
| Why there? | it is the only durable per-address state, it is what `restore` already reads (`persistence.rs:119-136`), and it is application-owned — so `Wallets` writes it through `Registry::advance` and the synchronizer never touches `payments_wallet` (`AGENTS.md`) |
| Semantics | every block in `[start_position, done_through]` has been inspected for this address. `NULL` = not started |
| Resume | `restore` re-enqueues any row with `done_through < backfill_anchor`, with an effective `lo` of `done_through + 1`. A completed prefix is never rescanned |
| Partially applied chunk | impossible — one chunk is one store transaction. A chunk whose *outcome* is unknown (permit dropped, `admission.rs:318-331`) is simply redone; §5.2's idempotency makes the redo a no-op |
| What does the wallet report meanwhile? | `Observation::Backfilling { start, done_through, anchor }` (§6) |
| Config wallets | `ConfiguredWallet` is not persisted to the registry (`wallets.rs:165-167`) — it is re-imported from `secret_env` every boot (`apps/server/src/main.rs:59-61`). So it has no durable progress and re-backfills from `start` on every restart. **Fix in the same change: `import` persists to the registry when the factory has one**, exactly like `adopt`, so a configured wallet resumes like any other. Without this, a config wallet with `"start": 0` rescans the chain on every restart |

### 5.7 When one address in the batch fails

| Failure | Attributable? | Effect |
|---|---|---|
| source cannot serve a block in the range (pruned node, ledger retention) | to the *range*, not one address — the fetch is shared | the range narrows to the highest servable position; every address whose start is below it becomes `Observation::Unavailable { earliest }`; the rest continue. The scope does **not** quarantine (§7) |
| required spend with no output (§5.1) | yes — the key names its owner | that one wallet becomes `Incomplete { since }`; the chunk commits; the batch continues |
| history mismatch (§5.2) | yes | terminal `Store`; chunk aborts; scope quarantines. This is corruption, not a completeness gap |
| checkpoint moved / revision bumped | no | retryable `Conflict`; chunk aborts; nothing written; next pass redoes it |
| store unavailable | no | chunk aborts atomically; retried per `019` §5.2 |

The rule: **a shared-resource failure narrows the range, a per-address fact marks
one wallet, and only store corruption stops the scope.** One address with an
impossible start must never block the other nine.

---

## 6. Wallet state and completeness

```rust
// sdk/wallets/src/wallet.rs
/// How completely this wallet's address has been observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation {
    /// Every block from `start` is indexed for this address.
    Complete { start: BlockPosition },
    /// `[start, done_through]` is indexed; `(done_through, anchor]` is not yet.
    Backfilling { start: BlockPosition, done_through: Option<BlockPosition>, anchor: BlockPosition },
    /// The source cannot serve blocks from `start`; nothing below `earliest`
    /// will ever be indexed.
    Unavailable { start: BlockPosition, earliest: BlockPosition },
    /// A spend of an output created before `start` was seen at `since`:
    /// balances and output listings for this address are provably short.
    Incomplete { since: BlockHeight },
}
```

A closed four-variant vocabulary, so it is an enum, as
`linter.toml`'s `string-backed-finite-state` requires.

`WalletInfo` (`sdk/wallets/src/wallets.rs:24-34`) gains `observation`, and so does
the wire `Wallet` (`apps/api/src/api/contract.rs:25-40`), which is the single
response body for create, import, watch, read and list:

```json
{ "id": "…", "asset": "btc", "network": {"chain":"bitcoin","name":"mainnet"},
  "address": "bc1…", "watch_only": false,
  "observation": { "state": "backfilling", "start": 842000,
                   "done_through": 842310, "anchor": 845112 } }
```

`Complete { start }` is `021` §8.1's `from_birthday`, not its `complete`:
completeness remains a claim about `start` that the index cannot prove on its own
(`021` §7.1). `Incomplete` is the honest form of `021`'s `overdrawn` — proved by a
missed spend rather than inferred from a negative fold.

### Balance mid-backfill

| Option | Verdict |
|---|---|
| error until complete | rejected: breaks every client for minutes, and the forward-only number is genuinely useful and is what the API returns today |
| forward-only, unlabelled | rejected: this is the lie `021` exists to remove |
| **partial, labelled** | **recommended** |

`GET /v1/wallets/{id}/balance` returns `200` with the amount folded over what is
indexed, plus the same `observation` object. A caller that needs certainty checks
`observation.state == "complete"`; a caller that wants a number gets one. And
because a mid-backfill fold may legitimately go negative, `Backfilling` suppresses
`021` §5.2's overdraw alarm: the endpoint reports `0` with the label rather than
raising. `GET /v1/wallets/{id}/transactions` carries `observation` beside its
existing `checkpoint` (`apps/api/src/api/history.rs:312-325`).

---

## 7. Cost controls

| Control | Shape | Where it fires |
|---|---|---|
| **Maximum scan** | `chains.<chain>.max_backfill_blocks: u64` (the name `021` §7.5 already uses) | checked in `Wallets::store` under the publication permit, against the checkpoint just read. Exceeded → `ErrorKind::Unsupported` → `422` naming the earliest acceptable start. A synchronous refusal, not a background failure |
| **Availability floor** | a one-method port beside `BlockSource` (which is at `linter.toml:140-142`'s 3-method cap): `fn earliest<'a>(&'a self) -> BoxFuture<'a, Result<Option<BlockPosition>, SourceError>>` | Solana `getFirstAvailableBlock` (`sdk/chains/solana/src/native/rpc/blocks.rs:80`), Bitcoin's existing pruned-node check (`sdk/chains/bitcoin/src/native/rpc/node.rs:67-72`), Ethereum's earliest servable block. A chain that does not implement it reports no floor and fails at fetch time instead |
| **Progress** | `SyncStatus` gains `backfill: Option<BackfillStatus { lo, hi, done_through, pending }>`; `019`'s `NetworkHealth` carries the same, so `GET /v1/status` shows it. Per wallet, `Observation::Backfilling` | |
| **Cancellation** | `019`'s `SyncControl` gains `cancel_backfill(&network)`: every pending address becomes `Unavailable { earliest: anchor }` and the range empties | a runaway backfill from a mistyped start must be stoppable without a restart |

**Pruned node.** The fetch fails with a `SourceError`. The range narrows to the
highest servable position, the affected addresses become `Unavailable { earliest }`,
and the scope stays `Ready`/`CatchingUp`. It does **not** quarantine: a pruned
node is a property of the *request*, not of the chain, and a retryable `Source`
error on a range that can never be served is precisely the busy-wait `019` §5.3
warns against. The operator sees which wallets are affected and what the earliest
acceptable start is, on `GET /v1/wallets` and `GET /v1/status`.

---

## 8. Interaction with 019 and 021

| Document | Relationship |
|---|---|
| `019` §2 (per-chain isolation) | **prerequisite.** Backfill work is per scope; without isolation one scope's backfill failure would still stall the others through `Composer::sync`'s `?` |
| `019` §3 (`SyncMonitor`) | a backfill failure reports as a `NetworkEvent` **without changing `NetworkState`**: the pass returns `Ok` with whatever forward progress it made, and the backfill fault rides on `NetworkHealth.backfill`. Only a terminal `Store` failure (§5.7) quarantines, because that is the store itself failing. This is how "a backfill failure must not halt the chain's forward sync" is enforced rather than promised |
| `019` §5.3 (`purge_network` / `--rescan`) | **still needed**, for two jobs backfill cannot do: recover from `ReorgTooDeep` (`synchronizer.rs:454-460`), and apply *new extraction rules* retroactively — backfill re-interprets blocks only for newly pending addresses, never for existing ones, which is exactly `021` §8.3's "historical rows do not gain new causes retroactively" |
| `019` §6.3 (refuse a birthday below the checkpoint) | **superseded.** Accept and backfill. The refusal survives only as §7's unservable-range case, with the same `already_registered_at` spirit: a restored address keeps its stored start and is never re-refused |
| `019` §6.2 (iv) ("per-address backfill without rewinding the scope" — rejected) | its three grounds are answered: UTXO state by §5.1's rule change, cursors by §5.3's epoch, and the duplicated synchronizer by making backfill a *phase* of the existing pass that reuses `fetch_blocks`, `inspect`, `active_addresses` and `CommitPermit` verbatim |
| `021` (derived balance) | **backfill is its precondition.** `Complete { start }` is `021` §7.1's condition (i); `Incomplete { since }` is the provable incompleteness its negative-fold check would otherwise have to infer; `Backfilling` is why a mid-backfill fold must not raise `overdrawn` |
| `012` (asset-scoped history + cursors) | `HistoryCursor` grows `PageAnchor`; its cross-asset reuse property (`012` §3.4) is unaffected, since the position half is unchanged |

---

## 9. Migration

### 9.1 By crate

| Crate | Change | Files |
|---|---|---|
| `indexing` | `Start`; `BackfillStore` + `BackfillBatch`/`BackfilledBlock`/`BackfillOutcome`; `PageAnchor` on both cursors; `BlockOutcome::Applied` carries `unknown_spends`; `SyncPolicy.recent_bytes`/`recent_blocks`; `SyncStatus.backfill`; `Registry::advance`; backfill phase + recent cache in `Synchronizer`; `SyncPlan::pending` | `src/backfill.rs` (new), `src/service.rs`, `src/history.rs`, `src/output.rs`, `src/block.rs`, `src/sync_policy.rs`, `src/synchronizer.rs`, `src/admission.rs`, `src/registry.rs`, `src/lib.rs` |
| `indexing-redb` | `BackfillStore`; idempotent history write; journal amendment; retention split on spends; `backfill_epoch` on `BlockRecord`; `RegistrationRecord` gains `backfill_anchor`/`done_through`; `Registry::advance`; delete the unknown-spend error | `src/repository/backfill.rs` (new), `.../blocks.rs`, `.../transactions.rs`, `.../outputs.rs`, `.../registry.rs`, `src/record.rs` |
| `indexing-postgres` | idem, plus `0003_backfill.sql` | `src/backfill.rs` (new), `src/projection.rs`, `src/read.rs`, `src/registry.rs`, `src/revert.rs`, `src/schema.rs` constants |
| `wallets` | `Start` on five methods; `import`/`insert` become `&self` and persist; one `store()` path; `Observation`; pending set; progress persistence | `src/wallets.rs`, `src/wallets/persistence.rs`, `src/wallet.rs`, `src/selection.rs` |
| `payment-api` | `start` on three bodies; `observation` on `Wallet`, balance and history payloads | `src/api/wallet.rs`, `src/api/address.rs`, `src/api/contract.rs`, `src/api/history.rs`, `src/api/cursor.rs`, `src/api/sync.rs` |
| `apps/server` | `ConfiguredWallet.start`; `max_backfill_blocks`; monitor wiring for backfill progress | `src/config.rs`, `src/chains.rs`, `src/readiness.rs` |
| chains ×3 | the availability floor port | `src/source.rs` per chain |
| docs | `docs/API.md:64-67,83,100-107,121-123`, `docs/INDEXING.md:95-112,165-172`, `ARCHITECTURE.md:550-553`, `docs/openapi.json` | |

### 9.2 Schema

`0003_backfill.sql` — additive, ordered after `0002_wallet_asset.sql`;
`0001_init.sql` is frozen by checksum (`docs/INDEXING.md:158-163`):

```sql
BEGIN;
ALTER TABLE payments_checkpoint ADD COLUMN backfill_epoch bigint NOT NULL DEFAULT 0;
ALTER TABLE payments_wallet     ADD COLUMN backfill_anchor bigint;
ALTER TABLE payments_wallet     ADD COLUMN done_through    bigint;
COMMIT;
```

`payments_wallet.start_height` already holds a **position**, not a height
(`sdk/indexing/postgres/src/registry.rs:22,73`; the misnomer dates to
`0001_init.sql:222-223`). Rename it to `start_position` in the same migration —
pre-release, so the column is renamed rather than aliased. Every rename lands in
`schema.rs`'s `EXPECTED_COLUMNS` / `EXPECTED_INDEXES` (`:70-185`), whose
`initializer_matches_validation` test parses the `.sql` and fails until they agree.

redb needs no DDL: collections are key-tag prefixes over two physical tables, and
both changed records are `bincode` structs gaining fields. Existing files still
decode only if the fields are added at the end with defaults; if that is not
guaranteed by the codec version in use, the redb file is rebuilt — acceptable
pre-release, and `docs/INDEXING.md:142` already says the file is created on first
open.

### 9.3 What breaks for existing callers

| Surface | Break |
|---|---|
| `POST /v1/wallets`, `/import`, `/addresses` | **`start` is now required.** A body without it is `400`. No alias, no default (`AGENTS.md:31-34`) |
| config `wallets[]` | `start_position` → `start`; the old name is `deny_unknown_fields`-rejected |
| `Wallets::{generate,adopt,watch,import,insert}` | all take `Start`; `import`/`insert` take `&self` |
| `HistoryCursor` / `OutputCursor` | field `checkpoint` → `at: PageAnchor`. Opaque on the wire, so no client change; every in-flight cursor at deploy time conflicts once |
| `BlockOutcome::Applied` | gains a payload; no longer `Copy` |
| `"block spends an unknown indexed output"` | the error is deleted. `apps/server/tests/acceptance.rs` and both repository contracts invert |
| `Registry` | 3 methods; both backends implement `advance` |

### 9.4 Tests

| Test | Where | Asserts |
|---|---|---|
| `a_start_below_the_checkpoint_is_backfilled_not_discarded` | `sdk/indexing/tests/service_contract.rs` | the §1.2 defect |
| `one_pass_backfills_every_pending_address_with_one_fetch_per_block` | idem | §3.3: block fetches == range length for 1 and for 10 addresses |
| `an_address_started_above_the_batch_minimum_matches_only_from_its_own_start` | idem | §3.3 step 4 |
| `an_address_added_mid_pass_joins_the_next_pass` | idem | §3.5 |
| `a_backfill_never_moves_the_checkpoint` | both repository contracts | §3.4 |
| `a_funded_output_spent_before_and_after_the_range_both_resolve` | both | §5.1 cases 1 and 2 |
| `a_spend_of_an_unknown_output_reports_instead_of_failing` | both | §5.1 — replaces `redb lib.rs:441-474` and `pg repository_contract.rs:1099-1140` |
| `a_backfilled_row_that_already_exists_is_accepted_once_and_verified` | both | §5.2 |
| `a_backfilled_row_that_disagrees_is_terminal` | both | §5.2 |
| `a_backfill_invalidates_an_open_history_cursor` | both + `sdk/indexing/src/indexer.rs` | §5.3 |
| `a_backfill_chunk_and_a_forward_commit_never_overlap` | `sdk/indexing/src/admission.rs` | §5.4 |
| `a_reorg_removes_backfilled_rows_on_both_backends` | both | §5.5 — this currently fails on redb |
| `a_spend_below_retention_writes_no_journal_output` | `pg tests/repository_contract.rs` | §5.5, the FK constraint |
| `backfill_progress_survives_a_restart` | `sdk/wallets/tests/` + `apps/server/tests/redb_api.rs` | §5.6 |
| `a_cached_tip_range_is_backfilled_without_a_fetch` | `sdk/indexing/tests/service_contract.rs` | §3.6 |
| `a_reorg_truncates_the_recent_cache` | idem | §3.6 |
| `an_unservable_start_marks_the_wallet_not_the_scope` | idem + `apps/api/tests/route_contract.rs` | §5.7, §7 |
| `wallet_payloads_carry_their_observation` | `apps/api/tests/{route,openapi}_contract.rs` | §6 |
| `start_is_required_on_every_add_route` | `apps/api/tests/route_contract.rs` | §2.3 |

### 9.5 Commit order, every gate green after each

| # | Commit | Effort |
|---|---|---|
| 1 | **Landed.** `refactor(indexing): report an unknown spend instead of failing the chain` — delete the terminal error in both backends, carry `unknown_spends` on `BlockOutcome::Applied`, surface the pass total on `SyncStatus::unknown_spends`, and pin the rule for both backends with `indexing::testing::SpendContract`. The two tests that asserted the error were repointed at the conflicts that remain (a re-created live output), so the atomicity they guarded is still checked. **Independently shippable and independently valuable: it removes a live production quarantine (§1.3) with no new feature attached.** | **S** |
| 2 | `feat(indexing): anchor a page to its checkpoint and backfill epoch` — `PageAnchor`, `backfill_epoch` column and record field, `0003_backfill.sql`, both backends, cursor tests. Epoch never increments yet, so behaviour is identical | **S** |
| 3 | `feat(indexing): add the BackfillStore port` — types, idempotent history write, retention split on spends, redb journal amendment, contract scenarios in both backends. Unwired | **M** |
| 4 | `feat(wallets): require a start point on every added address` — `Start`, five signatures, `import`/`insert` to `&self`, one `store()` path, `import` persists, `Registry::advance`, registry columns | **M** |
| 5 | `feat(indexing): backfill pending addresses in one batched pass` — the phase, pending set, range, chunking, progress reporting, `SyncStatus.backfill` | **L** |
| 6 | `feat(indexing): keep recent block evidence for tip-adjacent addresses` — `Recent`, byte budget, reorg truncation, cache-hit predicate | **M** |
| 7 | `feat(wallets): report how completely an address has been observed` — `Observation`, propagation to `WalletInfo` | **S** |
| 8 | `feat(api): require a start point and report wallet observation` — three bodies, `Wallet.observation`, balance and history payloads, `docs/openapi.json`, `docs/API.md` | **M** |
| 9 | `feat(server): bound and cancel a backfill` — `max_backfill_blocks`, the availability floor port on three chains, `SyncControl::cancel_backfill`, monitor wiring | **M** |

Steps 1–3 change no observable behaviour and can land while `019` is still in
flight. Step 4 is the wire break and should land with step 8 in one release.

---

## 10. Verdict

| Piece | Verdict | Effort |
|---|---|---|
| Mandatory `Start` on every add path, wire and config | **Build** | M |
| Unknown-spend error → reported fact (§5.1) | **Built** (commit 1) | S |
| Batched backfill pass, one per scope (§3.3) | **Build** | L |
| `BackfillStore` on both backends, idempotent + retention-split (§3.4, §5.2, §5.5) | **Build** | M |
| `PageAnchor` cursor epoch (§5.3) | **Build** | S |
| Recent-block cache (§3.6) | **Build** | M |
| `Observation` state model and its wire fields (§6) | **Build** | S |
| Cost bound, availability floor, cancellation (§7) | **Build** | M |
| Rewind-and-reindex (option a) | **Refuse** | — |
| Second `Service` instance (option c) | **Refuse** | — |
| `019` §6.3's blanket refusal | **Superseded** | — |
| `019` §5.3 `purge_network` / `--rescan` | **Keep**, for `ReorgTooDeep` and retroactive extraction rules | — |

**First commit: step 1.** It is three files, it deletes an error rather than
adding a mechanism, it fixes a failure that is reachable today with no new feature
(`POST /v1/addresses` on any funded Bitcoin address), and every later step depends
on the rule it establishes — that a spend the index cannot account for is a
completeness fact about one wallet, never a verdict on the chain.

## 11. What cannot be made to work

| Case | Constraint |
|---|---|
| Reversing a backfilled spend below `checkpoint.height - reorg_retention` | `payments_journal_output`'s FK to `payments_journal` (`0001_init.sql:215-216`) and the pruned journal itself. Below retention nothing is reversible, which is already the scope's contract (`019:479-487`) |
| Verifying a historical block's hash from the store | the store keeps no block identity below retention (`docs/INDEXING.md:89-91`). Canonicality is verified against the source at write time instead (§3.4) |
| Proving a wallet complete from the index alone | `021` §7.1: completeness is a claim about `start`, never a property of the rows. `Complete { start }` names the claim; it does not prove the balance at `start - 1` was zero |
| Deciding required-vs-tracked in the interpreter | a Bitcoin prevout carries value and address, not the creating block's height (`sdk/chains/bitcoin/src/native/prevout.rs`), so the interpreter cannot know whether the index should hold the output. The store decides, after the lookup it already performs (§5.1) |
| Serving a start below a pruned node's floor | no design fixes missing data. The wallet is `Unavailable { earliest }` and says so (§7) |
| Surviving a restart with a warm cache | the cache is in-process by construction. The miss costs one bounded fetch, never a rescan (§3.6) |
