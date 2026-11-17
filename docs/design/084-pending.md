# 084: Pending transactions

Status: built: no clock, the Ethereum nonce, one payment per job, the
broadcast job owning its payment. Next: the job watches its payment's
transaction index (085), and stuck payments are bumped. Replaces the
expiry of 066.

066 held a sent transaction's funds until it was mined **or its time was
up**. A clock cannot tell that a transaction is dead: one past its expiry
may still sit in a mempool and mine later, after its funds and its nonce
went to another send. Here a payment is held until it is in a final block,
or provably can never land. Before it is broadcast it is watched like a
wallet (a transaction index, 085); its broadcast job only reads that
index, and when the payment is still not in any of the blocks read since
it was sent, the job raises its fee, within limits.

## Decided

| Question | Decision | Why |
| --- | --- | --- |
| When does a pending transaction stop holding its funds? | When it is in a final block, or provably can never land. Never by age. | A clock frees funds and nonces of transactions that can still mine: silent loss, refused sends, double pay. A Bitcoin transaction stays valid for as long as its coins are unspent. |
| How many payments per broadcast job? | One job = one payment = one transaction, on every chain. Several payments are several jobs; the sender's topic runs them in order. | A job of several transactions failed as a whole while some were already sent; a retry paid those twice. Bitcoin multi-recipient batching is dropped for now and may come back as its own feature. |
| Who knows whether a payment is mined? | Its transaction index (085): made before the broadcast, read forward by the indexer with every block, `found` set on the block that holds it, its gaps closed by the chain (`Confirmer`) or given up (Bitcoin). | The same rules as a wallet. The index says exactly how far we have looked, so nothing is judged on blocks not read yet. |
| Who watches a sent payment? | Its own broadcast job, which only reads the transaction index. No separate walker. | One job is one payment's whole story; nothing races it; it never gives up on timeouts. |
| How is "dead" known? | Only by the transaction's own deadline: Solana's blockhash expiry, read against the index's indexed height. Ethereum and Bitcoin transactions never die on their own; they are bumped instead. | With keys used only by this server, nothing else can spend a payment's coins or nonce; our own earlier versions are watched by their own indexes. |
| When is a payment bumped? | When its index has read `bump_after` blocks past `broadcasted_at` and it is in none of them; at most `max_bumps` times. | Counted in blocks the index actually read, a stuck or lagging indexer never triggers a bump: it has not read those blocks. |
| How is a bump signed? | The same stateful builder as a first send: `from_existing(current)`, `set_fee(level)`, `bump_fee()`, `sign()`. Its saved state is the signed transaction the server keeps. | One fluent API for sending and bumping; each chain keeps its replacement rule and fee cap. |
| Keys used outside this server | Not supported (like history gaps, 083). Such a payment may end stuck at its bump limit, its funds held. | Detecting it needs per-chain conflict bookkeeping for a case we do not support. |
| The Ethereum nonce | The higher of the node's pending count and one above our highest pending nonce. | A load-balanced node's mempool may miss our last send; our own records do not. |
| What a forgotten send leaves | A row in `transaction_forgotten` (reason, bytes, legs); the `transaction` row and its legs go. | A soft delete would need a filter in every read; one missed filter brings a dead send back. |

## The broadcast job

```
record + watch ─► send ─► check ─┬─ found, final        → done
                           ▲     ├─ Solana, expired     → forgotten (dead), failed
                           │     ├─ given up / gone     → failed (forgotten)
                           │     ├─ bump due            → bump, watch the new version
                           └─────┴─ otherwise           → send the same bytes again
```

Input: `{chain, network, address, asset, to, amount, fee}` (`fee` a
`FeeLevel`, `Normal` when absent). State: the txids of every version it
sent (the last is current), and how many bumps.

1. **Sign** the payment, once:

   ```rust
   let mut builder = wallet.transaction(&asset)?;
   builder.from_transfer(transfer)?;
   builder.set_fee(intent.fee);
   let signed = builder.sign().await?;
   let index = TransactionIndexTarget::new(chain, signed.id.clone()) // + with_expires_height on Solana
       .index()
       .await?;                                                       // broadcasted_at: the tip now
   ```

2. **Record and watch**, one write: the funds check (what the sender holds
   beyond what is in flight), the pending row and its legs (its coins and
   funds are held from now on), its `transaction_index` row, the job's
   state. Short: rolled back, the job fails `insufficient-funds`.
