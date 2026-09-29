# 034: Repository ownership — the concrete schema and the boundary

> "Present the concrete schema and ownership changes before changing
> persistence. Identify conflicts with existing contracts explicitly."

This is that step, and nothing else: no source, no `Cargo.toml`, no
`linter.toml`, no `scripts/`. It answers what the tables become, what leaves
`sdk/indexing`, where the specification contradicts a contract that landed in
the last few hours, and what each step costs.

Baseline, measured on `refactor/chain-unification`:
`cargo test --locked --workspace --no-fail-fast` — **1151 passed, 0 failed, 9
ignored**, all gates green, `docs/openapi.json` sha `dd1a2163`. Every line
number below is from that tree. Where the specification asks for something the
tree already does, this document says so and plans no work; there are **six**
such items and they are collected in §1.3.

---

## 0. The three things worth knowing before the schema

**One: this is greenfield, not a migration.** The durable-format exception was
revoked in writing, and the revocation is the reason the current schema is a
list of ordered steps at all. §2 quotes it.

**Two: the spec's largest item is a direction change, not a table.** Balance is
folded at read time over rows the store already holds
(`sdk/indexing/src/balance.rs:153-268`). The spec wants it accumulated on the
`watch` row and written with the movements. That is derive-on-read →
maintain-on-write, and it collides with a fact the tree already depends on:
**history rows are written for every movement endpoint, watched or not**
(`sdk/indexing/src/storage/sqlite/blocks.rs:139-145`). A persisted total on a
row that only exists for watched observations has nothing to accumulate into
when the counterparty's movement lands, and nothing to reconcile against when
that counterparty is later watched. §5.3 and §5.7 are that problem; it is the
one item in this document where the recommendation is to decline part of the
spec.

**Three: the ownership move is mostly a move that already happened once.**
`History` left the core: `ConfirmedHistory` is the repository's query,
`ObservedHistory` joins it to the overlay, and every consumer holds
`Arc<dyn History>` (`sdk/wallets/src/composition.rs:234-251`,
`apps/api/src/api/mod.rs:123`). Balance is the same move with the same shape,
and the target is already written — `Histories` is the pattern. Outputs are
**not** the same move, because the output read carries a gate the repository
cannot take. §4.3.

---

## 1. Diagnosis: where persistence and queries live today

### 1.1 The ports, measured

`sdk/indexing/src/storage.rs:39-47` already states the split the spec asks for:

```rust
pub trait Repository: WatchStore + IndexStore {}
```

and its module doc (`storage.rs:1-27`) already calls the rest "consumer
capabilities". So the *declaration* is done. What is not done is the
*implementation*: the three capabilities are implemented inside `sdk/indexing`
for one store, and two of the three reads are `impl`s on `Service` itself.

| Thing | Where | Production lines | Owner the spec wants |
|---|---|---|---|
| `WatchStore` (4 methods) | `storage/watch.rs:132-247` | 278 of 349 | indexing (required) — **stays** |
| `IndexStore` (2 methods) | `storage/commit.rs:362-457` | 473 of 707 | indexing (required) — **stays** |
| `BalanceStore`, `OutputStore`, `Purging` | `storage/reads.rs:72-131` | 154 | repository |
| `ConfirmedHistory` | `history.rs:249-257` | 310 of 342 | repository |
| `Balances` + `Ledger` + `Completeness` | `balance.rs:44-279` | 280 of 429 | repository |
| `impl Balances for Service` | `reads.rs:128-149` | 22 | out of indexing |
| `impl Balances for Index<C>` | `index.rs:330-337` | 8 | out of indexing |
| `ObservedHistory` | `reads.rs:54-69, 82-113, 293-435` | ~200 of 1237 | out of indexing (§5.2) |
| `impl OutputStore for Service` + gates | `reads.rs:183-292` | 110 | **split** (§4.3) |
| SQLite query adapters | `sqlite/{balance,history,outputs,credits}.rs` | 87 + 406 + 368 + 163 = 1024 | repository |

`TransactionStore` **does not exist**. The spec's "`TransactionStore::list` is
not an indexing requirement" is already satisfied: the trait and
`storage/history.rs` were deleted, and the only surviving mentions are in
`docs/design/030-the-proposal.md:443,642` and
`docs/design/033-indexing-core.md:209,271,305,675,1025` describing a tree that
no longer exists. Those doc lines are stale and should be annotated, not acted
on.

### 1.2 The live schema, as five ordered steps

`sdk/indexing/src/storage/sqlite/schema.rs:28-225`, `MIGRATIONS: [&str; 5]`:

| Step | Creates | Key |
|---|---|---|
| 1 | `registration` | `(chain, network, asset, address)`, `UNIQUE (id)` |
| 2 | `checkpoint`, `history`, `movement`, `live_output` | see below |
| 3 | `block_credit` | `(chain, network, address, height, credit_id)` |
| 4 | `unmatched_spend` | `(chain, network, address, transaction_id, output_index)` |
| 5 | `DROP backfill`; `watch_progress` | `(chain, network, id)` FK → `registration(id)` |

Steps 3, 4 and 5 are days old. The spec reassigns all three. Every table is
`WITHOUT ROWID` and every one leads with `(chain, network)`, which is what makes
`purge_network` one `DELETE ... WHERE` per table (`schema.rs:41-45`,
`sqlite/tables.rs:164-180`).

### 1.3 Six spec items the tree already satisfies

Reported so no step is planned for them.

1. **Block identity.** `base::BlockRef` (`sdk/base/src/block.rs:88-98`) carries
   native `position`, produced `height`, own `hash`, and
   `parent: Option<BlockParent>`. That is exactly the spec's four
   requirements, and `BlockPosition` is documented as "Bitcoin/Ethereum height,
   Solana slot" (`block.rs:90`), so Solana traversal already uses slots.
2. **`indexed_through` records inspected blocks including empty ones.**
   `storage/watch.rs:28-30`: "every applicable block from the watch's birthday
   has been inspected for it — blocks with no matching activity included".
3. **Expected previous progress is checked.**
   `sqlite/progress.rs:60-62` refuses when
   `self.recorded(...) != claim.expected`, retryably.
4. **Registration repeats, conflicts, and asset independence.**
   `WatchStore::add`'s contract (`storage/watch.rs:143-155`) is already "an
   exact repeat is accepted and preserves existing progress" and "a conflicting
   definition fails the whole batch"; the durable key is already
   `(chain, network, asset, address)`, so a second asset at one address is
   already independent (`sqlite/watches.rs`, and the note at
   `watches.rs:290-297`).
5. **Exact atomic arithmetic; kind and direction separate.** Amounts are stored
   as `TEXT` and read through `Decimal::to_atomic(0)`
   (`balance.rs:259-267`); `MovementKind` is the stored discriminant and the
   sign is projected per `(movement, address)` pair and never stored
   (`balance.rs:109-141`).
6. **A custom repository requiring no balance/history/output methods.**
   `examples/custom_store/sink.rs` implements exactly `WatchStore`
   (`sink.rs:119`) and `IndexStore` (`sink.rs:279`) and says so at
   `sink.rs:10-16`. It must stay that way; see §7 obligation 11.

---

## 2. Greenfield or migration — the quote

The spec says "Follow the repository's durable-data migration requirements. Do
not discard persisted data." The repository's durable-data requirement is
`docs/SYSTEM_REQUIREMENTS.md:27-37`, quoted in full because the answer turns on
its exact wording:

> Pre-release policy: replace obsolete internal Rust and wire formats directly.
> No compatibility aliases, legacy readers, or `V1`/`V2` DTOs. **The on-disk
> index format is not an exception and is not a compatibility contract.** This
> requirement previously carved one out — the stored redb layout was durable and
> MUST be preserved, because a deployment's existing index file had to keep
> reading — and it is deliberately revoked, not quietly dropped, by the move to
> one SQLite store (`docs/design/030 §8 (ex-031)`). There is one storage
> backend, there is no reader for any earlier format, and there is no upgrade
> path from one: no deployment exists, so the exception protected nothing and
> cost every stored record a shape frozen for the life of the file. What
> replaces it is ordinary: the file carries `PRAGMA user_version`, a schema step
> may alter what an earlier step wrote, and a file this build cannot account for
> is refused rather than interpreted.

**Conclusion: this is a greenfield schema replacement.** "Do not discard
persisted data" binds *within* a file this build wrote — a step must not drop a
table whose contents this build still needs — and it does not oblige a reader
for the five-step shape. Concretely:

