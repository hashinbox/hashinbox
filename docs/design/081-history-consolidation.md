# 081 — History consolidation, in iterations

Status: proposal. Design only; nothing built. Supersedes 079. 079 keeps the
measurements (§1) and the per-chain research (§3.2–3.4); this doc does not
repeat them.

Paths are relative to the repository root. Line numbers are from today's
tree, including the uncommitted endpoint-routes diff.

## 1. Goal

1. **One flow for every chain.** Chains supply one small primitive; numbers
   per network are settings, not code paths.
2. **Bounded catch-up.** Past `max_backfill`, skip to the final tip and read
   live from there; one balance read closes the gap.
3. **An append-only `history`** per wallet index is the durable accounting
   record: opening, movements, adjustments.
4. **Raw rows are disposable.** `block`, `transaction` and `leg` rows that
   are final and accounted may be deleted.
5. **Four small iterations,** each shippable on its own.

Why: Solana devnet is 344k slots (38 h) behind and never catches up on
public nodes (079 §1.2–1.3). Block rows grow ~93 MB/day there, half of them
empty (079 §1.4). Watch-only wallets start at balance 0 (079 §1.1).

---

## 2. The model

### 2.1 One table: `history`

One row is one **final** change of one wallet index's balance. An index is
one asset of one address (`sdk/base/src/wallet.rs:290-306`), so a row is per
wallet index and asset.

```sql
-- Append-only. Rows are written only for final blocks, in (position, seq)
-- order, and never updated; they go only with their wallet index.
-- id: "{wallet_index_id}.{seq}".
CREATE TABLE history (
    id              TEXT    NOT NULL PRIMARY KEY,
    wallet_index_id TEXT    NOT NULL REFERENCES wallet_index (id) ON DELETE CASCADE,
    seq             INTEGER NOT NULL, -- 1, 2, … per index.
    kind            INTEGER NOT NULL, -- leg kinds 0–5 (received, spent, burned,
                                      -- minted, fee, reward), 6 opening, 7 adjustment.
    amount          TEXT    NOT NULL, -- Signed exact decimal.
    total           TEXT    NOT NULL, -- The balance right after this row.
    total_received  TEXT    NOT NULL, -- Observed in (kinds 0, 3, 5).
    total_sent      TEXT    NOT NULL, -- Observed out (kinds 1, 2, 4).
    position        INTEGER NOT NULL, -- The block's; opening/adjustment: R − 1.
    time            INTEGER,          -- The block's time, Unix s; kept after the block goes.
    txid            TEXT,             -- Source transaction; NULL: reward, opening, adjustment.
    leg             TEXT    UNIQUE,   -- Source leg id; no FK, legs are disposable.
    CHECK (id = wallet_index_id || '.' || seq),
    CHECK (kind BETWEEN 0 AND 7),
    CHECK ((kind >= 6) = (leg IS NULL)),
    UNIQUE (wallet_index_id, seq)
);
CREATE INDEX history_at ON history (wallet_index_id, position, seq);
```

- **Link to the source transaction while it exists:** `transaction.id =
  chain || '.' || network || '.' || txid`. Once the row is pruned, `txid`
  still gives the explorer link (`network.explorer_transaction`).
- **`leg` makes accounting idempotent.** A leg is accounted exactly when a
  history row names it.
- **Opening and adjustment rows sit at `R − 1`,** where `R` is the first
  position read (§3.3). Every `total` from there on is the true chain
  balance at that position, not just ours.
- **`total = total_received − total_sent + Σ opening/adjustment`.** The
  observed in and out stay observed, as 079 wanted.

### 2.2 Balances derive from it

```
balance at H = total of the index's last history row at or below H   (one seek, history_at)
             + Σ signed amounts of its live mined legs above that row's position, at or below H
```

- **The tail is short.** Only legs not yet final are unaccounted: at most the
  finality depth (Solana 32 slots, Ethereum ~64–95 blocks, Bitcoin a few).
- **`wallet_index_with_balance`** (`apps/server/init.sql:239-310`) swaps its
  `c` and `n` leg seeks for this. `balance_confirmed_spendable` and
  `balance_pending` keep their pending-leg sums
  (`init.sql:254-286`).
- **While an index is `opening` (§2.4) its balances are NULL.** Its gap is
  not accounted yet, so no number beats a wrong one.

### 2.3 How it relates to the existing tables

| Table | Role after this design | Change |
|---|---|---|
| `block` | Scan state near the tip, plus the restart anchor (§3.3). Disposable once final and unreferenced. | None in iterations 1–3; pruned in 4 |
| `transaction` | Pending sends and the recent mined tail. Disposable once mined, final and leg-less. | Pruned in 4 |
| `leg` | Raw facts: pending legs, the unaccounted tail, and coins (UTXO). Disposable once accounted, unless it holds an unspent coin. | Its `total*` columns go in 4 (`init.sql:89-104`, 072) |
| `wallet_index` | The scan cursor (`indexed_at`) as today. `state` gains `3 = opening`. | One CHECK value (`init.sql:217`) |
| `history` | The durable record and the balance base. | New |

