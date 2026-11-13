# 079 — Skip and reconcile: balances across gaps we do not read

Status: superseded by 081 (`081-history-consolidation.md`); its measurements (§1) and per-chain research (§3.2–3.4) still apply.

The user's idea, condensed: give each network a maximum backfill. Past it,
stop reading every block and **consolidate**: take each wallet's balance once
it has been quiet for N blocks, and store it as an **initial balance**. Keep
history in an append-only table. When history and the true balance disagree,
add an **adjustment**.

Then the user's refinement: *"we can read blocks from when the server is
started, and when we do we can tell what wallet was quiet and which not."*

Verdict:

- The idea holds once it is sharpened, and that refinement is the key.
- Read every block from the restart point R on. Read the balance pinned to a
  final block S that we have indexed.
- The gap's net effect is then exact arithmetic: `balance(S) − our total at S`.
- No quiet window is needed, and no history is rewritten.

Paths are relative to the repository root. Line numbers are from today's
tree, including the uncommitted endpoint-routes diff.

---

## 1. The problem, measured

### 1.1 How the indexer spends its reads today

- **One forward path per network.** Each step starts at the lowest
  `next()` of any indexing wallet index, or else at the last kept block
  (`sdk/base/src/indexer.rs:241-261`, `apps/server/src/models/wallet_index.rs:468-500`).
- **Every block is read.** Solana lists slots with `getBlocks`, then makes one
  `getBlock` per slot in batches of 10, with `transactionDetails:
  "accounts"` (`sdk/chains/solana/src/rpc.rs:82-140`, `:31`, `:110-117`).
- **Every block is stored**, empty ones included, as one `block` row
  (`apps/server/src/models/block.rs:262-344`). A new block must link to the
  live block below it (`apps/server/src/usecase/sdk/blocks.rs:92-108`). A gap
  fails loudly.
- **A lagging index blocks everyone.** A backfilling index, such as a new
  wallet with an old start, holds the whole network's forward path at its
  position (`indexer.rs:244-257`).
- **Triggers wait for the tip.** They are queued only when a step is `Idle`
  (`apps/server/src/usecase/workers/index.rs:169-173`). A network that never
  catches up never runs a trigger.
- **`max_history` only limits a start.** It is "how old a watch's start may
  be" (`sdk/base/src/config.rs:31-32`, `:108-110`): an explicit start older
  than it is refused with 400 (`apps/server/src/api/wallet_indexes.rs:53-80`).
  It bounds nothing after a restart.
- **A fresh watch starts at zero.** `Birthday::Fresh` starts at the produced
  tip (`indexer.rs:134-137`). A watch-only address that already holds funds
  shows a balance of 0 plus the legs seen after that point. This is wrong
  today, and §5 fixes it too.

### 1.2 Numbers

Measured on 2026-11-12 with curl against the public endpoints. Database
figures come from `.data/payment.db`, read-only.