3. **Send** the bytes to every node, then one write:
   - accepted, or a transient failure (a node may hold it): `sent_at` set;
     the job checks from now on;
   - refused by every node: forgotten (`refused`), its index deleted, the
     job fails.
4. **Check**, every `Tracking::check`, reading only the job's transaction
   indexes (one per version):
   - **a version `found`** at or below the final block: done. If it is not
     the current one, the current pending row is forgotten (`replaced`).
     Found above the final block: check again;
   - **Solana**, the current index read past its `expires_height`, not
     found: forgotten (`dead`), failed;
   - **the current index given up** (`no_data`, a Bitcoin jump) or gone:
     failed `forgotten`;
   - **a bump due** (below): bump;
   - **otherwise**: send the same bytes again; check again.

   When the job ends, its transaction indexes are deleted.

The send is between two writes, not inside one: a rollback cannot take
bytes back from a node, and a crash with the write still open would lose
the record of a payment that may already be out. Recording first keeps
it; a rerun sends the same bytes again.

Between checks the job blocks nobody (`blocks`, 067), so the sender's next
payment runs. A check holds the sender's topic while it runs.

## Bump

Due when the current version is in none of the blocks its index read, the
index has read at least `bump_after` blocks past its `broadcasted_at`, and
fewer than `max_bumps` were made. A lagging or stuck indexer has not read
those blocks, so it never makes a bump due.

```rust
let mut builder = wallet.transaction(&asset)?;
builder.from_existing(&current)?;
builder.set_fee(intent.fee);
builder.bump_fee();
match builder.sign().await {
    Ok(next) => replace(current, next), // below
    Err(error) if error.kind == FeeCap => resend(current), // reason: stuck
    Err(error) => …,
}
```

- **Replace**, one write: the current pending row and its legs out, the
  new version's in (the funds are held once); the new version's
  `transaction_index` (born at the tip now); its txid appended to the
  job's state. Then the new version is sent. Refused by every node: the
  write is undone (the old version stays current, the new index goes).
- **The old version's index stays**: whichever version lands is found,
  and the payment is done.
- **At the cap** (`FeeCap`), or `max_bumps` reached: the same bytes are
  sent again, and the job's reason says `stuck`.
- **A chain that cannot replace** (`from_existing` unsupported, Solana):
  the same bytes are sent again; a Solana payment ends by its expiry.
- Signed under the job's sender topic, so no other payment of the sender
  signs meanwhile.

### The builder keeps its state

A builder holds what it signs: the payment, the slot (the coins, or the
nonce) and the fee. Its saved form is the signed transaction itself,
which the server already keeps (`bytes` and legs); `from_existing` loads a
builder back from it.

```rust
/// How fast a payment should land; each chain maps it to its own fee.
pub enum FeeLevel { Low, Normal, High }

pub trait Builder: Send {
    fn with_default_source(&mut self, source: Signer);
    /// A new payment; its slot (coins or nonce) is picked when signing.
    fn from_transfer(&mut self, transfer: Transfer) -> Result<(), Error>;
    /// Continues `signed`, one this wallet signed: its slot, payment and
    /// fee. Default: unsupported (Solana).
    fn from_existing(&mut self, signed: &Transaction) -> Result<(), Error>;
    /// Today's estimate at `level`. Default: `Normal`.
    fn set_fee(&mut self, level: FeeLevel);
    /// After `from_existing`: enough to replace it: today's estimate at the
    /// level set, and at least the chain's minimum step above its fee.
    fn bump_fee(&mut self);
    /// Signs. Above the chain's fee cap: an error of kind `FeeCap`.
    fn sign(&self) -> Future<'_, Result<Transaction, Error>>;
}

/// From the chain's config.
pub struct Tracking {
    /// How often a broadcast job checks.
    pub check: Duration,
    /// Blocks read past `broadcasted_at`, not found, before a bump.
    pub bump_after: u64,
    pub max_bumps: u32,
}
```

`set_fee` takes a level, not a number, and `bump_fee` takes nothing: each
chain maps the level to its own fee and keeps its replacement rule and
cap. A signed transaction does not carry its level, so the job sets it
again from its input before `bump_fee`.

## Per chain

