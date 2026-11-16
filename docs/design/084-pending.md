# 084: Pending transactions

Status: proposed. Replaces the expiry of 066.

066 holds a sent transaction's funds until it is mined **or its time is
up**. A clock cannot tell that a transaction is dead: one past its expiry
may still be in a mempool and mine later, after its funds and its nonce
were given to another send. This design keeps a pending transaction until
the chain proves it can never mine, sends it again while it waits, and
raises its fee when it is stuck.

## Use cases

1. **No silent loss.** X pays Bob, waits through a gas spike, and is still
   unmined after an hour. The next send does not take X's nonce and
   replace it: X holds its nonce until it is mined or dead.
2. **No double pay.** A send that looks gone is not gone. Nothing frees its
   funds while it can still mine, so a retry by the user is refused as
   short instead of paying twice.
3. **Stuck sends get mined.** A send whose fee fell behind is sent again in
   its own slot at today's fee, up to a cap.
4. **Lost sends come back.** A send a node accepted and later dropped
   (full pool, restart) is sent again with the same bytes.
5. **Dead sends free their funds.** A send another transaction has
   displaced stops holding its funds as soon as the chain shows it.
6. **Back-to-back Ethereum sends do not clash.** Behind a load-balanced
   node, a second send no longer takes the nonce of the first.

## What stays from 066

- **Recorded is spent.** A transaction is recorded as pending before it is
  broadcast, and its outgoing legs count as spent from then on. Its funds
  are held before its bytes leave, and a crash mid-broadcast loses
  nothing.
- **Derived, never marked.** Spent coins and balances are read from legs.
  Deleting a pending row frees everything it held at once.
- **Built once.** A rerun sends the recorded bytes again and never signs
  again: a second signing can be a second payment.
- **One sender at a time.** A broadcast job locks `chain:network.sender`;
  a triggers job locks its whole network (067). Coin selection and nonces
  see only what is recorded.
- **Refused means every node refused.** One node accepting is success; a
  node that timed out may have accepted, so that is retried with the same
  bytes (`fan_out`, `apps/server/src/usecase/transport.rs`).

## What changes

- **A pending transaction lives until it is mined or proven dead.** No
  clock frees it. `Transaction::expires`, `Transactions::expiring`,
  `pending_ttl_ms` and `transaction.expires_at` go, and so does the
  `expire` job.
- **Dead is proven by the chain** (below), never guessed from age.
- **While pending, it is sent again** with the same bytes, every walk.
- **When it is stuck, its fee is raised** in its own slot (the same nonce,
  the same inputs), automatically, up to a cap. Chains that cannot replace
  only send again.
- **One row per slot.** A raise swaps the row; `origin` keeps the first
  version's id, so funds stay held once and a payment stays traceable.
- **An Ethereum nonce counts our own pending sends**, not only the node's
  mempool.

## Why a clock is wrong

10:00, X pays Bob 1 ETH at nonce 15; a node accepts it; gas spikes and X
waits. 11:00, the expire job deletes X: the books show 1 ETH free and
nothing pending. Then, each wrong:

1. Y pays Carol at nonce 15 with today's higher fee and replaces X. Bob is
   never paid; X's job says done.
2. Y's fee is not 10% higher: the node refuses it ("replacement
   underpriced"). Carol is not paid either.
3. The user sees X gone and pays Bob again. Gas drops, X mines first, the
   retry takes nonce 16: Bob gets 2 ETH.

Bitcoin makes it worse: a transaction stays valid for as long as its
inputs are unspent. A node drops it after 14 days (`mempoolexpiry`) or
sooner when its pool is full, but anyone holding its bytes can send it
again, and it mines.

## Dead, per chain

A pending transaction is **dead** when the chain proves it can never mine.
Only then is it forgotten.

| Chain | Dead when | Read |
| --- | --- | --- |
| Ethereum | its sender's nonce count at block `at` is above its nonce | `eth_getTransactionCount(from, at)` |
| Bitcoin | a coin it spends is spent by another transaction in a live block | the server's legs, no node |
| Solana | its signature has no status and its blockhash is no longer valid at `finalized` | `getSignatureStatuses` (with history), `isBlockhashValid` |