- The correct form is **one new step 6** that creates the new tables, copies
  what is copyable inside the same transaction, drops the old tables, and bumps
  `user_version` to 6. `schema.rs:285-292` already applies each step in one
  transaction with its own bump, so this is the mechanism, not new machinery.
- **What is copyable**: `registration` → `watch` (all columns, `balance` seeded
  by a fold, `balance_basis` seeded incomplete); `watch_progress` →
  `watch.indexed_through_block`; `checkpoint` → `network_checkpoint`; `history`
  + `movement` → `transaction` + `transaction_address` + `balance_movement`;
  `block_credit` → `balance_movement` with `transaction_id IS NULL`;
  `live_output` and `unmatched_spend` → `output`.
- **What is not copyable and must be stated**: nothing. Every old row maps. The
  one judgement call is `watch.balance`: seeding it requires running the fold
  once per `(address, asset)` inside the migration step, which is the same
  `SELECT` the balance query runs today. A step that instead seeds zero would
  silently corrupt every existing file, so the fold is not optional.
- **The alternative that is also defensible**: because no deployment exists,
  step 6 may simply `DROP` and recreate, and `schema.rs:265-281` already refuses
  a file it did not create. That is cheaper by roughly 120 lines of copy SQL.
  **Recommendation: write the copying step anyway.** The copy is the only
  executable proof that the two shapes hold the same facts, and it is the thing
  `AGENTS.md`-style review will ask for; 120 lines is cheap for that.

---

## 3. The concrete schema

SQLite types, `WITHOUT ROWID` throughout except where stated, `(chain, network)`
leading every key so `purge_network` stays one `DELETE ... WHERE` per table.
`chain` and `network` are two columns because `base::Network` is a pair
(`sqlite/tables.rs:44-57`); the spec's single `network` column is that pair, and
this document keeps both rather than inventing a composite text encoding.

### 3.1 `watch` — replaces `registration` and `watch_progress`

```sql
CREATE TABLE watch (
    chain              TEXT    NOT NULL,
    network            TEXT    NOT NULL,
    asset              TEXT    NOT NULL,
    address            TEXT    NOT NULL,
    id                 TEXT    NOT NULL,
    birthday           INTEGER NOT NULL,   -- BlockPosition
    balance            TEXT    NOT NULL,   -- atomic, non-negative, scale 0
    balance_basis      INTEGER NOT NULL,   -- 0 incomplete, 1 opening, 2 complete
    basis_block_hash   BLOB,               -- evidence: the block vouched for
    basis_amount       TEXT,               -- evidence: what the witness read
    indexed_position   INTEGER,            -- indexed_through_block, or NULL
    indexed_height     INTEGER,
    indexed_hash       BLOB,
    indexed_parent_position INTEGER,
    indexed_parent_hash     BLOB,
    indexed_timestamp       INTEGER,
    PRIMARY KEY (chain, network, asset, address)
) WITHOUT ROWID;
CREATE UNIQUE INDEX watch_id ON watch (chain, network, id);
```

- The PK is the spec's "Unique observation: `(network, address, asset)`",
  reordered `asset, address` to match the existing PK column order so the copy
  is an index walk (`schema.rs:37`).
- `id` keeps its unique index. Today it is `UNIQUE (id)` globally
  (`schema.rs:39`); adding `(chain, network)` narrows it to per-network, which
  is what `WatchStore::add`'s conflict rule actually means. **This is a
  behaviour change**: a caller reusing one id on two networks is accepted where
  it was refused. `sqlite/watches.rs:373` maps the violation to `Conflict`;
  narrowing it needs `tests/watch_contract.rs` to say which it wants.
  **Recommendation: keep it global** (`UNIQUE (id)`), because
  `apps/api`'s id → key map is global (`payment_api::Signers`).
- The six `indexed_*` columns are one nullable `BlockRef`, spelled exactly as
  `watch_progress` spells it (`schema.rs:211-223`) and read by
  `Tables::coordinates`/`Tables::block` (`sqlite/tables.rs:227-258`) unchanged.
  Folding them into `watch` deletes the FK, the cascade, and
  `Tables::registered` (`sqlite/progress.rs:94-105`) — a read per claim.
- `balance_basis` is an integer discriminant, matching how `movement.kind` is
  stored (`sqlite/history.rs:287-315`), not a string: `linter.toml:136`
  (`rust/string-backed-finite-state`).
- `basis_block_hash` + `basis_amount` are the spec's "retain the evidence needed
  to justify the classification". They are only ever non-null for basis 1 and 2,
  and today only `chain_ethereum::BalanceWitness` can produce either
  (§5.4).

### 3.2 `network_checkpoint` — replaces `checkpoint`

Unchanged in shape, renamed, with the block reference spelled as the six
columns:

```sql
CREATE TABLE network_checkpoint (
    chain TEXT NOT NULL, network TEXT NOT NULL,
    position INTEGER NOT NULL, height INTEGER NOT NULL, hash BLOB NOT NULL,
    parent_position INTEGER, parent_hash BLOB, timestamp INTEGER,
    PRIMARY KEY (chain, network)
) WITHOUT ROWID;
```

The spec's reason for keeping it separate — "individual watches may still be
catching up" — is already the tree's reason, and already load-bearing:
`WatchProgress::caught_up(checkpoint)` (`storage/watch.rs:112`) is the
predicate with four callers.

### 3.3 `transaction` — replaces the shared half of `history`

```sql
CREATE TABLE transaction_fact (
    chain             TEXT    NOT NULL,
    network           TEXT    NOT NULL,
    transaction_id    TEXT    NOT NULL,
    block_position    INTEGER NOT NULL,
    block_height      INTEGER NOT NULL,
    block_hash        BLOB    NOT NULL,
    block_parent_position INTEGER,
    block_parent_hash     BLOB,
    block_timestamp       INTEGER,
    transaction_order INTEGER NOT NULL,
    execution_status  INTEGER NOT NULL,   -- 0 included, 1 failed
    failure_reason    TEXT,
    fee_asset         TEXT,
    fee_amount        TEXT,
    fee_payer         TEXT,
    PRIMARY KEY (chain, network, transaction_id)
) WITHOUT ROWID;
```

Named `transaction_fact`, not `transaction`: `TRANSACTION` is a SQLite keyword
and every statement would need quoting. The spec asks for singular entity names,
which this is.

**What it buys, measured.** Today `history` copies the whole block reference
(six columns), the fee (three columns) and the status (two columns) **once per
participating address** (`sqlite/history.rs:332-336`, fanned out by
`Tables::rows` at `sqlite/blocks.rs:189-203`). A two-party transfer stores the
eleven shared columns twice; a Bitcoin transaction with twelve distinct
addresses stores them twelve times. Normalizing is a real storage reduction and
the only one in this document.

**What it costs.** `Tables::verify` (`sqlite/blocks.rs:173-188`) compares a
re-presented row against the stored one and calls a mismatch terminal
corruption. That comparison is per `(address, height, transaction)` today; it
becomes per `(transaction)`, which is *stronger* (one row, one truth) and
simpler. Good.

**`transaction_order` is new and is a contract change.** Nothing stores native
in-block order today: `history` is ordered `ORDER BY height, transaction_id`
(`sqlite/history.rs:122`) and `CanonicalPage` documents "ascending by
`(height, transaction id)`" (`history.rs:199-207`). See §5.6.

### 3.4 `transaction_address` — the association, and why it is necessary

```sql
CREATE TABLE transaction_address (
    chain          TEXT    NOT NULL,
    network        TEXT    NOT NULL,
    address        TEXT    NOT NULL,
    block_height   INTEGER NOT NULL,
    transaction_order INTEGER NOT NULL,
    transaction_id TEXT    NOT NULL,
    PRIMARY KEY (chain, network, address, block_height, transaction_order, transaction_id),
    FOREIGN KEY (chain, network, transaction_id)
        REFERENCES transaction_fact (chain, network, transaction_id) ON DELETE CASCADE
) WITHOUT ROWID;
```

The spec is cautious here — "only where needed". **It is needed, and the code
says exactly why.**

`CanonicalTransaction::addresses()`
(`sdk/indexing/src/observation.rs:247-263`) is the attribution rule:

```rust
let mut addresses = self.movements.iter()
    .flat_map(|m| m.from().cloned().into_iter().chain(m.to().cloned()))
    .collect::<BTreeSet<_>>();
if let Some(payer) = self.fee.as_ref().and_then(|fee| fee.payer.clone()) {
    addresses.insert(payer);
}
```

The fee payer is inserted **unconditionally**, and that is the whole argument:

- **Fee-only transactions reach history today.** A successful transaction whose
  movements never touch the payer still produces a row under the payer, with
  zero movement rows. `sqlite/history.rs:103-108` states it: "a history row
  counts for a holding even when none of its movements are in that asset,
  because the row may still carry a fee the holding paid". The selection rule is
  `HistoryQuery::charges` (`history.rs:188-196`): the payer sees it when the
  transaction failed, or when the query asset is the fee asset.
- **Failed transactions have no movements at all.**
  `ObservationDraft::validate_movements`
  (`observation.rs:360-369`) refuses a `Failed` draft carrying any movement:
  `"failed transaction contains movements"`. So for a failed transaction
  `addresses()` is exactly `{fee payer}`.
- Therefore **omitting `transaction_address` loses every failed transaction and
  every fee-only transaction from every address's history**, because there is no
  `balance_movement` row to join through. Two contract scenarios assert exactly
  those two cases: `HistoryContract::failed_fee_payment_appears_for_every_asset`
  (`sdk/indexing/src/testing/history.rs:97`) and
  `successful_fee_payment_appears_for_the_fee_asset_only`
  (`testing/history.rs:116`).

The key leads with `(address, block_height, transaction_order, transaction_id)`
so a history page is an ascending index walk, as `schema.rs:49-53` requires of
every table here.

**One gap found, unrelated to the spec.** A *failed* transaction with
`fee.payer == None` is written under no address at all and is silently absent
from every history. No chain produces one today (all three attribute the fee),
so this is a latent hole rather than a live defect. Worth a note in
`docs/INDEXING.md`, not a step here.

### 3.5 `balance_movement` — replaces `movement` and `block_credit`

```sql
CREATE TABLE balance_movement (
    chain           TEXT    NOT NULL,
    network         TEXT    NOT NULL,
    address         TEXT    NOT NULL,
    asset           TEXT    NOT NULL,
    block_height    INTEGER NOT NULL,
    block_position  INTEGER NOT NULL,
    movement_order  INTEGER NOT NULL,
    movement_id     TEXT    NOT NULL,
    transaction_id  TEXT,                -- NULL for a transactionless event
    kind            INTEGER NOT NULL,    -- MovementKind, + Fee, + Withdrawal
    direction       INTEGER NOT NULL,    -- 0 credit, 1 debit
    amount          TEXT    NOT NULL,    -- atomic, non-negative
    counterparty    TEXT,                -- the other endpoint, when there is one
    PRIMARY KEY (chain, network, address, asset, block_height,
                 transaction_id, movement_id, direction)
) WITHOUT ROWID;
```