No accounting cursor column: the index's last history row is the cursor.

### 2.4 State `opening` (3)

- **Scanned like `indexing`** (`advance`, `find_next`:
  `apps/server/src/models/wallet_index.rs:468-545` read `state IN (0, 3)`).
- **Its history is held below `birthday`.** Legs from `birthday` on are kept
  but not accounted.
- **It ends with one balance read** at a final block `S ≥ birthday` (§3.3).
  That appends one `opening` row (the index's first) or `adjustment` row at
  `birthday − 1`, then accounts the held legs, then sets state 0.

### 2.5 What replaces the 079 tables

| 079 | Replaced by |
|---|---|
| `wallet_index_adjustment` | `history` rows of kind 6 (opening) or 7 (adjustment). The read's balance is derivable: `balance(S)` = total of the last row ≤ S. |
| `network_gap` | Nothing durable. A gap is visible as the adjustment row at `R − 1` after the previous row, and as `requested_from ≠ birthday`. The resume point is one ordinary `block` row with `parent` NULL, the **anchor** (§3.3). The "never archive below the gap" rule becomes one generic rule: never archive a final block (§3.5). |
| `wallet_index.gap_from/gap_to` | `birthday` is `R`; the previous history row tells where the gap began. |

One small thing stays: a `skips` counter on `network_index`, so constant
skipping is an alarm (add endpoints), not silent.

### 2.6 `last_seen` on `wallet_index`: no

- **Nothing in the flow decides on it.** The gap path needs no quiet window:
  the read at `S` is exact whatever the wallet did (079 §3.1). A skip moves
  every lagging index; activity cannot exclude one, because gap activity is
  exactly what we did not see.
- **It would be a second copy** of the last history row, written on every
  leg, and wrong after a reorg of the tip unless also rewound.
- **For display,** "last activity" is the `time` of the last history row
  (or of the newest tail leg): the seek §2.2 already does. Expose it from the
  view if the UI wants it; no column.

---

## 3. The one flow

### 3.1 Normal indexing writes the history

1. The indexer keeps blocks and legs as today (`BlockStore::insert`,
   `sdk/base/src/block.rs:519-546`).
2. In the same write that records the step (`network_index` save,
   `apps/server/src/usecase/workers/index.rs:155-163`), **account** through
   `F`, the chain's own final tip (`Tips::confirmed`,
   `sdk/base/src/block.rs:226-231`, already in `Stats::confirmed`):
   - per index in state 0, every live mined leg at or below `F` with no
     history row, in (position, seq) order;
   - each becomes one row, its totals carried from the previous row (the
     loop of `apps/server/src/models/block.rs:279-299`).
3. Indexes in state 3 are skipped: their history waits for the read.

### 3.2 Consolidation: which raw rows go

Iteration 4. Per network, in one write of the existing prune job
(`apps/server/src/usecase/workers/prune.rs`):

| Row | Deleted when every condition holds |
|---|---|
| `leg` | Mined, its position ≤ `F`, and a history row names it. Also an unaccounted leg below its index's last history row: a later adjustment already covers it (a restart over an index still in state 3). **Not** a `received` coin no final `spent` leg consumes. A coin's `received` and `spent` legs go together, so `find_unspent` (`apps/server/src/models/leg.rs:471-520`) never sees a spent coin alone. |
| `transaction` | Mined at ≤ `F`, and no leg left. Pending ones always stay. |
| `block` | Position ≤ `F`; no leg, transaction or `wallet_index.indexed_at` references it; below every index's `next()` (so `advance` never mistakes a pruned range for "no block since birthday", `wallet_index.rs:527-536`); not the network's last live block. Kept children get `parent = NULL` first. |
| archived rows | Archived blocks (reorged) and their facts below `F`: out of every read already. |

Effects:

- Solana: ~93 MB/day of block rows gone (079 §1.4).
- The movements list reads `history` ∪ the tail legs ∪ pending legs. It is
  switched in iteration 1, so pruning changes no reader.
- `leg.total`, `total_received`, `total_sent` go. `find_running`
  (`leg.rs:354`) seeds from the last history row instead.

### 3.3 The gap path

Setting: `network.max_backfill` (positions, like `confirmations`; NULL:
never skip). The indexer compares positions; the UI shows it as time
(× `block_time_ms`).

At the start of `Indexer::fetch` (`sdk/base/src/indexer.rs:241`):