- **`at` is the indexed tip of the index it spends from** (its spent leg's
  wallet index). A transaction of ours mined at or below `at` was read
  there, so its row is mined, not pending; a row still pending whose nonce
  is used at `at` was displaced, or mined inside a gap (083), and in both
  cases is no longer pending. Above `at` nothing is judged, so our own
  transaction mined in a block not yet read is never taken for dead.
- **An index that is not `indexing`** (paused, no data) has no
  `at`: its pending sends are not judged and keep holding.
- **Bitcoin needs no node read.** A wallet spends only coins of live mined
  blocks (`find_unspent`), never pending change, so a pending transaction
  dies only when one of its own coins is spent elsewhere, and the legs
  show that.
- **Solana:** once the blockhash is invalid at the finalized bank, no later
  block can take the transaction. Any block that has it must lie at or
  below the finalized one, so a signature with no status is final.
- A failed send again never decides anything: only the dead check forgets.

## Replace, per chain

A **replacement** is the same payment, signed again in the same **slot**
with a higher fee. Exactly one version of a slot can mine; the rest are
then dead.

| Chain | Slot | Rule for a replacement | Cap |
| --- | --- | --- | --- |
| Ethereum | sender's nonce | `maxFeePerGas` and `maxPriorityFeePerGas` each at least 10% higher | `FeeLimits` (existing) |
| Bitcoin | its inputs | a higher fee rate and a higher total fee (by 1 sat/vB × size), taken from the change | `max_fee_rate` (new) |
| Solana | none | none: a transaction dies with its blockhash in about 1–2 minutes | — |

- **Ethereum:** the gas limit stays; each fee is the higher of today's
  estimate and the old one plus 10%. No gain when today's estimate is not
  above the old fees. Some L2s have a private sequencer and no public
  mempool: a send is taken quickly or refused, and there is nothing to
  replace.
- **Bitcoin:** the inputs and payment outputs stay; the change shrinks by
  the extra fee. Our sends already signal RBF
  (`Sequence::ENABLE_RBF_NO_LOCKTIME`). No change output, or a change that
  would fall below dust: no replacement.
- **Solana** has no replacement. A stuck send dies on its own; the next
  triggers round decides again from the balance and sends anew.

## Design

### `base` (needs approval)

**`Broadcaster::alive`**, required on every chain:

```rust
pub trait Broadcaster: Send + Sync {
    fn broadcast<'a>(&'a self, transaction: &'a Transaction) -> Future<'a, Result<(), Error>>;

    /// `false` only when the chain proves pending `transaction` never
    /// mines, judged at block `at`, the indexed tip of the index it spends
    /// from. `true` when it cannot tell.
    fn alive<'a>(&'a self, transaction: &'a Transaction, at: u64) -> Future<'a, anyhow::Result<bool>>;
}
```

Bitcoin answers `true`: its rule is the server's (legs).

**`Replace`**, an optional capability, as `Balances` is (083):

```rust
pub trait Replace: Networked + Send + Sync {
    /// `transaction` signed again in its slot by `wallet` at today's fee;
    /// `None` when nothing is gained or the fee is above the cap.
    fn replace<'a>(
        &'a self,
        wallet: &'a dyn Wallet,
        transaction: &'a Transaction,
    ) -> Future<'a, Result<Option<Transaction>, Error>>;
}

pub struct Service {
    // ...
    /// Raises a stuck transaction's fee in its slot; `None` where the chain
    /// cannot.
    pub replace: Option<Arc<dyn Replace>>,
}
```