**Identity, which the spec requires ("Every movement still needs a stable
identity for deduplication").** `MovementId` is unique within its transaction,
enforced at `observation.rs:370-371` (`!ids.insert(&movement.id)` → invalid
block). `BlockCredit::id` is unique within its block, enforced at
`block.rs:80-96` (`validate_all`). So:

- transaction movement: `(network, transaction_id, movement_id, address, asset, direction)`
- transactionless credit: `(network, block_height, movement_id, address, asset, direction)`
  with `transaction_id IS NULL`

`direction` **must** be in the key. A same-address transfer produces a credit
and a debit for the same `(transaction_id, movement_id, address, asset)`; the
spec requires both be preserved ("Preserve both sides of same-address transfers
without changing the net balance") and without `direction` in the key one would
overwrite the other. SQLite treats `NULL` as distinct in a PK, so
`transaction_id IS NULL` rows need `block_height` in the key, which it has.

**Bitcoin's fee rule is preserved, and the mechanism moves.** The spec says
"Bitcoin inputs/outputs already account for fees; do not add a second fee
debit." Today that is a *read-time* parameter: `BlockInterpreter::settlement()`
(`sdk/indexing/src/source.rs:98`) returns `Movements` for bitcoin
(`sdk/chains/bitcoin/src/interpreter.rs:110-112`) and `Charged` for the other
two (`ethereum/src/interpreter.rs:111-113`, `solana/src/interpreter.rs:108-110`),
and `Ledger::apply` consults it per query (`balance.rs:193-200`, fed from
`reads.rs:136`).

Under `balance_movement` the fee becomes a **row**, not a parameter: a
`Charged` chain emits one `kind = Fee, direction = debit` movement for the
payer in the fee asset; a `Movements` chain emits none, because Σinputs −
Σoutputs already paid it. The shape preserves the rule, and it is strictly
better — the rule becomes a stored fact rather than something every reader must
be told. The cost: `FeeSettlement` must be known at commit time. It is —
`Service` owns the interpreter (`service.rs:46`) — but the fee row must be
generated **once per transaction per payer**, not once per address, or a
multi-address Bitcoin-style fan-out would charge it repeatedly. That is a
correctness invariant and is marked as one in §6.

`counterparty` replaces `sender`/`recipient` (`schema.rs:103-104`). One column
rather than two, because a row now describes one address's effect: the row's own
`address` is one endpoint and `direction` says which, so the second column is
the *other* endpoint. This is the spec's "Do not mechanically rename existing
movement rows" applied concretely.

**Transactionless movements must appear in activity results — they do not
today.** `block_credit` is read by exactly one query, the balance fold
(`sqlite/credits.rs:122` called only from `sqlite/balance.rs:74`). History
paging reads `history` + `movement` only (`sqlite/history.rs:52-135`). This is
documented as deliberate at `overlay.rs:276-281` and `block.rs:22-27`, and is a
**published API contract**: `docs/API.md:156` says a validator withdrawal
address "can report a balance while `GET /v1/wallets/{id}/transactions` is
empty, because that page lists transactions and a withdrawal is not one."
Unifying into `balance_movement` satisfies the spec and **breaks that
documented behaviour**. §5.8.

### 3.6 `output` — replaces `live_output` and `unmatched_spend`

```sql
CREATE TABLE output (
    chain             TEXT    NOT NULL,
    network           TEXT    NOT NULL,
    transaction_id    TEXT    NOT NULL,
    output_index      INTEGER NOT NULL,
    address           TEXT    NOT NULL,
    -- creation_details, all NULL together or all present together
    asset             TEXT,
    amount            TEXT,
    evidence          BLOB,
    created_height    INTEGER,
    coinbase          INTEGER,
    -- spending_details
    spent_height      INTEGER,
    spent_by          TEXT,
    PRIMARY KEY (chain, network, address, transaction_id, output_index),
    CHECK (asset IS NOT NULL OR spent_height IS NOT NULL)
) WITHOUT ROWID;
CREATE INDEX output_unspent
    ON output (chain, network, address, transaction_id, output_index)
    WHERE spent_height IS NULL;
```

**`address` stays in the primary key**, against the spec's literal
`(network, transaction_id, output_index)`. Reason, with the line: the spendable
page walks `WHERE chain/network/address ORDER BY transaction_id, output_index`
(`sqlite/outputs.rs:92-95`) and the key *is* that order — the comment at
`schema.rs:47-53` says every table here is addressed by its whole primary key so
`EXPLAIN QUERY PLAN` shows an index walk rather than a sort. Dropping `address`
from the key forces a secondary index that re-adds the b-tree `WITHOUT ROWID`
avoided, for no gain: an outpoint is claimed by one address, so
`(address, transaction_id, output_index)` and `(transaction_id, output_index)`
identify the same row. This is a deliberate divergence, not an oversight.

**The four valid states map to column nullity**, and the `CHECK` forbids the
fourth:

| Spec state | Columns | Today |
|---|---|---|
| creation known, spend unknown | creation set, `spent_height IS NULL` | a `live_output` row |
| creation and spend known | both set | **no row at all** (the row is `DELETE`d) |
| spend known, creation unknown | creation NULL, spend set | an `unmatched_spend` row |
| neither known | refused by `CHECK` | unrepresentable |

**The second row is the storage tradeoff the spec asks to document.** Today a
spend is `DELETE FROM live_output` (`sqlite/outputs.rs:261-264`) and the row is
gone. Retaining it means the table grows with every output a watched address
ever received, forever, and `purge_network` is the only removal. For a
long-lived Bitcoin deposit address that is one row per historical UTXO —
bounded by received-payment count, not by uptime or chain length, but
unbounded in the sense that nothing ever shrinks it. The partial index
`output_unspent` is what keeps the spendable page's cost proportional to the
*unspent* set rather than the historical one, and it is not optional: without
it, coin selection's page walk degrades with the address's lifetime.

**What retention buys, and it is not small.** `unmatched_spend` exists because
a forward spend that matched no live output must be remembered, or a later
backfill re-creates a permanently spendable phantom UTXO. The rationale is at
`schema.rs:157-179` and `sqlite/outputs.rs:236-248`, and the contract scenario
is `SpendContract::a_reported_miss_outlives_the_block_that_reported_it`
(`sdk/indexing/src/testing/spends.rs:180`), which runs the same backfill batch
twice (`spends.rs:201-210`) so a store that *consumed* the tombstone to cancel
the first create would insert a spendable output on the redo. Under the merged
table that property is structural rather than cooperative: the backfill's
"discover the creation" becomes

```sql
UPDATE output SET asset = ?, amount = ?, evidence = ?, created_height = ?, coinbase = ?
 WHERE ... AND asset IS NULL;
INSERT OR IGNORE INTO output (...creation..., spent_height = NULL) VALUES (...);
```

— filling creation into a row whose spend is already known leaves
`spent_height` set, so the output is never offered again, **and the redo is a
no-op because the row already has its creation**. That is the spec's
"Discovering creation later must never erase spending" holding by shape.
`Tables::unmatched` and `Tables::record_miss` (`sqlite/outputs.rs:288-321`, 34
lines) disappear, and so does the `removed == 0 && required` dance at
`outputs.rs:276-280`.

**The one thing that must not change.** Only *required* spends are remembered;
a **tracked** spend records nothing. `sqlite/outputs.rs:244-248` states the
reason — "no address claimed the outpoint, and the backfill of the address that
eventually does claim it anchors above this block and so inspects this very
spend again" — and `spend()` enforces it with `if removed == 0 && required`
(`outputs.rs:276-280`). Under the merged table, writing a row for an unmatched
*tracked* spend would insert one per unwatched outpoint spent in every block:
unbounded in chain length, which is the one growth this schema must not have.
Marked as a correctness invariant in §6.

### 3.7 No block table

The spec says do not add one automatically. Agreed, and the reason is already
in the tree: `BlockRef` is the existing representation, it is written inline as
six columns in three places today, and the only reader that needs more is
confirmation arithmetic, which is derived at read time
(`reads.rs:293-302`: "a confirmation count and a `Pending` status are statements
about the line those rows were read at"). Normalizing would trade eleven
duplicated columns per transaction for a join on every page — and §3.3 already
removes the per-address duplication, which was the actual cost.

---

## 4. The ownership change

### 4.1 What moves out of `sdk/indexing`

Three moves, largest first.

**Move 1 — balance, symmetric with the history move already done.**
Out: `Balances` (`balance.rs:276-279`), `Ledger` (`balance.rs:153-268`),
`Completeness` (`balance.rs:44-89`), `BalanceQuery`/`BalanceStore`
(`storage/reads.rs:30-76`), `impl Balances for Service` (`reads.rs:128-149`),
`impl Balances for Index<C>` (`index.rs:330-337`), and `Indexer: Indexing +
Balances` (`indexer.rs:83-85`) collapsing to `Indexer = Indexing`.
The consumers are eight production call sites: `apps/api/src/api/mod.rs:35,382`
and six example sites (`examples/transfer/main.rs:106,135`,
`examples/all_chains/main.rs:140`, `examples/watch_only/main.rs:84`). All of
them go from `index.balance(holder)` on `Arc<indexing::Index>` to
`balances.balance(holder)` on an `Arc<dyn Balances>` composed per network —
**exactly** what `Histories` (`sdk/wallets/src/composition.rs:234-251`) already
does for history, including its `NetworkMismatch` for an unmatched network
(`composition.rs:247-251`). This is the biggest move and the lowest-risk one,
because the target shape is already written and tested.

**Move 2 — the query adapters and their vocabulary.**
`sqlite/{balance,history,outputs,credits}.rs` (1024 production lines),
`ConfirmedHistory` + `HistoryQuery` + `CanonicalPage` + `HistoryPosition` +
`Page` + `TransactionPage` (`history.rs`, 310 production lines), `OutputRequest`
/ `OutputPage` / `OutputCursor` (`output.rs:75-149`), `Purging` + `PurgedRows`
(`storage/reads.rs:89-153`). These are the spec's "Pagination, filtering, and
query result types". They keep their code; what changes is which crate declares
them.

**Move 3 — `ObservedHistory`.** §5.2 is the conflict; the recommendation there
is that it moves, and what it costs.

**Not a move — the SQLite backend stays in `sdk/indexing/src/storage/sqlite`.**
User decision, overriding move 2's implication that the adapters change crate:
*"bad idea, should stay there"*. Move 2 therefore covers the query **vocabulary**
(`ConfirmedHistory`, `HistoryQuery`, `CanonicalPage`, `HistoryPosition`, `Page`,
`TransactionPage`, `OutputRequest`, `OutputPage`, `OutputCursor`, `Purging`,
`PurgedRows`) and not the files that implement it against SQLite.

The reasons this is right, recorded so the ownership branch does not reopen it:
the built-in store is what makes `sdk/indexing` usable without a consumer
writing one, and it is already behind a default-off-able feature, so its cost to
a third-party sink is zero; §6's own risk note says the publicly exported
`testing::*` conformance suite is reused by `sdk/wallets` and that moving five of
its six contracts across a crate boundary could force a **third** crate — the
SQLite move is what drags the suite with it; and the spec's own requirement is
that a third-party repository can index *without implementing our query API*,
which is satisfied by the ports being narrow, not by the built-in adapter living
elsewhere. `examples/custom_store/` already proves that with two methods.

### 4.1b The in-memory store becomes a shipped backend

User decision: *"I don't want this to be in testing, I want memory storage to be
usable as production code as well."*

`Memory` (`sdk/indexing/src/testing/store.rs:74`) is already a complete backend —
it answers `WatchStore`, `IndexStore` and the query ports, which is why the
conformance contracts run against it beside SQLite. What is wrong is its address:
it sits under `testing/` behind `feature = "testing"`, so its name says test
double and a consumer cannot depend on it without pulling the conformance suite.

The move:

- out of `sdk/indexing/src/testing/` into the storage layer beside `sqlite`,
  reachable as `indexing::memory::Repository`, mirroring `indexing::sqlite::Repository`;
- its own `memory` Cargo feature beside `sqlite`, so a consumer picks either,
  both, or neither, and neither drags the other;
- **it keeps the query ports.** A production in-process store must answer
  balance, history and outputs. It is not the minimal-sink reference —
  `examples/custom_store/` is, with two methods, and stays that way;
- public rustdoc stating what it is for (tests, ephemeral processes, a consumer
  that wants no file) and what it is not (durable across a restart);
- the conformance contracts keep running against both backends. That is what
  proves the two agree, and it is how a divergence was caught before: a blank
  address was refused by `Memory` and answered with an empty page by SQLite.

Note check 11 in `scripts/check-chains.sh` forbids a `src/` directory holding one
`.rs` file and no subdirectory, so a single-file `storage/memory/` is not
available; either the backend is several files or it sits flat beside `sqlite.rs`.

**Landed**, in the shape above and with the suite move of §4.1c that follows
from it. `storage/memory.rs` holds the tables and the reads,
`storage/memory/{commits,observations}.rs` the write half and the watch list —
three files, so check 11 is satisfied the way `sqlite` satisfies it. The
`testing` feature is gone: `memory` replaced it for `sdk/wallets`, the only
crate that had named it, and the contract scenarios moved to
`sdk/indexing/tests/contracts/`.

### 4.1c The contract suite moves to `tests/`

Forced by 4.1b: once `Memory` is a shipped backend, the scenarios beside it are
the only reason a consumer would link the suite, and they are not API. They are
now `sdk/indexing/tests/contracts/`, reachable from `repository_contract.rs`
alone.

The constraint this had to clear is that **`src/` cannot use `tests/`**, and
four SQLite adapter modules ran the shared scenarios in-module behind
`#[cfg(all(test, feature = "testing"))]`. Those runs did not need relocating one
by one: `tests/repository_contract.rs` now emits *two* tests per scenario, one
per backend, under a module named for the scenario — so removing the in-module
`contract!` blocks moved no coverage, it de-duplicated it. Each of those modules
keeps only what a store on a *file* can be asked, which is what its doc comment
now says. Arithmetic: `--lib` went 138 → 81 (−57 runs), `repository_contract`
118 → 122, and the purge contract gained a SQLite run it had only ever had
in-module.

No crate outside `sdk/indexing` was left needing the scenarios, so the third
crate §4.1 warned about was not required. `sdk/chains/solana` never enabled
`indexing`'s `testing` feature at all — the audit's claim that it did was about
`json_rpc::testing`, a different crate's feature of the same name, which is
untouched.

### 4.2 What stays, and why

| Stays | Line | Justification |
|---|---|---|
| `WatchStore` (4 methods) | `storage/watch.rs:132` | the spec's "Watches, birthdays, and inspection planning" |
| `IndexStore` (2 methods) | `storage/commit.rs:362` | the spec's "Atomic commit requests" |
| `IndexedBatch` and its five validators | `storage/commit.rs:75-328` | continuity, finality, claim justification |
| `Frontier`, `Sample`, `LiveChanges` | `overlay.rs:33,95,316` | the spec's "Indexing retains reorgable pending facts" |
| `Watches`, `WatchProgress::caught_up` | `watches.rs`, `storage/watch.rs:112` | inspection planning; the gate fact |
| `CanonicalBlock`, `ValueMovement`, `BlockCredit`, `OutputChanges` | `block.rs`, `observation.rs`, `output.rs:153` | the spec's "Chain-adapter contracts and block interpretation" |
| `FeeSettlement` | `balance.rs:102` | consulted at **commit** time now (§3.5), so it belongs with interpretation, not with the fold |
| the `spendable` and `resumable` gates | `reads.rs:245,271` | §4.3 |

### 4.3 The gate: who owns `Service::spendable` afterwards

`Service::spendable` is at `sdk/indexing/src/reads.rs:245-256` — not in
`service.rs` or `output.rs`:

```rust
fn spendable(backfilling: bool) -> Result<(), IndexError> {
    if !backfilling { return Ok(()); }
    Err(IndexError::new(
        IndexErrorKind::Conflict,
        "spendable outputs are not readable while this address's own history \
         is still being inspected: an unspent row cannot be proven unspent \
         below the checkpoint it was read at",
        true,
    ))
}
```

Its one caller is `impl OutputStore for Service` (`reads.rs:199`), which feeds
it `self.watches.backfilling(&address, page.checkpoint.as_ref())`
(`reads.rs:196-198`). The blast radius on refusal, traced:
`sdk/chains/bitcoin/src/sender/utxos.rs:93` propagates it →
`utxos.rs:53-58` aborts the whole multi-address read so no partial UTXO set
exists → `sdk/chains/bitcoin/src/sender/outbox.rs:206-208` maps it to
`TransactionErrorKind::Unavailable` **before** `self.reservations.admit(...)`
at `outbox.rs:209`, so nothing is reserved, selected, signed or submitted.
`sdk/base/src/transaction.rs:16-17` makes `Unavailable` retryable by contract.
That is the P1 fix, and it is three hops from a one-line `if`.

**The repository cannot own this gate**, and the reason is structural rather
than stylistic: the gate's input is `Watches::backfilling`
(`sdk/indexing/src/watches.rs`), which reads the hot watch set's
`indexed_through` against the checkpoint the page carried. A repository knows
the rows; it does not know what has been *inspected for this observation* at the
instant of the read, because that lives in the index's own set. It could read
`watch.indexed_*` from its own table, but that is the durable value, and the hot
set is ahead of it between commits — a gate taken on the stale value refuses
reads that are already safe.

**Recommendation: `OutputStore` does not move; its implementation does.** The
repository declares and implements the *query* — pages of rows at a checkpoint.
Indexing keeps a decorator on `Service` that (a) samples the overlay, (b) takes
`resumable` on the cursor revision, (c) takes `spendable` on the fresh
`backfilling` fact, (d) folds `LiveChanges`. Bitcoin keeps receiving
`indexer.clone()` as its `Arc<dyn OutputStore>`
(`sdk/chains/bitcoin/src/config.rs:294`, coercing through the alias at
`sdk/chains/bitcoin/src/indexer.rs:17`) and nothing in
`sdk/chains/bitcoin/src/wallet.rs:169,189` changes. The spec's "Remove
balance/history/output query implementations and forwarding from indexing" is
satisfied for the implementation; the ~110 lines at `reads.rs:183-292` are not
forwarding, they are the two gates and the overlay fold, and they are the reason
the P1 stays fixed.

`Purging` is the easy case: one production consumer,
`apps/server/src/rescan.rs:103`, already outside indexing. It moves wholesale.

---

## 5. Conflicts

Ten. Five were named in the brief; five more were found. Each states the
conflict, the cost either way, and a recommendation.

### 5.1 `WatchStore::reanchor` is a postcondition; the spec forbids the upsert

**The conflict.** `reanchor`'s contract is deliberately a postcondition
(`storage/watch.rs:231-238`): "It states what must be true afterwards, not what
to change … So the store holds each observation as given whether or not it held
it before, and **repeating the call is how an uncertain one is settled**." The
spec says "Do not use an unrestricted upsert that resets or redefines an
existing watch."

**Why it exists.** It serves `Backfilling::cancel`
(`sdk/indexing/src/watches.rs:238-267`), which re-anchors every *owing* watch to
the checkpoint's successor so a read's completeness stays honest once the range
below will never be read. It is a fourth method with a lint exception
(`storage/watch.rs:127-131`) bought specifically so the operation is one
transaction: `forget` then `add` passes through a state where the address is
watched by nothing, and a crash leaves it there, indistinguishable from an
address nobody registered.

**Cost of the spec's way.** Making `reanchor` refuse a row it does not hold
turns a lost acknowledgement into an unrecoverable state: `cancel` cannot tell
"applied" from "applied none" from "withdrawn since", and `watches.rs:229-237`
explicitly relies on the repeat to settle all three.

**Cost of today's way.** Once `balance` lives on the `watch` row (§3.1), an
upsert stated as "holds each observation as given" is a total reset — it would
zero a persisted balance. The spec's objection is real, and it is *about the new
schema*, not about the old one.

**Recommendation: narrow it to a monotone raise rather than restricting
existence.** `reanchor` keeps taking a batch and keeps being idempotent, and
gains two rules: it writes only `birthday` and `indexed_through_block`, never
`balance`, `balance_basis` or `id`; and it refuses a birthday *below* the stored
one. Both properties hold under repeat — raising to a birthday already stored is
a no-op — so the recovery story survives intact, and the spec's actual concern
(a reset that redefines a watch) is answered by narrowing the column set rather
than by adding a precondition. Cost: about 25 lines in
`sqlite/watches.rs:211-243` plus a scenario in `testing/watch.rs`.

### 5.2 `ObservedHistory` holds `Arc<Service>`; the spec wants it outside

**The conflict.** `ObservedHistory` is at `reads.rs:54-69` and its constructor
says why (`reads.rs:60-64`): "One argument, because one value already owns both
halves. Handing the overlay out instead would make it shared mutable state with
two readers and no rule about which of them a commit lands between." The spec
says do not keep the orchestration inside indexing merely to reach memory, and
instead "expose a coherent pending-facts snapshot to the query layer."

**What the snapshot concretely is.** `overlay::Sample`
(`sdk/indexing/src/overlay.rs:33-38`) — `{ blocks: Vec<CanonicalBlock>,
revision: u64 }`, produced by `Frontier::sampled()`, which **clones** each
block's facts. It is already a by-value snapshot, not a borrow. Its ordering
contract is `Sample::above(checkpoint)` (`overlay.rs:40-74`), which states the
two rules that make a two-read answer sound: sample the overlay *first*, then
narrow by the checkpoint the store's own answer carried. Both rules are
properties of the *call order*, not of the type.

**So the stated objection does not survive inspection.** Handing out a `Sample`
is not shared mutable state — it is an owned clone with a revision stamp. What
would be shared mutable state is handing out the `Arc<Frontier>`, which is a
different thing and which nothing proposes. The overlay is already behind an
`Arc` and already held by test fixtures (`service.rs:54-57`, `Service::over` at
`service.rs:94-100`).

**What it does cost, honestly.** Two things, both real.

1. **The ordering rule leaves the type that enforces it.** Today a caller cannot
   read the store before sampling, because `ObservedHistory::history` does both
   (`reads.rs:92-95`). Exported, the rule becomes documentation: a query layer
   that samples second double-counts a block committed in between, which
   `overlay.rs:56-62` spells out as a defect. Mitigation: export `Sample` with
   `above` as its only consumer-facing method and make `sampled()` return a
   value whose only use is `above`, so the narrow is unavoidable once you hold
   one. That is a type-level fix, not a comment.
2. **`Completeness` weakening needs a second fact across the boundary.**
   `history` also calls `self.index.watches.backfilling(...)`
   (`reads.rs:99-103`). That is the same fact the §4.3 gate needs and it is not
   in the `Sample`. The snapshot must therefore be a pair — the blocks and the
   per-address inspection answer — or the query layer must call back for it.

**Recommendation: move it, and widen the snapshot to carry both facts.** Export
`PendingFacts { sample: Sample, inspected_through: Option<BlockRef> }` for one
address, taken in one call so the two cannot be read at different instants
(the same discipline `WatchStore::all`'s "one consistent snapshot" already
imposes, `storage/watch.rs:161-168`). `ObservedHistory`'s ~200 lines move to
the query layer; the three construction sites
(`sdk/chains/bitcoin/src/config.rs:297`, `ethereum/src/config.rs:282`,
`solana/src/config.rs:287`) change type and nothing else.

### 5.3 Derive-on-read versus a persisted total

**The conflict.** `Ledger` folds movements in `BigUint` at query time
(`balance.rs:153-268`, driven by `sqlite/balance.rs:47-94`). The spec wants
"the persisted accumulated amount" on `watch`, written atomically with movements
and progress.

**What it buys.** The fold's cost today is proportional to an address's whole
history: `sqlite/balance.rs:74` walks every `history` row and every
`block_credit` row for the holding, on every balance request. A persisted total
makes the read one row. For `apps/api`'s balance route
(`apps/api/src/api/wallet.rs:289` → `mod.rs:382`) that is O(history) → O(1).
That is the entire benefit, and it is a latency benefit, not a correctness one.

**What it risks, and it is the specific risk the current design was chosen to
avoid.** `balance.rs:1-8` states the property: the confirmed half is "a fold
over the movements the index already stores … not a second number read from a
node, so it cannot disagree with the history the same store answers." A
persisted total *can* disagree with its movements, and has no self-correction:
`Completeness::Overdrawn` (`balance.rs:62-65`) exists precisely because the fold
can *prove* itself short — credits less than debits — and that proof is only
available to something that recomputes. A persisted total that drifts low looks
exactly like an honest small balance.

**Double counting a persisted total.** Today a repeated movement is prevented by
the primary key: `INSERT OR IGNORE INTO history` (`sqlite/history.rs:332`) and
`INSERT OR IGNORE INTO block_credit` (`sqlite/credits.rs:33`), and a re-fold of
the same rows yields the same number by construction. A persisted total has no
such property — the row is updated, and an update applied twice is wrong. The
only sound form is: the balance update must be **derived from the rows the same
statement actually inserted**, i.e. `UPDATE watch SET balance = balance + ?`
where `?` is computed from `changes()` after the inserts report which rows were
new. `sqlite/history.rs:355-364` already returns exactly that signal
(`if written == 0 { return Ok(false) }`), so the mechanism exists; what is new
is that the balance delta must be accumulated from those booleans rather than
from the batch. Every `INSERT OR IGNORE` that reports zero contributes zero.

**Recommendation: adopt it, with a reconciliation obligation the spec does not
ask for.** Persist `balance`, maintain it from insert-reported deltas, and keep
`Ledger` as an **offline** fold exposed through `Purging`-style maintenance (the
spec's "Offline maintenance"), used by (a) the step-6 migration to seed every
row, (b) a `--verify` mode that recomputes and reports disagreement, (c) the
`Overdrawn` proof, which becomes "the recomputation went negative" rather than
"the read went negative". Without (c) the spec's own "expose incompleteness"
loses its only negative proof. Cost of keeping `Ledger`: it stays at ~115
production lines instead of being deleted; that is the honest price of not
having a total that cannot check itself.

### 5.4 `balance_basis` versus `Completeness`

**They are not the same axis, and one of them is not persistable as stated.**

`Completeness` (`balance.rs:44-66`) has three variants:

| Variant | Meaning | Produced by |
|---|---|---|
| `Complete` | every change since the birthday is indexed **and** the birthday is proven at-or-before first use | only `chain_ethereum::BalanceWitness::attest` (`sdk/chains/ethereum/src/wallet.rs:270-297`), on agreement at the fold's own block, and only from `FromBirthday` |
| `FromBirthday` | every change since the birthday is indexed; nothing proves the birthday was early enough | the default everywhere (`balance.rs:246-248`) |
| `Overdrawn` | the fold went negative: credits provably missing, `amount` forced to zero | `balance.rs:242-243` |

The spec's three classifications are: complete history; verified opening
balance; incomplete.

**The mapping:**

| Spec | `Completeness` | Notes |
|---|---|---|
| complete history (from before first funding) | — | **no variant exists**; `Complete` is not this |
| verified opening balance | `Complete` | this is what the witness actually proves: agreement at one block, not history from genesis |
| incomplete | `FromBirthday` | |
| — | `Overdrawn` | **no spec classification**; it is a proof, not a basis |

So they overlap on two of five and disagree on the two that matter.
`Completeness::Complete`'s doc (`balance.rs:46-52`) claims "the start is proven
to be at or before the address's first use", but the only thing that constructs
it is an independent read agreeing at one block — which proves the *total* at
that block, not the birthday. `docs/INDEXING.md:203-211` describes it correctly
("agreement promotes `FromBirthday` to `Complete`, and only for that block")
while the variant's own name and doc overclaim. That is a pre-existing
misnaming the spec has correctly noticed.

**Second problem: `Complete` is produced outside indexing, at read time.**
`BalanceWitness::attest` takes `&mut Balance` and mutates `completeness` in
`sdk/chains/ethereum` (`wallet.rs:291-294`). It is never persisted. A
`balance_basis` **column** requires someone to write the attestation durably,
which nothing does today.

**Third: `while_backfilling` is a different axis again.**
`Completeness::while_backfilling` (`balance.rs:83-88`) is applied at read time by
both reads (`reads.rs:99-104`, `reads.rs:142-144`) and by nothing else. It
weakens the claim while rows are still arriving. That is a statement about
*inspection progress*, which `watch.indexed_through_block` already holds — so it
is derivable, not storable, and must stay a read-time projection over the
persisted basis.

**Recommendation: `balance_basis` replaces `Completeness`'s first two variants
and only those.** Three columns of state, two axes of answer:

- `balance_basis` persisted, three values: `0 = incomplete` (today's
  `FromBirthday`), `1 = verified opening balance` (today's `Complete`, renamed to
  what it means, with `basis_block_hash` + `basis_amount` as the retained
  evidence the spec asks for), `2 = complete history` (nothing produces it; keep
  it in the vocabulary as `Complete` is kept today, per `balance.rs:48-52`).
- `Overdrawn` stops being a basis and becomes what it is: the result of a
  recomputation, reported by §5.3's verify path and by the read when the
  recomputed total disagrees.
- `while_backfilling` stays read-time, derived from `indexed_through_block`
  against the checkpoint the answer carried.

This renames a public enum variant and changes `docs/openapi.json`: the wire
value `"from_birthday"` (`docs/API.md:156,377,410-418`) becomes
`"incomplete"`, and `"complete"` becomes `"verified_opening_balance"`. Pre-release
policy permits it (`docs/SYSTEM_REQUIREMENTS.md:27`), but it is a wire change and
belongs in its own step.

### 5.5 `block_credit` and `unmatched_spend` — confirmed, with two caveats

The brief asked me to confirm the spec's two mappings against the code. Both
hold; each has one caveat that must survive.

**`block_credit` → `balance_movement` without `transaction_id`: confirmed.**
`BlockCredit` (`block.rs:34-44`) is `{ id, asset, amount, to }` — one address,
one asset, one amount, an identity derived from canonical chain data
(`withdrawal:<consensus index>`), and no counterparty, fee or status
(`balance.rs:204-221`). It is a credit to one address with no transaction, which
is precisely a `balance_movement` row with `transaction_id IS NULL`, `direction
= credit`, `kind = Withdrawal`. The spec's "Validator withdrawals are
withdrawals, not minting" is satisfiable: `MovementKind` gains a `Withdrawal`
discriminant rather than reusing `Mint`.
**Caveat:** the disjointness argument at `balance.rs:212-221` — a credit cannot
double-count against a transaction in the same block "because the two are
disjoint by construction: `apply` reads only `CanonicalTransaction::movements`
and the transaction's fee, a credit is in neither, and no chain can express one
as the other" — **stops being structural** once both are rows in one table. It
becomes an identity argument instead: `movement_id` is unique within its block
for credits (`block.rs:80-96`) and within its transaction for movements
(`observation.rs:370-371`), and `transaction_id` NULL-vs-set separates the two
namespaces. That is sound, but it is a *different* proof, and
`docs/design/021 §4.4`'s refusal of a synthetic transaction identifier was
argued from the structural version. Worth stating in the step, not just
assuming.

**`unmatched_spend` → `output` with spending known before creation:
confirmed**, and §3.6 works through the upsert that makes the property
structural. **Caveat:** tracked-spend suppression, §3.6's last paragraph. This
is the single highest-risk detail in the schema change.

### 5.6 `transaction_order` changes a public cursor

**Found, not in the brief.** The spec requires "deterministic
transaction/movement ordering within blocks". Ordering *is* deterministic today
— `ORDER BY height, transaction_id` (`sqlite/history.rs:122`), declared on
`CanonicalPage` as "ascending by `(height, transaction id)`"
(`history.rs:199-207`) — but it is not *native block order*. Adding
`transaction_order` and paging by it changes:

- `HistoryPosition { height, transaction }` (`history.rs`), which is the opaque
  `cursor` in `GET /v1/wallets/{id}/transactions`;
- `docs/API.md:404-406`, which promises "The cursor carries a position and
  nothing else … a cursor stays valid however long a caller holds it. There is
  no `409` on paging";
- `docs/openapi.json` (sha `dd1a2163`).

**Cost of adopting it.** A cursor issued before the change and presented after
resolves to a different row. Pre-release policy allows the break; the wire
change must be deliberate.

**Cost of not adopting it.** Native order is unrecoverable once discarded, and
two facts want it: `transaction_address`'s page order (§3.4) and Solana, where
`transaction_id` ordering within a slot is meaningless to a caller.

**Recommendation: store `transaction_order`, page by it, and say so in
`docs/API.md`.** It is one `INTEGER` and the interpreter already has it —
`base::Transactions` preserves block order (`sdk/base/src/block.rs:108-138`,
"Iterates in block order"). Storing it is cheap; recovering it later is not.

### 5.7 A persisted balance on a row that does not exist yet

**Found, not in the brief. This is the deepest conflict in the document.**

History rows are written for **every** movement endpoint, watched or not.
`sqlite/blocks.rs:135-145`:

> "A newly watched address routinely already has rows in its own past: a history
> row is written for *every* endpoint of every movement, so the address has one
> wherever it was somebody else's counterparty (`docs/design/024` section 1.4)."

Nothing in `apply`/`refill`/`rows` (`sqlite/blocks.rs:107-203`) consults the
watch list, and `Memory` does the same (`testing/commits.rs:139-149`). So the
store holds movements for addresses with no `watch` row.

Now take the spec's rules together:

- "Insert a new watch with no inspection progress."
- "Forgetting and re-observing must not double-count retained movements."
- `unwatch` "deletes no indexed row: the history, the live outputs and the
  credits of the address stay" (`docs/SYSTEM_REQUIREMENTS.md:84-88`).

A fresh `watch` row therefore starts at `balance = 0` while the store already
holds that address's movements — from its time as a counterparty, or from a
previous watch that was withdrawn. Its balance is wrong from the first read, and
stays wrong: forward commits add only *new* movements.

Three ways out:

1. **Fold on insert.** `WatchStore::add` computes the total over retained
   movements and seeds `balance`. Correct, but it makes registration cost
   O(retained history) and puts a fold back inside `WatchStore` — the one port
   the spec wants free of queries, and the one `examples/custom_store` must be
   able to implement without a query API (§7 obligation 11). It would break
   obligation 11 outright.
2. **Seed zero and let the backfill correct it.** Wrong for a counterparty
   address whose movements are all *below* its birthday: the backfill inspects
   from the birthday, never re-reads those blocks, and the movements are already
   stored, so `INSERT OR IGNORE` reports zero new rows and contributes zero
   delta. The balance stays permanently short with `balance_basis = incomplete`,
   which is honest but useless — and it silently disagrees with the history the
   same store returns, which is the exact property `balance.rs:1-8` was designed
   to guarantee.
3. **Keep the fold as the seed, computed outside `WatchStore`.** The repository
   seeds `watch.balance` in its own registration path if it has a query
   capability, and leaves it zero with `basis = incomplete` if it does not.

**Recommendation: 3, stated as a repository-level rule rather than a
`WatchStore` obligation.** `WatchStore::add` keeps inserting with no progress
and no balance claim; a repository that *also* implements the balance query
seeds the row in the same transaction. A query-less sink inserts zero and
reports `incomplete` — which is true for it, because it cannot answer balance at
all. This keeps obligation 11 intact and makes the seed a capability rather than
a requirement. It is still the largest correctness risk in the plan, and step 4
in §6 is marked accordingly.

### 5.8 Transactionless credits in activity results break a documented contract

**Found, not in the brief.** The spec says "Transactionless movements must
appear in the repository's activity results." They do not today, and their
absence is documented behaviour:

- `docs/API.md:156`: "a validator withdrawal address can report a balance while
  `GET /v1/wallets/{id}/transactions` is empty, because that page lists
  transactions and a withdrawal is not one".
- `overlay.rs:276-281`: "It is the one thing reported here that the pending half
  of a *history* page cannot show, because that page lists transactions and a
  credit is not one (`docs/design/021` section 4.4)".
- `block.rs:22-27` refuses a synthetic `TransactionRef` for a credit.
- Mechanically: `block_credit` has exactly one reader, `sqlite/credits.rs:122`
  called only from `sqlite/balance.rs:74`.

**Cost of adopting the spec.** `TransactionPage` currently holds
`CanonicalTransaction`s, whose `transaction_id: TransactionRef` is
non-optional (`observation.rs:226`). An activity result that includes credits
needs either a nullable identifier — reintroducing precisely the synthetic
identifier `docs/design/021 §4.4` refused, and which `block.rs:22-27` argues
makes a credit indistinguishable from a transaction to every reader and every
join — or a second item shape on the page. The second is a wire change:
`docs/openapi.json`'s transaction item gains a variant.

**Cost of not adopting it.** A withdrawal-funded address's balance moves with no
visible cause. That is the spec's complaint and it is legitimate.

**Recommendation: adopt it as a *separate* activity concept, not by widening
the transaction page.** The repository's activity query returns
`balance_movement` rows directly — which is what the spec's table is for — and
the transaction page stays transactions. `apps/api` then has two reads where it
has one; whether it exposes both is an API decision, not a schema one. This
defers the wire change rather than forcing it into the persistence step, and it
keeps `docs/design/021 §4.4` intact. It is the one place in this document where
the spec's requirement is met by adding a query rather than by changing an
existing one.

### 5.9 `Indexer: Indexing + Balances` collapses

**Found, minor but contract-visible.** `indexer.rs:83-85` is
`pub trait Indexer: Indexing + Balances {}` with a blanket impl, and
`index.rs:330-337` is `impl Balances for Index<C>`. Removing `Balances` from
indexing makes `Indexer` identical to `Indexing`, so `Indexer` should be deleted
rather than kept as an alias — `linter.toml:209` (`rust/redundant-marker`) would
flag it. Its doc already anticipates the shape (`indexer.rs:75-82`: "History is
not here … routing a read through an indexer to reach a store's rows would put
the pagination model back into the contract a sink implements"); the same
argument now applies to balance. Cost: `Indexer` disappears from
`sdk/indexing/src/lib.rs:67` and from three chain `Store` bounds
(`bitcoin/src/config.rs:32-35,45-47`, `ethereum/src/config.rs:32-33,43-44`,
`solana/src/config.rs:32-33,43-44`).

### 5.10 `WatchStore`'s method count and the lint exception

**Found, procedural.** `linter.toml:142` is `max_methods = 3`;
`WatchStore` has four and carries
`// linter:disable rust/trait-method-count -- re-anchoring must be one transaction.`
(`storage/watch.rs:127`). Nothing in this plan adds a fifth, and §5.1's
narrowing of `reanchor` keeps it at four. Stated so the step does not
accidentally reach for a fifth method to seed a balance (§5.7 rules that out
independently).

---

## 6. The plan

Ordered so each step is independently green. Budgets are production lines,
signed; tests are counted separately because several steps are mostly tests.
**Steps marked ⚠ touch a correctness invariant** and name it.

| # | Step | Prod lines | Tests | Invariant |
|---|---|---|---|---|
| 1 | **Schema step 6, tables only.** Add `MIGRATIONS[5]` creating `watch`, `network_checkpoint`, `transaction_fact`, `transaction_address`, `balance_movement`, `output` + the partial index. Nothing reads them yet; `schema.rs` grows one entry and its commentary. | +150 | +20 | — |
| 2 | **Copy and drop.** Extend step 6 with the copy SQL (§2) and `DROP` of the seven old tables. `watch.balance` seeded by the fold. | +130 | +60 | ⚠ every old row maps; a lost row is silent |
| 3 | **Repoint the write path.** `sqlite/blocks.rs` `refill`/`rows` write one `transaction_fact` + N `transaction_address` instead of N `history`; `insert_movements` writes `balance_movement` per address per direction; credits become `balance_movement` rows. | +40 / −120 | +40 | ⚠ the fee row is written once per payer, not once per address (§3.5); credit/movement identity namespaces stay disjoint (§5.5) |
| 4 | **`watch.balance` maintained on write.** Balance delta accumulated from insert-reported novelty; written in the commit transaction with movements and progress. `Ledger` retained as the offline fold + verify path (§5.3). Registration seeds from the fold where the repository can (§5.7). | +120 / −20 | +90 | ⚠ a retry or a rediscovery must contribute zero; a fresh watch over retained movements must not read short (§5.7) |
| 5 | **Fold `watch_progress` into `watch`.** Delete the FK, the cascade and `Tables::registered`. `reanchor` narrowed to a monotone raise of `(birthday, indexed_through)` only (§5.1). | −60 | +30 | ⚠ expected-progress check preserved (`sqlite/progress.rs:60-62`); repeat-settles-uncertainty preserved |
| 6 | **Merge `live_output` + `unmatched_spend` into `output`.** Spend becomes an `UPDATE` setting `spent_*`; create becomes an upsert that fills creation only where it is NULL. Delete `unmatched`/`record_miss`. | +50 / −80 | +40 | ⚠ tracked spends still record nothing (§3.6); creation never erases spending |
| 7 | **`transaction_order` stored and paged by.** `HistoryPosition` gains it; `docs/API.md` and `docs/openapi.json` updated. | +45 | +25 | ⚠ cursor stability contract changes (§5.6) |
| 8 | **`balance_basis` replaces `Completeness`.** Rename the two variants, persist the basis + evidence, keep `while_backfilling` read-time, move `Overdrawn` to the verify path. `chain_ethereum::BalanceWitness` writes the attestation. Wire change. | +90 / −40 | +50 | ⚠ `Complete` must not be reachable without evidence; `Overdrawn` must stay reachable |
| 9 | **Move the query implementations out.** `sqlite/{balance,history,outputs,credits}.rs`, `ConfirmedHistory`, `BalanceStore`, `OutputStore`'s repository half, `Purging`, and their vocabulary change crate. `OutputStore`'s decorator and both gates stay (§4.3). | ±1200 moved, ≈0 net | ±400 moved | ⚠ `Service::spendable` keeps its input and its caller (§4.3) |
| 10 | **Move `Balances` out.** `Ledger`, `Balances`, `impl for Service`, `impl for Index`, `Indexer` deleted (§5.9). `apps/api` and four examples hold `Arc<dyn Balances>` composed per network, mirroring `Histories`. | −300 from indexing, +90 in the consumer | +60 | — |
| 11 | **Export `PendingFacts`; move `ObservedHistory`.** `Sample` + per-address `inspected_through` as one snapshot; the reader's ~200 lines leave indexing; three chain configs change a type. | −200 from indexing, +210 in the consumer, +40 for the snapshot | +50 | ⚠ sample-then-narrow ordering must be type-enforced (§5.2) |
| 12 | **The activity query.** Repository query over `balance_movement`, so transactionless credits reach activity results without widening the transaction page (§5.8). | +110 | +50 | — |

**Totals.** Net production: **−175 inside `sdk/indexing`** (steps 1-8 add ≈ +365
and remove ≈ −320 in the persistence layer, then steps 9-11 move ≈ 1700 lines
out and delete ≈ 500 of them), **+410 outside it**, plus **+915 test lines**.
Roughly **1700 lines relocated, 500 deleted, 700 written**. The honest summary:
this is not a reduction. The spec's schema is genuinely smaller in duplication
(§3.3) and genuinely deletes machinery (§3.6, §5.1's narrowing, step 5's FK),
but §5.3's persisted balance *adds* a verify path that derive-on-read did not
need, and §5.8's activity query adds a query that did not exist. Steps 1-8 are
the persistence change and are worth doing on their own; steps 9-12 are the
ownership change and are worth doing on their own; **the two are independent and
should not be one branch.**

---

## 7. Proof obligations

The spec names eleven. Each mapped to where it would be tested, with what exists
today.

| # | Obligation | Where | Today |
|---|---|---|---|
| 1 | atomic movement/balance/progress writes | `testing/commits.rs` + a new `BalanceContract` scenario, run against both stores via `tests/repository_contract.rs:441-449` and `sqlite/progress.rs:132-159` | partially: `sqlite/progress.rs:228` `a_refused_batch_rolls_back_its_movement_rows_too`; `sqlite/blocks.rs:566` `a_refused_commit_leaves_every_table_as_it_was`. **Balance is new** |
| 2 | duplicate retries and historical rediscovery without double counting | `testing/balance.rs` and `testing/backfill.rs`, both already redo a batch | exists for the *fold*: `BackfillContract::fills_in_a_credit_no_transaction_names` (`testing/backfill.rs:407`) asserts a backfilled credit counts once on redo. **Must be re-asserted for the persisted total** (step 4) |
| 3 | empty-block progress | `testing/watch.rs` / `testing/backfill.rs` | exists by contract (`storage/watch.rs:28-30`) and by the claim guard; assert it against `watch.indexed_*` after step 5 |
| 4 | honest balance completeness with arbitrary birthdays | `testing/backfill.rs`, `tests/backfill_contract.rs` (940 lines, whose whole subject this is) | exists: `reads.rs:610` `a_history_page_never_claims_completeness_while_a_backfill_is_outstanding`. **Extend to `balance_basis`** (step 8) |
| 5 | registration repeats, conflicting definitions, re-observation | `testing/watch.rs` (`WatchContract`, `testing/watch.rs:24`) | exists for `add`; **re-observation over retained movements is new** and is §5.7's risk (step 4) |
| 6 | same address with multiple assets | `testing/watch.rs`, `testing/balance.rs` | exists: durable key is already `(asset, address)` (`schema.rs:37`), and `testing/balance.rs:122` asserts per-asset isolation of a credit |
| 7 | transactionless credits and failed-transaction fees | `testing/balance.rs:122`, `testing/history.rs:97,116` | both exist. **New**: the credit must also appear in the activity result (step 12), which is §5.8's contract break |
| 8 | historical creation never resurrecting spent outputs | `testing/spends.rs:180` `a_reported_miss_outlives_the_block_that_reported_it`, plus `spends.rs:217` | exists and already runs the batch twice (`spends.rs:201-210`). **Add**: a tracked spend still writes no row (§3.6) |
| 9 | restart, network isolation, pending/confirmed reorg | `tests/service_contract.rs` (2268 lines), `sqlite/blocks.rs:423` `purge_network_removes_only_its_own_network`, `sqlite/watches.rs:442` `registrations_survive_reopening_the_file` | exists; re-run unchanged, which is the regression check on steps 1-6 |
| 10 | offline maintenance | `apps/server/src/rescan.rs` + `testing/purge.rs` (`PurgeContract`, `purge.rs:19`) | exists (`purge.rs:54` counts credits). **Add**: the verify/recompute path from §5.3 |
| 11 | a custom repository requiring no balance/history/output methods | `examples/custom_store/sink.rs` + `examples/tests/custom_store.rs` (547 lines) | **already satisfied and must stay so.** `sink.rs:119` `WatchStore`, `sink.rs:279` `IndexStore`, and `sink.rs:10-16` states it implements none of the other three. §5.7's recommendation exists specifically to keep this true: seeding a balance inside `WatchStore::add` would require a fold and break it |

**One obligation the spec does not name and this plan needs.** The `testing::*`
suites are a **publicly exported conformance suite** behind the `testing`
feature (`sdk/indexing/src/lib.rs:33-34`), with six entry points —
`WatchContract` (`testing/watch.rs:24`), `HistoryContract`
(`testing/history.rs:25`), `BalanceContract` (`testing/balance.rs:34`),
`BackfillContract` (`testing/backfill.rs:28`), `SpendContract`
(`testing/spends.rs:23`), `PurgeContract` (`testing/purge.rs:19`) — run against
both `testing::Memory` (`tests/repository_contract.rs:441-531`) and the real
file (`sqlite/blocks.rs:395-431`, `sqlite/balance.rs:95-115`,
`sqlite/progress.rs:132-159`, `sqlite/watches.rs:409-420`), and reused
downstream by `sdk/wallets` (`sdk/wallets/Cargo.toml:16-18`). Steps 9-12 move
five of those six contracts to another crate. **Their bounds must move with
them** — `BackfillContract` requires `BalanceStore + OutputStore + Repository +
ConfirmedHistory` (`testing/backfill.rs:32`), four traits that would then span
two crates — or `sdk/indexing`'s own tests lose their write-path coverage.
That is a dependency-direction question step 9 must answer before it starts, and
it is the one thing in this plan that could force a third crate.