1. `start` = the lowest `next()` (as today). If `F − start > max_backfill`
   and the chain has a balance reader:
2. **Account first.** Every leg ≤ `F` of every state-0 index is in history.
3. **Restart, in one store write** (`BlockStore::restart(anchor)`):
   - keep the header of block `F` as the **anchor**: a `block` row with
     `parent` NULL and no facts;
   - every index of the network in state 0 or 3 with `next() ≤ F` gets
     `birthday = R = F + 1`, `indexed_at = NULL`, `state = 3`;
     `requested_from` stays.
   `Stats::skips` counts it.
4. The next step starts at the anchor (`find_last`), sees it on the chain,
   and reads every block from `R` live. No other indexer change.
5. **Read.** For each state-3 index caught up to `F`, at most K per step:
   `balance(address, asset, from = birthday, through = its indexed_at)`
   answers `(S, hash, amount)`, final, `birthday ≤ S ≤ through`.
6. **Settle, in one write** (`IndexStore::settle`): check our live block at
   `S` has `hash`; append the row at `R − 1`:

   ```
   amount = balance(S) − (last history total + Σ legs in [R, S])
   kind   = opening if the index has no row yet, else adjustment
   ```

   then account the held legs `[R, …≤ F]`, then `state = 0`.
7. A failed or early read (`None`) leaves the index in state 3 and counts in
   `Stats`. It never fails the step.

Example (079 §2): history shows 1 BTC received and 0 sent, the chain says
0.5. The adjustment is −0.5. `total_received` stays 1, `total` reads 0.5.

### 3.4 Adding a wallet with a birthday

Same steps 5–7, no skip.

- **Generated key** (no past): starts in state 0, balance 0 before its
  birthday is true. `Birthday::Fresh` becomes `Birthday::New` for it
  (`indexer.rs:31-36`, `:135`).
- **Watch-only or imported, any birthday,** on a chain with a balance
  reader: starts in state 3. Its opening row at `birthday − 1` is the
  balance it held before we looked. This fixes the zero start (079 §1.1).
- **Chain without a balance reader** (Bitcoin): state 0, as today.
- An explicit start older than `max_backfill` is skipped like any lagging
  index (Q2).

### 3.5 Reorgs

- **Only final rows are accounted** (≤ `F`, or `S` the chain calls final).
  A reorg near the tip archives legs above `F` as today
  (`BlockStore::delete`, `block.rs:557-571`); history is untouched.
- **Only accounted, final rows are pruned.**
- **The anchor and `S` are final.**
- **Archiving a final block is an error, not a rewind.** `fetch` refuses
  `delete(from ≤ F)` (`indexer.rs:272`), and the server's
  `BlockStore::delete` refuses to archive a leg that history names. A node
  reorging a final block is a node bug; it fails loudly. This one rule
  replaces 079's per-gap guard.

---

## 4. Chain primitives

One trait, optional per chain like `Dated` (`sdk/base/src/config.rs:107`):

```rust
// sdk/base/src/balance.rs
/// One asset of one address as the chain holds it at a final block.
pub struct Holding {
    /// S: final by the chain's own rule.
    pub position: u64,
    /// The chain's hash at S; the store checks our live block there.
    pub hash: BlockHash,
    /// Atomic amount.
    pub amount: Decimal,
}

pub trait Balances: Networked + Send + Sync {
    /// The balance at a final block S, `from <= S <= through.position`.
    /// `None`: not answerable in that range yet; try again next step.
    fn balance<'a>(
        &'a self,
        address: &'a Address,
        asset: &'a Asset,
        from: u64,
        through: &'a BlockHeader,
    ) -> Future<'a, anyhow::Result<Option<Holding>>>;
}
```

`Service` gains `balances: Option<Arc<dyn Balances>>`. A network with
`max_backfill` set and no `balances` fails to build: a setting the chain
cannot honour is a config error.

| Chain | `balance` | Calls |
|---|---|---|
| Solana | `getBalance`, or the sum of `getTokenAccountsByOwner{mint}` (finalized, `minContextSlot = from`). `S = context.slot`; `None` if `S > through`. Then `getBlock(S, none, finalized)` for the hash (232 B). Keep `context`, dropped today (`sdk/chains/solana/src/rpc.rs:412-416`). | 2 |
| Ethereum | The `finalized` header, then `eth_getBalance` or `eth_call balanceOf` at its `blockHash` (EIP-1898). Works while live: full nodes keep ~128 blocks of state. | 2 |
| Bitcoin | **None for now.** | — |

**Bitcoin defers the gap path and the opening.**

- Spending needs coins, not a number (`InputStore::find_unspent`,
  `sdk/base/src/inout.rs:162-169`). An opening amount cannot be spent.