| | Solana | Ethereum | Bitcoin |
|---|---|---|---|
| Block time | 0.4 s (`seed.sql:16`), 2.5 slots/s | 12 s | 600 s |
| Positions per day | **216,000** | 7,200 | 144 |
| Read cost per block | devnet `getBlock` in accounts mode: **26–55 KB** (26,170 / 35,628 / 41,505 / 53,435 B). Mainnet in accounts mode: **1.55–2.66 MB** (1,551,970 / 1,775,523 / 2,659,011 B) | block ~0.3 MB plus receipts ~0.5 MB on mainnet (`docs/issues.md` I3) | `getblock` verbosity 3; a few MB |
| Bytes per day | devnet 5.6–11.9 GB; **mainnet ~335–575 GB** | ~5.8 GB | < 1 GB |
| Rate limit | Public, per IP: 100 req/10 s overall, **40 req/10 s per method**, 100 MB/30 s ([Solana clusters](https://solana.com/docs/references/clusters)). Observed: 429 on `getBlock` near 2.5 req/s. Tatum free tier: 3 rps | publicnode serves batches of 10 without limiting (`seed.sql:4-5`) | own node |
| Live load needed | **2.5 `getBlock`/s** plus about 1 `getBlocks` per 100 slots | 0.08 blocks/s | 0.002 blocks/s |
| Our index today | `network_index`: tip 521,442,873, indexed 521,098,696: **344,177 slots (38 h) behind**, 911 failures, reason `429 Too many requests` on `getBlock` | Sepolia 910 blocks behind | — |

Two facts stand out:

- **On mainnet the data cap is hit before the request cap.** 2.5 blocks/s ×
  ~2 MB is about 5 MB/s, more than the public 3.3 MB/s (100 MB/30 s). Reading
  every Solana mainnet block is impossible on public nodes, whatever else we
  do.
- **On devnet the request cap is the limit.** Bytes are small.

### 1.3 Catching up after a one-day outage on Solana

- The backlog is B = 216,000 slots.
- The net catch-up rate is r − 2.5, where r is our sustained `getBlock`
  requests per second.
- Catch-up time is B / (r − 2.5).

| Sustained r | Example | Catch-up time |
|---|---|---|
| 2.5 | public devnet as observed | **never**: the lag only holds still |
| 3 | Tatum free tier alone | 432,000 s = **5 days** |
| 4 | public nominal (40/10 s) | 144,000 s = 40 h |
| 5.5 | public plus Tatum, routed (§4.3) | 72,000 s = 20 h |
| 10 | 3–4 routed keys | 28,800 s = 8 h |

For contrast:

- **Ethereum:** one day is 7,200 blocks, about 720 steps of 10. That is
  minutes to an hour on publicnode.
- **Bitcoin:** one day is 144 blocks, a few minutes.
- **Only Solana has the problem.**

### 1.4 Database growth

Measured on the local Sepolia data: 15,454 block rows; 15,914 legs; 15,876
transactions.

| Row | Bytes per row, with its indexes | Measured from |
|---|---|---|
| `block` | **~430 B** | 4.54 MB table + 1.59 MB primary key + 0.52 MB `block_live` |
| `leg` | ~930 B | 7.48 MB + 5 indexes 7.38 MB |
| `transaction` | ~480 B | 4.35 MB + 3.33 MB |

- **Solana block rows:** 216,000 × 430 B ≈ **93 MB/day ≈ 34 GB/year**,
  almost all of them empty.
- **Legs stay small:** they grow only with the watched wallets' own activity.
- **48% of the Sepolia block rows** (7,435 of 15,454) hold no leg.
- **So database relief is about block rows, not wallet history** (§3.10).

---

## 2. The proposal, restated with every term defined

| Term | Definition |
|---|---|
| **max backfill** `M` | Per network, in positions. When an index's `next()`, or the network's forward path, is more than `M` below the confirmed tip, it is not read up to: it is **skipped**. NULL means never skip. |
| **restart point** `R` | Where reading resumes after a skip: the network's **confirmed** (final) tip at the moment of the skip (`Tips::confirmed`, `sdk/base/src/block.rs:225-231`). Every block from `R` on is read. |
| **gap** | The positions an index did not observe: from its old `next()` to `R − 1`. For a new wallet, from −∞ to `R − 1`. |
| **settle point** `S` | The final block at which the chain answers a balance read: `R ≤ S ≤ min(indexed_through, confirmed)`, and our live block at `S` has the chain's hash there. |
| **quiet window** | The user's idea: no leg in the last N blocks. **Not needed** (§3.1). Its useful core is that `S` is final and indexed. |
| **confirmed balance** | The balance at the confirmed tip. Today it is the last live leg's `total` at or below `finality_at` (`apps/server/init.sql:239-310`). |
| **consolidation** (renamed **reconcile**) | For one index after a gap: read `balance(S)` from the chain, compare it with our total at `S`, and append the difference as one adjustment. |
| **initial balance** | The balance at `R − 1`, equal to `balance(S) − Σ observed legs in [R, S]`. It is implied by the adjustment and not stored separately (§5.1). |
| **adjustment** | An append-only record per index: `amount = balance(S) − total(S)`, placed at position `S`. It carries the gap it explains. History, meaning legs and their `total_received` and `total_sent`, is never touched. |

The user's example: history shows 1 BTC received and 0 sent, the chain says
0.5. The adjustment is −0.5. `total_received` stays 1, and the balance reads
0.5.

---

## 3. Validation: where it holds, where it breaks

### 3.1 "No activity in the last N blocks means the balance is confirmed"

**As stated, it is unsound and unnecessary.**

- **Unsound.** To know there was no activity you must observe the window. You
  either read its blocks, or ask an address index (Solana
  `getSignaturesForAddress`, Ethereum `eth_getLogs`). A Solana token
  transfer into an existing ATA does not list the owner's address. So
  `getSignaturesForAddress(owner)` misses it, and you must also ask every
  token account. Ethereum has no RPC that lists native ETH received.
- **Racy.** "Balance now" and "quiet since" are two reads at two different
  heights. Activity between them is invisible.
- **Starves.** A busy hot wallet is never quiet for 32 slots.

**The stronger form holds.** It is the user's refinement made exact:

```
Every block in [R, S] is read            (we index live from R)
S is final, and our block at S is the chain's block at S
initial(R−1) = balance(S) − Σ observed legs of the index in [R, S]
adjustment   = balance(S) − total(S)      (total(S) = our running total, gap included)
             = initial(R−1) − total(R−1)
```

- **No quiet wait.** The legs in [R, S] are known, whatever the wallet did.
- **Busy wallets are fine.** A wallet active every slot reconciles as easily
  as an idle one.
- **What remains of "waiting":**
  - for `S` to be final and indexed: seconds when we are live;
  - on Bitcoin, for the scanned block to be confirmed (§3.4).

**Recommendation: the stronger form.** Quietness is dropped. What replaces it
is that `S` is settled: final, indexed, and the same hash as ours.

### 3.2 Pinning a balance read to S, per chain

| Chain | Read | Pinned how | Guarantee |
|---|---|---|---|
| Solana, native | `getBalance(addr, {commitment: "finalized", minContextSlot: R})` | Not to a chosen slot. The answer is at the node's root, `context.slot`, with `S ≥ R` (otherwise error -32016, retry). | A rooted slot is final. Then `S` must be ≤ our `indexed_through`; if not, retry next step. Our code drops `context` today (`sdk/chains/solana/src/rpc.rs:412-416`): keep it. |
| Solana, token | `getTokenAccountsByOwner(owner, {mint}, {finalized, minContextSlot: R, encoding: "jsonParsed"})` | Same, one `context.slot` for every account of the mint in one answer | The sum over all of the owner's accounts of the mint matches our leg rule: per owner, per mint, from `pre/postTokenBalances.owner` (`sdk/chains/solana/src/transaction.rs:352-389`). ATAs created or closed and other token accounts are included on both sides. |
| Solana, same fork | `getBlock(S, {transactionDetails: "none", finalized})`, 232 B measured | Its `blockhash` must equal our live block at `S` | We index at `confirmed` commitment (`rpc.rs:57-72`). This closes the rare case of a confirmed block that never finalized. |
| Ethereum, native | `eth_getBalance(addr, {blockHash: h(S), requireCanonical: true})` (EIP-1898) | Exactly our block, so the same fork by construction | `S = min(indexed_through, finalized)`, about tip − 64..95. A full node keeps state for about 128 blocks, so the read works only while we are live. Too old a block errors: retry with a newer `S`. |
| Ethereum, ERC-20 | `eth_call balanceOf` at the same `blockHash` | Same | Same |
| Bitcoin | `scantxoutset start [addr(...)]` | **Cannot pin.** It scans the UTXO set at the node's tip and answers `height`, `bestblock` and `unspents[]`. | `S` = that height. Accept it only if `bestblock` is our live block at `S`, then wait until `S ≤ confirmed` and the block is still live; otherwise rescan. One scan at a time, minutes on mainnet; many providers disable it. |

**Each asset reconciles on its own.** Each asset is its own wallet index
(`sdk/base/src/wallet.rs:290-306`) with its own `S`. Native and token reads do
not need one shared slot.

### 3.3 Tokens, ATAs, fees, rent

- **Fees.** In observed blocks, fees are legs (`transaction.rs:333-339`). In
  the gap they are inside the adjustment.
- **ATA rent.**
  - Paying rent to create an ATA: the owner's lamports go down. That is a
    native leg in observed blocks, and part of the adjustment in the gap.
  - Closing an ATA: the account drops out of `postTokenBalances`, giving a
    −amount leg. The rent refund is a native leg.
  - `getTokenAccountsByOwner` no longer lists a closed account.
  - Both sides agree.
- **Ethereum internal transfers.** Native ETH a contract moves internally is
  not seen (`sdk/chains/ethereum/src/chain.rs:8-10`). Reconcile sees it as a
  difference: a new benefit (§3.11).
- **Assets that do not count anywhere.** Token-2022 confidential balances and
  wrapped SOL count neither in our legs nor in the chain reads we use. Both
  sides agree.

### 3.4 Bitcoin: a balance cannot spend

- Spending reads coins (`InputStore::find_unspent`,
  `sdk/base/src/inout.rs:162-169`), built from `received` legs that carry a
  coin. A number is not enough.
- Reconciling Bitcoin means **coin-level** adjustments:
  - coins in the scan that we do not hold become opening coins;
  - our unspent coins missing from the scan are closed.
- This needs coin-carrying records, which the adjustment table (§5.1) does
  not have.
- **Holds:** the arithmetic, if Bitcoin ever needs it.
- **Breaks:** the cost. A one-day outage is 144 blocks (§1.3), and
  `scantxoutset` costs minutes and is often disabled.
- **Decision:** Bitcoin keeps `max_backfill = NULL`. Coin-level reconcile is
  open question Q6.

### 3.5 Pending and unconfirmed amounts; what the UI shows

**While an index is `reconciling`:**

- Its confirmed balance is unknown, because the gap is not accounted for.
- `balance_confirmed` and `balance_confirmed_spendable` are **null**, not a
  wrong number.
- `balance_pending` is null too.
- Legs from `R` on are shown as usual.

**A pending send of ours mined inside the gap:**

- Its mined legs are never observed. The pending legs stay until `expires_at`
  (`apps/server/init.sql:77-78`).
- The adjustment already contains the send.
- Until expiry, spendable is under-stated by that send. That is conservative:
  it never over-spends.
- The fix is open question Q3.

**After reconcile:** the movements list shows one `adjustment` row at `S`. It
carries the time of block `S`, the gap `[from, to]`, no counterparty, and
the amount.

### 3.6 Triggers during reconcile

- Triggers read only indexes "fully indexed through confirmed"
  (`sdk/trigger/src/store.rs:10-23`; SQL `wallet_index.rs:713`
  `wi.indexed_through >= ?`). They read balances only, never movements.
- The rule gains `AND wi.state = 0`. A reconciling index is invisible to
  triggers, as a backfilling one is today.
- A skip brings the network back to the tip at once, so `Idle` happens again
  and rounds resume (`index.rs:169-173`). Today, triggers on Solana never run.

### 3.7 Reorgs across R and S

- **R and S are both final** when chosen. A reorg can only archive blocks
  above them.
- **The adjustment sits at `S`.** It is final, so it is never archived. Its
  position is not tied to the archive, unlike a leg at the moving tip.
- **The block store refuses `delete(from)` at or below the resume point of a
  gap** (`block.rs:556-571`). The rewind would otherwise land on the pre-gap
  block `L` and backfill the gap. It fails loudly; a node reorging a final
  block is a node bug.
- **A reorg above `S`, before reconcile:** the index stays `reconciling`. Its
  legs in (S, tip] are archived as usual. The next attempt picks a new `S`.

### 3.8 Re-consolidating repeatedly

- **Each skip adds one gap and, once reconciled, one adjustment per affected
  index.** Adjustments carry a running `total_adjusted`, so a balance stays
  two seeks (§5.1).
- **With r < 2.5 for good:** every gap grows past `M` again, so the network
  skips again. The result is "poll the balance every M slots plus read what
  we can". Balances stay right; history gets holes. The network shows
  `skips` and gap rows, so this is visible, not silent.

### 3.9 What the user loses

For gap activity only:

- per-transaction legs;
- txids;
- times;
- counterparties;
- the split between fee, received and sent (it shows as one net amount).

Everything outside gaps is unchanged. The adjustment keeps:

- the gap range;
- the net change;
- the read (`S`, `balance(S)`, when it was read).

### 3.10 Database relief: consolidation is the wrong tool

- **The bulk is empty block rows** (§1.4), not wallet history. Relief means
  pruning block rows that nothing references and that are below
  `confirmed − margin` (§5.7). It needs no chain read and no adjustment.
- **Squashing history is just a delete.** Totals are absolute, so later legs
  are untouched (072, `init.sql:89-104`). To squash an index's legs below
  `H`, keep the last leg at or below `H` (Bitcoin: plus unspent coin legs)
  and delete the rest. That is opt-in and later (Q7).
- **The user's "initial balance + adjustment" is the right model for gaps,
  not for relief.**

### 3.11 Bonus: reconcile as a correctness probe

- **A non-zero adjustment on an index with no gap is drift.**
  - On Ethereum: internal transfers.
  - On Solana: a parser bug.
- **Running reconcile periodically on healthy indexes measures it.** Record
  it with `reason = 'drift'`.
- **This is "observable correctness"** (AGENTS.md).

### 3.12 Summary

| Claim | Verdict |
|---|---|
| Max backfill per network, then do something cheaper | **Holds**: bounds catch-up and stops one lagging index from blocking the network (§1.1) |
| "Quiet for N blocks ⇒ balance confirmed" | **Breaks**: quietness cannot be proven without reads, the two reads race, busy wallets never qualify |
| "We read blocks from restart, so we know who was quiet" | **Holds, and goes further**: no quiet needed, only S final and indexed (§3.1) |
| Initial balance at a known confirmed height | **Holds** as `balance(S) − Σ[R, S]`; stored as an adjustment, not a second balance |
| Birthday works the same way | **Holds**, and fixes the zero-start bug (§1.1). Every new watch-only index gets an opening reconcile. |
| Works for every chain | **Partly.** Solana and Ethereum yes. Bitcoin needs UTXOs: deferred (§3.4). |
| Append-only history, kept apart, plus an adjustment | **Holds.** `leg` already is that append-only per-index history (072). Adjustments get their own table. |
| Consolidation for database relief | **Wrong tool**: prune empty block rows instead (§3.10) |
| Needs live reading to keep up | **Yes, hard prerequisite**: r ≥ 2.5 `getBlock`/s sustained, otherwise skips repeat (§3.8, §4.3) |
| Solana mainnet | **Breaks**: reading every block exceeds the public data cap (§1.2). It needs an address-only mode or a paid provider (Q5). |

---

## 4. Alternatives

### 4.1 Comparison

| Option | Cost for Solana devnet, 1-day outage | Complexity | Data kept | Solana | Ethereum | Bitcoin |
|---|---|---|---|---|---|---|
| A. Read every block (today) | 216k `getBlock`; 8 h to never (§1.3) | built | everything | devnet only if r ≫ 2.5; mainnet no | fine | fine |
| B. Address-only: `getSignaturesForAddress` (≤ 1,000 per page) + `getTransaction` per new signature, for the owner **and every token account** | W × (1+T) calls per poll, plus 1 per transaction. Polling every 10 s at 3 rps fits about 30 addresses with no tokens | **L**: new indexer mode; reorgs by signature status; ATA discovery (`docs/issues.md` I4) | everything; public devnet serves deep history (`getFirstAvailableBlock` = 116,113,408 today) | good for small W, the **only mainnet-viable** option | no RPC address index | no |
| C. Routed and load-balanced nodes (`network_endpoint_route`, built) | raises r; a config change | none | everything | needed for any option that reads blocks | helps | n/a |
| D. Archival or paid provider (Helius, Triton, QuickNode) | money; high r, no data cap | low | everything | yes | yes | yes |
| E. Consolidation as first proposed (quiet window, balance now) | ~2 calls per index | M | net only for the gap | racy, starves busy wallets | no quiet signal for native | needs UTXOs |
| **F. Skip + reconcile (§2, recommended)** | ~2–3 calls per index per gap: balance plus `getBlock(none)` | **M** | everything outside gaps; net change for gaps | **yes (devnet)** | yes, plus drift fix | deferred |
| G. Hybrid F + B | F, then explain gaps through B in the background | L | gaps explained too | yes | — | — |

### 4.2 Recommendation

1. **C now, with no code.** Route `getBlock` over at least 2 endpoints so that
   r ≥ 5. Without r > 2.5, nothing that reads blocks works.
2. **F as the safety valve and as the opening for new wallets.** It is the
   user's idea in its exact form. It bounds outages and lagging indexes. It
   fixes zero-start watches and Ethereum drift. It reuses running totals with
   no rewrites.
3. **Block-row pruning** for database relief (§5.7), not consolidation.
4. **Solana mainnet** needs B or D. F does not change that (Q5). G later,
   only if users ask to see gap history (Q4).

### 4.3 Keeping up live is the prerequisite

- **The routes are in the current diff.**
  - Each method with rows in `network_endpoint_route` goes round robin over
    those endpoints (`apps/server/init.sql:396-403`,
    `sdk/payments/src/payments.rs:15-18`, `:440`).
  - The server reads routes in `apps/server/src/models/endpoint_route.rs:91-163`.
- **Solana: route `getBlock`** (and `getBlocks`) over k endpoints, so that
  `r = Σ rᵢ ≥ 2.5 × 1.5`, which leaves headroom.
- **A batch counts per call.** A batch of 10 `getBlock` is 10 requests
  against the 40/10 s per-method limit.
- **What happens when live reading falls behind again:**
  1. The lag grows.
  2. Past `M`, the network skips to a new `R`.
  3. The affected indexes reconcile.
  4. Balances stay correct; a gap row records the hole.
- **Persistent skipping is an alarm.** It is shown as `network_index.skips`
  and the network's gaps. It means: add endpoints.

---

## 5. Design (option F)

### 5.1 Data model

No migrations: init.sql says a schema change means a fresh file
(`apps/server/init.sql:1-3`).

```sql
-- network: how far behind an index may fall before it is skipped (NULL: never).
ALTER TABLE network ADD COLUMN max_backfill INTEGER;   -- in init.sql: a column
-- network_index: skips since the row was made.
--   skips INTEGER NOT NULL DEFAULT 0

-- A hole in a network's kept chain: blocks after `after` and below `resume`
-- were never read. `resume` was final when the gap was opened. The store
-- lets the block at `resume` stand on `after` without linking, and refuses
-- to archive at or below `resume`.
CREATE TABLE network_gap (
    chain     TEXT    NOT NULL,
    network   TEXT    NOT NULL,
    after     INTEGER,            -- last kept position before; NULL: none
    resume    INTEGER NOT NULL,   -- R
    opened_at INTEGER NOT NULL,   -- Unix ms
    PRIMARY KEY (chain, network, resume),
    FOREIGN KEY (chain, network) REFERENCES network (chain, network)
);

-- wallet_index.state gains 3 = reconciling:
--   CHECK (state IN (0, 1, 2, 3))
-- plus the gap it waits on:
--   gap_from INTEGER,   -- first unobserved position; NULL: from genesis (opening)
--   gap_to   INTEGER,   -- R - 1
--   CHECK ((state = 3) = (gap_to IS NOT NULL))

-- One correction per reconcile: the chain's balance at a final block S
-- minus our total there. Append-only. A balance at H is the last live leg's
-- total at or below H plus the last adjustment's total_adjusted at or below H.
-- id: "{wallet_index_id}.{seq}".
CREATE TABLE wallet_index_adjustment (
    id              TEXT    NOT NULL PRIMARY KEY,
    wallet_index_id TEXT    NOT NULL REFERENCES wallet_index (id) ON DELETE CASCADE,
    seq             INTEGER NOT NULL,     -- 1, 2, … per index
    block           TEXT    NOT NULL REFERENCES block (id),  -- our live block at S
    position        INTEGER NOT NULL,     -- S
    amount          TEXT    NOT NULL,     -- signed exact decimal: chain − ours
    total_adjusted  TEXT    NOT NULL,     -- running sum of amount
    chain_balance   TEXT    NOT NULL,     -- balance(S) as read
    gap_from        INTEGER,              -- NULL: opening (from genesis)
    gap_to          INTEGER,              -- NULL: drift check, no gap
    reason          INTEGER NOT NULL,     -- 0 opening, 1 gap, 2 drift
    read_at         INTEGER NOT NULL,     -- Unix ms
    CHECK (id = wallet_index_id || '.' || seq),
    CHECK (reason IN (0, 1, 2)),
    UNIQUE (wallet_index_id, seq)
);
CREATE INDEX wallet_index_adjustment_at
    ON wallet_index_adjustment (wallet_index_id, position, seq);
```

**How it relates to the existing tables:**

- **`block`:** the adjustment references our live block at `S`. Pruning
  (§5.7) keeps it. A gap is a row of its own, not missing link checks
  everywhere.
- **`leg`:** unchanged. It stays the observed, append-only history.
  `total`, `total_received` and `total_sent` mean "observed" (072).
- **`wallet_index`:** `birthday` becomes `R` on skip. `requested_from` stays,
  so the shortening is visible (070 §2). It carries `state = 3` and
  `gap_from` / `gap_to` while reconciling.
- **`wallet_index_with_balance`** (`init.sql:239-310`):
  - add one seek, `a` = the last adjustment at or below `i.confirmed`, and
    `a_now` = the last one of all;
  - `balance_confirmed = SUM_DECIMAL(c.total, a.total_adjusted)`, and the
    same for `n` with `a_now`;
  - all balances are NULL while `state = 3`;
  - `balance_confirmed_received` and `balance_confirmed_sent` stay observed;
  - add `balance_confirmed_adjusted = a.total_adjusted`.

### 5.2 Per-network settings

| Network | `max_backfill` | Why |
|---|---|---|
| solana/devnet, solana/mainnet | 9,000 (1 h) | At r = 5 it catches up in 1 h; past that, history loses to freshness |
| ethereum/* | NULL | A one-day backlog is 7,200 blocks, minutes to an hour |
| bitcoin/* | NULL | §3.4 |

- **`max_history_days` stays** for explicit starts (070 §1).
- **An explicit start further back than `max_backfill`** is skipped like any
  other lagging index. The API shows `requested_from` against `birthday`.
  See Q2.

### 5.3 State machine per wallet index

```
             create (keyed, generated)        create (watch-only / imported, adapter exists)
                    │                                  │
                    ▼                                  ▼
               ┌──────────┐  next() < confirmed − M ┌──────────────┐
   resume ───▶ │ indexing │ ──── skip(R) ─────────▶ │ reconciling  │
               │   (0)    │ ◀──── reconcile ok ──── │     (3)      │
               └──────────┘   (adjustment at S)     └──────────────┘
                 │   ▲  │                              │  │ ▲
         pause   │   │  │ NotServed, adapter exists    │  │ └─ reorg above S / S not yet
                 ▼   │  └──────────────────────────────┘  │    final or indexed: stay
              ┌────────┐   NotServed, no adapter          │ pause
              │ paused │ ◀── (070) ──▶ no_data (2)        ▼
              │  (1)   │                               paused (keeps gap_*)
              └────────┘
```

- **`reconciling` is indexed like `indexing`.** `advance` and `find_next`
  read `state IN (0, 3)` (`wallet_index.rs:468-550`). Legs from `R` on are
  kept, so `Σ[R, S]` is known.
- **Reconcile succeeds** when the chain answers at an `S` with
  `R ≤ S ≤ min(indexed_through, confirmed)` and the hash at `S` matches. In
  one write it inserts the adjustment and sets `state = 0`, clearing
  `gap_*`.
- **A started index the node no longer serves** (070 §3) becomes
  `reconciling` instead of `no_data` when the chain has an adapter. `no_data`
  remains for chains without one.

### 5.4 sdk/base traits

Bitcoin, Ethereum and Solana are core crates: ask before changing
`sdk/base`. Sketch:

```rust
// sdk/base/src/balance.rs
/// One asset of one address as the chain holds it at a final block.
pub struct Holding {
    /// S: the block the answer is at.
    pub position: u64,
    /// The chain's hash at S, compared with our live block there.
    pub hash: BlockHash,
    /// Atomic amount.
    pub amount: Decimal,
}

/// Reads balances at a final block: optional per chain, like `Dated`.
pub trait Balances: Networked + Send + Sync {
    /// The balance of `asset` of `address` at a final block in
    /// `from..=through.position`. `through` is our last live block at or below
    /// confirmed; a chain that can pin reads exactly there (Ethereum: EIP-1898
    /// `blockHash`). `None` when the node cannot answer inside that range yet
    /// (Solana root above `through`, Bitcoin tip not ours): try again later.
    fn balance<'a>(
        &'a self,
        address: &'a Address,
        asset: &'a Asset,
        from: u64,
        through: &'a BlockHeader,
    ) -> Future<'a, anyhow::Result<Option<Holding>>>;
}
```

- **`Service` gains** `balances: Option<Arc<dyn Balances>>`, next to `dated`
  (`sdk/base/src/config.rs:106-107`).

**Store changes:**

```rust
// BlockStore (sdk/base/src/block.rs:519)
/// Opens a gap at `resume` (final): in one write, records it, and moves
/// every indexing index whose next() is below `resume` to `reconciling`
/// with birthday `resume`, `indexed_at` unset and gap_from = its old
/// next(). The next insert may start at `resume` without linking to the
/// last live block. A no-op when nothing is below `resume`.
fn skip<'a>(&'a self, network: &'a Network, resume: u64) -> Future<'a, anyhow::Result<()>>;
// delete(): refuses `from <= resume` of any gap of the network.

// WalletIndexStore (sdk/base/src/wallet.rs:352)
/// In one write: if `index` is still reconciling and our live block at
/// `held.position` has `held.hash`, appends the adjustment
/// held.amount − total(held.position) and sets it indexing. Whether it did.
fn reconcile<'a>(&'a self, index: &'a WalletIndex, held: &'a Holding)
    -> Future<'a, anyhow::Result<bool>>;
```

- **`WalletIndexState`** gains `Reconciling`.
- **`WalletIndex`** gains `gap: Option<(Option<u64>, u64)>`.

### 5.5 Chain adapters

| Chain | `Balances::balance` |
|---|---|
| Solana | `getBalance` or `getTokenAccountsByOwner{mint}` (finalized, `minContextSlot = from`). If `context.slot > through.position`, return `None`. Then `getBlock(S, none, finalized)` for the hash. Two calls. |
| Ethereum | `eth_getBalance` or `eth_call balanceOf` at `{blockHash: through.hash, requireCanonical: true}`. One call. `S = through`. |
| Bitcoin | none at first (§3.4) |

### 5.6 What the indexer does differently

`Indexer::fetch` (`sdk/base/src/indexer.rs:241-289`):

```rust
let tips = self.chain.tips().await?;
let start = lagging.or(last.map(|l| l.position));
if let (Some(m), Some(start)) = (self.max_backfill, start)
    && tips.confirmed.saturating_sub(start) > m
{
    self.blocks.skip(network, tips.confirmed).await?;   // R = confirmed
    self.stats.get().skips += 1;
    return Ok(Next::Skipped(tips.confirmed));
}
// … unchanged forward fetch …
// After a kept run, or on Idle: reconcile up to K reconciling indexes.
```

- **Reconcile in the step.** It runs for each `Reconciling` index with
  `indexed_through ≥ R`, about K = 20 per step, so a step stays bounded:
  - `through` = our last live block at or below `min(indexed_through,
    confirmed)`;
  - `balances.balance(...)`;
  - then `indexes.reconcile(...)`.
- **A failed read** leaves the index reconciling and counts in `Stats`. It
  never fails the step.
- **Opening reconcile on watch.**
  - `watch` with a watch-only or imported key creates the index as
    `reconciling` with `gap_from = NULL` and `gap_to = birthday − 1`, so the
    zero-start bug goes away.
  - A generated key, which has no past, starts as `indexing`.
- **The 070 `NotServed` path** (`indexer.rs:204-239`): a started index goes
  to `reconciling` when `balances` exists.

### 5.7 Database relief: pruning block rows

A `prune` step of the existing prune job (`apps/server/src/usecase/workers/prune.rs`).
Per network, delete the live block rows that meet every condition:

- position < `confirmed − margin` (margin = `max(confirmations, 1000)`);
- position < the network's lowest `next()`, because pruned blocks must not
  look like "no live block since birthday" to `advance`
  (`wallet_index.rs:526-540`);
- not referenced by a `leg`, a `transaction`, a `wallet_index.indexed_at`,
  a `wallet_index_adjustment` or a `network_gap`;
- not the last live block.

Before deleting, set `parent = NULL` on any kept child; the link is only
checked at write time, near the tip.

- **Saves about 93 MB/day on Solana** (§1.4).
- **Archived blocks** (`archived_at` set) older than the margin go too.
- **The blocks list API shows fewer blocks.** That is accepted.

### 5.8 API and UI

| Where | Change |
|---|---|
| `models::WalletIndex` | `state: "reconciling"`; `gap_from` and `gap_to`; `balance_*` null while reconciling; `balance_adjusted`; `adjustments: [{position, amount, chain_balance, gap_from, gap_to, reason, read_at}]` |
| Movements (`wallet_index_legs`) | adjustment rows mixed in by `(position, seq)`, kind `adjustment`, block time of `S` |
| `GET /v1/networks` | `max_backfill`, `skips`, `gaps: [{after, resume, opened_at}]` |
| UI wallet page | a "Reconciling: gap X–Y" badge; balance shows "—" until reconciled; the adjustment row in history with an explorer link to the address |
| UI network page | gaps and skips counter; a hint: "add a `getBlock` route" |
| `docs/openapi.json`, `ui/src/api/schema.d.ts` | regenerated |

### 5.9 Migration

- **Fresh file** (init.sql rule). The current `.data/payment.db` is not
  carried over.
- **After upgrade:**
  - Sepolia and Solana wallets are re-added.
  - Watch-only ones get an opening reconcile, so they show the true balance
    on day one instead of 0.
  - The 344k-slot devnet backlog disappears: the first step starts at the
    confirmed tip.

---

## 6. Phased rollout

| Phase | What | Unblocks |
|---|---|---|
| **0 (config only)** | Route `getBlock` and `getBlocks` for solana/devnet over ≥ 2 endpoints (public + Tatum, or keys) so r ≥ 5. Confirm `behind_tip` falls. | A Solana node that keeps up, the prerequisite for everything else |
| **1 (smallest code step for Solana)** | `max_backfill`, `network_gap`, `BlockStore::skip`, `Next::Skipped`, state `reconciling`, `wallet_index_adjustment`, the view change, `state = 0` in the trigger selection, the Solana `Balances` adapter (native + SPL), opening reconcile on watch | A one-day outage costs ≤ M slots of reading; triggers run again; balances are right after any gap |
| 2 | Ethereum adapter; periodic drift reconcile (`reason = 2`); NotServed → reconciling | Ethereum internal-transfer drift fixed and measured |
| 3 | Block-row pruning (§5.7) | ~93 MB/day of Solana growth gone |
| 4 (optional) | Q3 pending check, Q4 gap explanation, Q6 Bitcoin coins, Q7 leg squash | — |

---

## 7. Open questions, each with a recommendation

| # | Question | Recommendation |
|---|---|---|
| Q1 | `max_backfill` in positions or time? | Positions, which is what the indexer compares; the UI shows it as time (× `block_time_ms`). Solana 9,000; others NULL. |
| Q2 | Is an explicit start further back than `max_backfill` refused, or skipped? | Skipped, with one rule for every index. The API shows `requested_from` against `birthday` (070 §2). It must not hold the network's forward path (§1.1). |
| Q3 | Our pending sends mined inside a gap stay pending until expiry. | Phase 4: at reconcile, `getSignatureStatuses` / `eth_getTransactionReceipt` for that index's pending txids; mined ones are dropped (the adjustment already holds them). Until then: conservative. |
| Q4 | Explain gap history (txids, counterparties) later? | Not now. If asked: phase-4 job using `getSignaturesForAddress` (owner + token accounts), written as a side table of explanations; it never moves totals. |
| Q5 | Solana mainnet: ~2 MB per block exceeds the public 3.3 MB/s cap. | Do not enable mainnet on public nodes. Either a paid provider (D) or address-only mode (B, `docs/issues.md` I4). Decide before mainnet. F still applies on top of either. |
| Q6 | Bitcoin coin-level reconcile? | Defer; `max_backfill = NULL`. If needed: `scantxoutset` gives opening coin legs and closing legs, written as `received` / `spent` legs with coins under a synthetic adjustment transaction. |
| Q7 | Squash old legs for relief? | Later and opt-in (`keep_history_days`): keep each index's last leg ≤ H plus unspent coin legs; totals stay valid (§3.10). |
| Q8 | Do drift adjustments on Solana raise an alert? | Yes: a non-zero Solana drift is a parser bug. Log it and show a red badge on the index. On Ethereum it is expected (internal transfers). |
| Q9 | Keep the Solana same-fork `getBlock(none)` check (+1 call)? | Yes. 232 B. It closes the confirmed-but-not-finalized edge for good. |
| Q10 | `sdk/base` changes (`Balances`, `skip`, `reconcile`, `Reconciling`) | Needs the owner's approval (core crate rule) before phase 1. |

Sources:

- [Solana clusters and public RPC limits](https://solana.com/docs/references/clusters)
- [getSignaturesForAddress](https://solana.com/docs/rpc/http/getsignaturesforaddress)
- [getBalance](https://solana.com/docs/rpc/http/getbalance) / [getTokenAccountsByOwner](https://solana.com/docs/rpc/http/gettokenaccountsbyowner) (`minContextSlot`, `context.slot`)
- [EIP-1898](https://eips.ethereum.org/EIPS/eip-1898) (block hash in state reads)
- [Bitcoin Core scantxoutset](https://developer.bitcoin.org/reference/rpc/scantxoutset.html)
- The 2026-11-12 measurements in §1.2 were taken with curl against `api.devnet.solana.com` and `api.mainnet-beta.solana.com`.