| | `expires_height` | `from_existing` | `bump_fee` step | Fee cap | `Tracking` (`check` / `bump_after` / `max_bumps`) |
| --- | --- | --- | --- | --- | --- |
| Bitcoin | `None` | the coins from its spent legs, the payment from its outputs | a higher fee rate and a higher total fee (1 sat/vB × size); the change pays it | `max_fee_rate` (new setting) | 60 s / 6 blocks / 5 |
| Ethereum | `None` | the nonce, recipient and amount from its bytes | `maxFeePerGas` and `maxPriorityFeePerGas` each at least 10% higher | `FeeLimits` | 30 s / 50 blocks / 5 |
| Solana | `lastValidBlockHeight` from `getLatestBlockhash` | unsupported (the default) | — | — | 10 s / — / 0 |

A chain sets `expires_height` when it signs, maps `FeeLevel`, and
implements `from_existing` and its `bump_fee` step if it can replace.
Everything else is `sdk/base` and the server, the same for all.

## Schema

Built:

- `"transaction"`: `expires_at` dropped; `sent_at` is when a node may hold
  it (accepted, or a transient failure), NULL while recorded and not sent,
  and once mined.
- `transaction_forgotten (id, chain, network, txid, reason, forgotten_at,
  bytes, legs)`: every forgotten pending transaction; reasons `refused`,
  `dead`, `jump`, and next `replaced`.
- `docs/design/084-migrate-1.sql`; not applied to any real database.

From 085: `transaction_index (transaction, broadcasted_at, indexed_at,
found, expires_height, state, reason)`.

## API

- `POST /v1/wallets/{id}/transactions` (`send_payment`) takes one payment,
  `{asset?, destination, amount, fee?}`, and enqueues one job.
- `GET /v1/jobs`: a broadcast shows its `txid` and `accepted`. It succeeds
  once its payment is in a final block; failed says why: `refused`,
  `dead`, `forgotten`, `insufficient-funds`, `invalid`; its reason says
  `stuck` while at its bump limit.
- Triggers enqueue one job per payment, all of a round's jobs in its one
  write.

## Iterations

| # | What | Status |
| --- | --- | --- |
| 1 | No clock: `expires`, `pending_ttl_ms`, `expires_at` and the `expire` job removed; `transaction_forgotten` | built |
| 2 | Ethereum nonce counts our sends in flight (`TransactionIndexStore::find_sent`, was `Storage::pending`) | built |
| 3 | One job = one payment = one transaction; builders sign one transfer; API and triggers | built |
| 4 | The broadcast job owns its payment until final or dead; no `pending` job | built |
| 5 | The job watches its payment's transaction index (085): made in the record write, read by every check; `alive` and `is_displaced` removed | in progress (085) |
| 6 | The stateful builder (`from_transfer`, `from_existing`, `set_fee`, `bump_fee`, `sign`), `FeeLevel`, `Tracking`, the bump; Ethereum | next |
| 7 | Bitcoin `from_existing` and `bump_fee`, `max_fee_rate` | next |

## Tests (iterations 5–7)

- A payment found at a final block is done; found above it, checked
  again; its block reorged, sent again.
- A payment whose index has not read `bump_after` blocks yet (a lagging
  indexer) is never bumped.
- Solana: read past its `expires_height`, not found → forgotten (`dead`),
  failed; found before it → done.
- Ethereum and Bitcoin: never dead by age; sent again at every check.
- A given-up index (a Bitcoin jump) → failed `forgotten`.
- Bump: `bump_after` blocks read, not found → replaced: the row swapped,
  the funds held once, a new index; refused → undone; at the cap or
  `max_bumps` → sent again, reason `stuck`; the old version found instead
  → done, the current row forgotten (`replaced`).

## Open

1. Bitcoin `max_fee_rate`: what default?
2. Cancel (a replacement paying the sender itself): a manual endpoint,
   later.
3. `FeeLevel` per chain. Proposed: Bitcoin confirmation targets of 12, 3
   and 1 blocks; Ethereum the priority fee at 0.5×, 1× and 2× the
   estimate; Solana a priority fee of none, the recent median and the
   recent 75th percentile.

## Notes

- A trigger round that falls due while a check holds the sender's topic is
  skipped (076); rare at these intervals.
- A sign-time failure retries three times, then fails; nothing was
  recorded, so nothing is held.