It reads what it needs from the transaction's bytes (the Ethereum fields;
the Bitcoin inputs and outputs) and legs (the spent coins' amounts).

**`Storage::pending`**, for the Ethereum nonce:

```rust
pub trait PendingStore: Send + Sync {
    /// `sender`'s pending transactions, as recorded.
    fn sent<'a>(&'a self, sender: &'a Address) -> Future<'a, anyhow::Result<Vec<Transaction>>>;
}

pub struct Storage {
    // ...
    pub pending: Arc<dyn PendingStore>,
}
```

The server builds every chain's service through `Storage`, so an
Ethereum-only port could not reach the builder; this one is generic, and
only Ethereum reads it.

**`Transaction::expires` and `Transactions::expiring` go.** The caller's
duties on `Broadcaster` become:

1. record a transaction (its legs) before broadcasting it, and count its
   outgoing legs as spent from then on;
2. build, record and broadcast a sender's transactions one at a time;
3. on `Rejected` or `Expired` from a broadcast, forget it; on a transient
   error, keep it and send the same bytes again, never building anew;
4. forget a pending transaction only when `alive` says `false`.

### Chains

- **Ethereum:**
  - The builder's first nonce for a sender is the higher of
    `eth_getTransactionCount(pending)` and one above the highest nonce of
    `Storage::pending.sent(sender)` (decoded from their bytes).
  - `alive` per the table.
  - `Replace` per the table.
  - Settings: `bump_after_ms` (600,000) replaces `pending_ttl_ms`.
- **Bitcoin:**
  - `alive` is `true`.
  - `Replace` per the table.
  - Settings: `bump_after_ms` (3,600,000) and `max_fee_rate` replace
    `pending_ttl_ms`.
- **Solana:**
  - `alive` per the table.
  - No `Replace`.
  - `pending_ttl_ms` goes; a send past its blockhash still fails to
    broadcast as `Expired` and is forgotten then.

No stored network settings set `pending_ttl_ms` today, so removing it
breaks no row (`deny_unknown_fields`).

### Server

#### Schema

- `"transaction"`:
  - gains `origin TEXT`, the id of the first version of its slot: its own
    id when recorded, kept by a replacement, and kept when it is mined;
  - drops `expires_at`;
  - its CHECK becomes `block IS NULL OR sent_at IS NULL`;
  - `CREATE INDEX transaction_origin ON "transaction" (origin)`.
- `sent_at` is when the current version was sent: the bump clock.
- Migration `084-migrate.sql`: rebuild `"transaction"` (SQLite cannot drop
  a column under a CHECK), with `origin = id` for pending rows.

#### Models (`transaction::db`)

- `find_pending(network)`: every pending row with the indexed tip of the
  index it spends from.
- `find_dead_by_coin(network)`: pending rows with a spent coin that a
  `Spent` leg of another transaction consumes in a live block.
- `get_by_origin(origin)`: the current version of a slot.
- `swap(old, new)`: in one write, if `old` is still pending, deletes it
  and its legs and keeps `new` with `old`'s origin; else nothing.
- `upsert` (mined) keeps a stored `origin`.
- `expire` goes. `delete_pending` (refused, forgotten) stays.

#### The broadcast job

- It records, then broadcasts, as now.
- It looks up each recorded transaction by `origin`, not `id`, so a
  version swapped in by a raise is still found.
- "Forgotten meanwhile" now means dead: it fails `dead`, not
  `outcome-unknown`.

#### The `pending` job (replaces `expire`)

One per indexed network, every 60 s; topic `pending:chain:network`, which
conflicts with nothing. For each pending row whose index is `indexing`:

1. **Dead?** By `find_dead_by_coin`, or `alive(tx, at) == false`: forget it
   (`delete_pending`).
2. **Stuck?** Sent at least `bump_after_ms` ago, and the network has
   `Replace`: enqueue a `bump` job for its origin (unique per origin).
3. **Otherwise:** send the same bytes again; the answer is ignored.

It needs no sender lock: forgetting is one DB write, and the same bytes
again conflict with nothing. A send that builds meanwhile only misses
funds freed a moment later. An Ethereum nonce stays right: a dead
transaction's nonce is already used on the chain, so the node's count is
above it anyway.

#### The `bump` job

Input: the slot's origin. Topic: `chain:network.sender`, as broadcast, so
it never runs beside a send of its sender or a triggers round. State: the
replacement, once signed.

1. **Sign** (first run only): load the current version by origin; mined or
   gone: done. `Replace::replace` gives `None`: done. Check that its
   extra fee fits what its sender holds beyond what is in flight; short:
   done. Save the replacement as the job's state.
2. **Broadcast** it.
   - Refused by every node: nothing changes; the old version stays.
   - Transient: retry with the same bytes.
3. **Accepted:** `swap(old, new)`. If the old version was mined
   meanwhile, nothing is swapped and the new one dies with its slot.

The replacement is recorded only after a node accepted it. Until then the
old row holds the slot's funds, and the job's lock keeps every other send
of its sender out, so the higher fee cannot be spent twice. A crash after
the broadcast reruns from the saved state: the same bytes again, then the
swap.

#### Topics

| Job | Topic | Does |
| --- | --- | --- |
| `triggers` | `chain:network` | decide; start broadcast jobs (068) |
| `broadcast` | `chain:network.sender` | sign, check and record, broadcast |
| `bump` | `chain:network.sender` | replace a stuck send in its slot |
| `pending` | `pending:chain:network` | forget the dead; enqueue bumps; send again |

## Open

1. **Approval for `base`:** `Broadcaster::alive`, `Replace` with
   `Service::replace`, `Storage::pending` with `PendingStore`, and
   `Transaction::expires` removed.
2. **`bump_after_ms`:** 10 min for Ethereum, 60 min for Bitcoin?
3. **Bitcoin `max_fee_rate`:** what default?
4. **The 083 jump forgets an index's pending sends at once**
   (`forget_sent`), which is the blind forgetting this design removes.
   - Proposal: Ethereum and Solana stop forgetting and leave it to the
     dead check, since a send mined inside the gap is proven dead and its
     effect is already in the settle's balance read.
   - Bitcoin keeps forgetting: its coins are archived and written off, so
     the legs can no longer prove anything.
5. **A send at its cap** stays pending and is sent again for ever. How is
   it shown? A stats count of stuck sends, a job reason, or nothing until
   a cancel endpoint exists?
6. **Cancel**, a replacement that pays the sender itself, is a manual
   endpoint, later.
7. **What a dead send leaves.** Today a forgotten row is deleted, and only
   its job's reason remains. Should the row stay, with a `dropped` state
   and no legs, so the API can show that a payment never happened?

## Tests

- A pending send past an hour still holds its funds and its nonce; a
  second send takes the next nonce.
- Ethereum: a node whose pending count misses our first send still gets
  the second the next nonce.
- Ethereum dead: the nonce is used at `at` by another transaction → it is
  forgotten. Our own transaction mined above `at` → it is not.
- Bitcoin dead: a coin it spends is spent in a live block by another
  transaction → it is forgotten. Spent by its own replacement, mined →
  the replacement is the mined row.
- Solana dead: no status and an invalid blockhash → forgotten. A status →
  kept.
- An index not `indexing` → its sends are not judged.
- The walker sends again every pending row and never forgets on a refusal.
- Bump:
  - stuck past `bump_after_ms` → replaced in its slot, the row swapped,
    `origin` kept, the funds held once;
  - refused by every node → the old row stays;
  - a crash after the broadcast → the rerun swaps;
  - the old version mined meanwhile → no swap;
  - at the cap or no gain → no replacement;
  - short of the extra fee → no replacement.
- A broadcast job whose transaction was swapped finds it by origin; one
  forgotten as dead fails `dead`.

## Iterations

| # | What | Where | Done when |
| --- | --- | --- | --- |
| 1 | No clock: `alive`, the `pending` job (forget the dead, send again), `origin`, `expires_at` and `pending_ttl_ms` removed, the `Broadcaster` duties | `sdk/base`, chains, server, migration | a send unmined for an hour still holds its funds; a displaced one is forgotten on each chain |
| 2 | Ethereum nonce counts our pending sends (`Storage::pending`) | `sdk/base`, Ethereum, server | two back-to-back sends behind a node that misses the first both land |
| 3 | `Replace` and the `bump` job, Ethereum | `sdk/base`, Ethereum, server | a send stuck under a low fee is replaced and mined |
| 4 | Bitcoin `Replace` | Bitcoin | the same on Bitcoin |
| 5 | The 083 jump (open 4) | server | as decided |