- `scantxoutset` cannot pin a block and is often disabled (079 §3.2, §3.4).
- So Bitcoin keeps `max_backfill = NULL`, starts watch-only at 0 as today,
  and gets iterations 1 and 4 in full.
- **UTXOs under pruning:** a coin's `received` leg, its transaction and its
  block stay until a final `spent` leg consumes it (§3.2). History holds
  the amounts; legs hold the coins.
- If ever needed: the scan's coins become opening `received` legs plus one
  `opening` row (Q6).

---

## 5. Iterations

Order: **history, opening, gap, prune.** Each needs only the ones before it.

| # | What | Ships | Tests | Unlocks |
|---|---|---|---|---|
| **1** | `history` table; accounting in the step write (§3.1); `wallet_index_with_balance` and the movements list read history + tail; server `delete` refuses accounted legs. **No `sdk/base` change.** | Server only; `init.sql` fresh file | After every step, each history row's `total*` equals its leg's `total*` (two truths, one check); a tip reorg leaves history; a reorg into accounted legs fails; triggers see the same balances as before | The durable record; every later iteration writes into it |
| **2** | `Balances` + Solana and Ethereum adapters; state `opening`; `IndexStore::settle`; reconcile in the step (§3.3 steps 5–7); `Birthday::New`; triggers select `state = 0` (`wallet_index.rs:713`) | `sdk/base` (approval), chains, server | Fake chain: watch-only with balance 5 shows an opening row of 5 − Σ legs; generated key gets none; read at `S > through` retries | Watch-only wallets show the true balance (zero-start bug gone); the read + row the gap path reuses |
| **3** | `network.max_backfill`; `BlockStore::restart` (anchor); `Next::Skipped`; never archive ≤ `F`; `network_index.skips`; Solana 9,000 (1 h), others NULL | `sdk/base` (approval), server, seed | Fake chain 20k behind with M = 9k: one skip, anchor at `F`, every index state 3, then an adjustment closes each; a busy wallet reconciles like an idle one | Solana catches up after any outage; triggers run again (Idle returns, `index.rs:169-173`) |
| **4** | Prune job (§3.2); drop `leg.total*`; `find_running` seeds from history | Server only | Prune then compare every balance and the movements list with before; unspent coins still spendable; pruning twice is a no-op | ~93 MB/day of Solana block rows gone; raw rows become a short tail |

Why this order:

- **Opening before gap.** The birthday opening is the gap path with the gap
  `(−∞, birthday)`. It brings the primitive, the state and the close write
  without touching the indexer's forward path. The gap then only adds
  `restart`.
- **Prune last.** It deletes the raw rows that check history. Iterations
  1–3 run with both, and the equality test in 1 keeps proving history
  right. Prune once adjustments have run in production too.
- **Prerequisite, no code (079 §4.3):** route `getBlock` over ≥ 2 Solana
  endpoints so live reading (r ≥ 2.5/s) holds. Without it, iteration 3 skips
  over and over: balances stay right, history gets one adjustment per hour.

`sdk/base` changes needing approval (core-crate rule):

- **It. 2:** `balance.rs` (`Balances`, `Holding`), `Service::balances`,
  `IndexState::Opening`, `IndexStore::settle`, `Birthday::New`, the read in
  `Indexer::next`.
- **It. 3:** `Indexer::max_backfill`, `Next::Skipped`, `Stats::skips`,
  `BlockStore::restart`, the doc on `Store::delete` ("never final").
- **It. 1 and 4:** none.

---

## 6. Open questions

| # | Question | Recommendation |
|---|---|---|
| Q1 | Approve the `sdk/base` additions above? | Yes, per iteration. Each is optional per chain, like `Dated`. |
| Q2 | An explicit start older than `max_backfill`: skip or refuse? | Skip, the one rule for every index. `requested_from` against `birthday` shows the shortening (070 §2). A backfilling index would hold the network's forward path anyway (`indexer.rs:241-261`). |
| Q3 | One history row per leg, or per (transaction, index)? | Per leg: 1:1, idempotent by leg id, no merge rules. Rows are small and grow only with wallet activity. |
| Q4 | Our pending sends mined inside a gap stay pending until `expires_at` (`init.sql:77-78`). | Accept in iteration 3: spendable is under-stated, never over-stated. Later: at close, check those txids and drop the mined ones. |
| Q5 | Re-read healthy indexes on a schedule (drift: Ethereum internal transfers, parser bugs)? | Yes, after iteration 3, as a per-network setting. A non-zero read appends an `adjustment` (079 §3.11); on Solana it is an alarm. |
| Q6 | Bitcoin opening and gap via `scantxoutset` coins? | Defer. Bitcoin's backlog after a day is 144 blocks; it never needs to skip. |
