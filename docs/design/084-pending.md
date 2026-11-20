# 084: Pending transactions

Status: built: no clock, the Ethereum nonce, one payment per job, the
broadcast job owning its payment, the stateful builder, fee levels and the
Ethereum bump (iteration 6), Solana's priority fee and bump (iteration 8;
see As built). Next: the Bitcoin bump (iteration 7). Replaces the expiry of
066.

066 held a sent transaction's funds until it was mined **or its time was
up**. A clock cannot tell that a transaction can never land: one past its expiry
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
| How is "expired" known? | Only by the transaction's own deadline: Solana's blockhash expiry (`expires_height`), against the height of the newest kept block at or below the network's confirmed position (`block::db::find_height_at_or_below`); the index read through the confirmed position, not found (`TransactionIndex::expired`). The broadcast job judges it, not the indexer (087). Ethereum and Bitcoin transactions never expire; they are bumped instead. An expired Solana version is dropped and signed anew, until `max_bumps`; then the job fails `expired`. | With keys used only by this server, nothing else can spend a payment's coins or nonce; our own earlier versions are watched by their own indexes. |
| When is a payment bumped? | One that never expires (Ethereum, Bitcoin): when its index has read `bump_after` blocks past `broadcasted_at` and it is in none of them. One that expires (Solana): never in flight; once expired it is dropped and signed anew. At most `max_bumps` times. | Counted in blocks the index actually read, a stuck or lagging indexer never triggers a bump: it has not read those blocks. A Solana transaction has no nonce or coin to replace it by: a next version is safe only once the old one can never land. |
| What does a `FeeLevel` mean? | A multiple of the tip of the network's rate now (`FeeRate { base, tip }`): `low`, `normal` and `high` pay × 0.5, 1 and 2 of the tip; the base is the network's, so `low` never goes below what nodes accept. | One meaning for a caller whatever the chain. |
| How is a bump signed? | The same builder as a first send: `load(&state)`, `with_fee(Fee::Times(1.1))`, `sign()`: a tenth over the higher of the rate now and the one it paid. `state` is what the builder `save`d when it signed the current version, kept in the job's state. | One fluent API for sending and bumping; each chain keeps its floor and fee cap. |
| Keys used outside this server | Not supported (like history gaps, 083). Such a payment may end stuck at its bump limit, its funds held. | Detecting it needs per-chain conflict bookkeeping for a case we do not support. |
| The Ethereum nonce | The higher of the node's pending count and one above our highest pending nonce. | A load-balanced node's mempool may miss our last send; our own records do not. |
| What a forgotten send leaves | Its broadcast job's state: the outcome, the reason, every txid it sent; the `transaction` rows still pending and their legs go (087 dropped `transaction_forgotten`). | A soft delete would need a filter in every read; one missed filter brings a forgotten send back. The job already is the payment's whole story. |
| How does every payment end? | Resolved, never abandoned: final, reverted, expired, refused, cancelled or forgotten (Resolution). Out of bumps, or unable to bump, it is cancelled in its slot. | A payment stuck at its bump limit held its funds forever. |

## The broadcast job

```
record + watch ─► send ─► check ─┬─ found, final        → done
                           ▲     ├─ given up / gone     → failed (forgotten)
                           │     ├─ bump due            → bump, watch the new version
                           │     ├─ Solana, expired     → dropped; signed anew, or failed (expired)
                           └─────┴─ otherwise           → send the same bytes again
```

Input: `{chain, network, address, asset, to, amount, fee}` (`fee` a
`FeeLevel`, `Normal` when absent). State: the txids of every version it
sent (the last is current), how many bumps, and the current version's
builder state (`builder`; absent in jobs queued before it was kept: those
are watched, never bumped).

1. **Sign** the payment, once:

   ```rust
   let mut builder = wallet.transaction(&asset)?;
   builder.from_transfer(transfer)?;
   builder.with_fee(Fee::Set(intent.fee));
   let signed = builder.sign().await?;
   let state = builder.save()?;                                       // the job's `builder`
   let index = TransactionIndexTarget::new(chain, signed.id.clone()) // + with_expires_height on Solana
       .index()
       .await?;                                                       // broadcasted_at: the tip now
   ```

2. **Record and watch**, one write: the funds check (what the sender holds
   beyond what is in flight), the pending row and its legs (its coins and
   funds are held from now on), its `transaction_index` row, the job's
   state. Short: rolled back, the job fails `insufficient-funds`.
3. **Send** the bytes to every node, then one write:
   - accepted, or a transient failure (a node may hold it): the job's
     state `sent` (the API's `accepted`); the job checks from now on;
   - refused by every node: forgotten (`refused`), its index deleted, the
     job fails.
4. **Check**, every `Tracking::check`, reading only the job's transaction
   indexes (one per version):
   - **a version `found`** at or below the final block: done. If it is not
     the current one, the current pending row is forgotten (`replaced`).
     Found above the final block: check again;
   - **the current index given up** (`no_data`, a Bitcoin jump) or gone:
     failed `forgotten`;
   - **a bump due** (below): bump;
   - **Solana**, a version expired (its index read through the final
     block, not found, that block's height above its `expires_height`):
     dropped in one write (its pending row, legs and index); none
     left: signed anew while bumps are left, else failed `expired`;
   - **otherwise**: send the same bytes again; check again.

   When the job ends, its transaction indexes are deleted.

The send is between two writes, not inside one: a rollback cannot take
bytes back from a node, and a crash with the write still open would lose
the record of a payment that may already be out. Recording first keeps
it; a rerun sends the same bytes again.

Between checks the job blocks nobody (`blocks`, 067), so the sender's next
payment runs. A check holds the sender's topic while it runs.

## Bump

Due (`TransactionIndex::bump_due(bump_after)`, the one place that says so; how many bumps a payment may have, `max_bumps`, is the job's to judge):

- **a transaction that never expires** (Ethereum, Bitcoin): when it is in
  none of the blocks its index read, the index has read at least
  `bump_after` blocks past its `broadcasted_at`, and fewer than
  `max_bumps` were made;
- **one that expires** (Solana): never due in flight. It cannot be
  replaced in flight (no nonce or coins name the payment), so a next
  version is safe only once the old one can never land: once expired it is
  dropped, and with no version left a new one is signed while fewer than
  `max_bumps` were made. After `max_bumps` the job fails `expired`.

A lagging or stuck indexer has not read those blocks, so it never makes a
bump due.

```rust
let mut builder = wallet.transaction(&asset)?;
builder.load(&state)?;
builder.with_fee(Fee::Times(1.1));
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
  sent again, and the job's reason says `stuck` (Solana at `max_bumps`:
  expired, above).
- **No builder state** (a job queued before it was kept): never due a
  bump; the same bytes are sent again.
- Signed under the job's sender topic, so no other payment of the sender
  signs meanwhile.

### The builder keeps its state

A builder keeps what it chose when it signed: the payment, the source's
address, the slot (the coins, or the nonce and gas limit, or the
compute-unit limit) and the rate. `save` writes it as JSON (never a key);
`load` reads it back, and the next `sign` continues it.

```rust
/// `base::fee`: one unit's price, `FeeRate { base, tip }` (`times`,
/// `higher`, `at` scaling only the tip, `total`); its provider
/// (`FeeRateProvider`, kept by `FeeRateProviderCached`, the one
/// `Service::rates`), and the rule a builder signs at.
pub enum Fee { Set(FeeLevel), Times(f64), Rule(Box<dyn FeeRule>) }

pub trait Builder: Send {
    fn with_default_source(&mut self, source: Signer);
    /// A new payment; its slot (coins or nonce) is picked when signing.
    fn from_transfer(&mut self, transfer: Transfer) -> Result<(), Error>;
    /// The fee's rule; `Fee::default()` (`Set(Normal)`) until set.
    fn with_fee(&mut self, fee: Fee);
    /// `fee.rate(now, old)`, raised to the chain's floor, × the exact
    /// size; signs and keeps what it chose. Above the chain's fee cap: an
    /// error of kind `FeeCap`.
    fn sign(&mut self) -> Future<'_, Result<Transaction, Error>>;
    /// What the last sign chose.
    fn save(&self) -> Result<serde_json::Value, Error>;
    /// A saved state: the next sign continues it, never below its rate.
    fn load(&mut self, state: &serde_json::Value) -> Result<(), Error>;
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

A `Fee` is a rule over rates, not a number: each chain keeps its own
rate's shape, floor and cap. A bump needs no level: `Times(1.1)` is over
the higher of the rate now and the old one.

## Resolution

Every payment's job ends resolved (087). Its states, and what moves each:

| State | Moves on | To |
| --- | --- | --- |
| new | signed, held, recorded and watched in one write | recorded |
| new | short of funds, a sender index missing, invalid | failed (nothing recorded) |
| recorded | broadcast accepted, or no sure answer | sent |
| recorded | refused by every node | **refused** |
| sent | a version found above the final block, or not read that far | sent (waits: no resend, no bump) |
| sent | that block reorged: pending again, its spend held, its index capped | sent (missing) |
| sent | missing: in none of the blocks read | sent (same bytes again) |
| sent | missing `bump_after` blocks past the current version's broadcast, bumps left | bumped (a version a tenth up, watched) |
| sent | bump due, no bump left, or the bump not signable (fee cap, short of change or funds) | cancelling |
| cancelling | a cancel in the payment's slot: Ethereum zero ether to the sender at its nonce, Bitcoin its coins back in one output (RBF), at least a tenth over the last version; its pending spend is its fee | sent (the cancel watched as a version) |
| cancelling | due again | a cancel again, a tenth up |
| cancelling | no cancel signable (fee cap; Solana `Unsupported`) | sent (same bytes again, reason `stuck`) |
| sent | a payment version found succeeded in a final block | **final** (done) |
| sent | a version found reverted in a final block | **reverted** |
| sent | a cancel found in a final block | **cancelled** |
| sent | Solana: a version expired (index read through the final block, not found, the final height above its `expires_height`) | that version dropped in one write: its pending row, legs and index; its spend released |
| sent | that drop, another version still pending | sent (that one watched) |
| sent | that drop, no version left, a bump left | recorded (a new version on a fresh blockhash, a tenth up, its own funds check; short: **expired**, `insufficient-funds`) |
| sent | that drop, no version left, no bump left | **expired** |
| sent | its index given up, or its row gone (a jump) | **forgotten** |

Terminal: final, reverted, expired, refused, cancelled, forgotten; every other
state is checked again at `Tracking::check`. Bumps off (`max_bumps` 0) or no
builder state (a job queued before it was kept): never bumped nor cancelled,
sent again until it resolves. A cancel is signed by the payment's builder:
`load(&state)`, `cancel()`, `with_fee(Fee::Times(1.1))`, `sign()`; a
cancel's saved state loads as a cancel.

The job's end, one write: every version still pending is deleted with its
legs (what never landed frees its funds); a mined version stays chain
history (the landed one; a reverted one's or a landed cancel's fee); every
version's transaction index is deleted; the job's state keeps `outcome`,
`reason` and every txid sent (`replaced`, `txid`, `cancels`): the only
record.

## Per chain

| | `expires_height` | Saved state | `FeeRate` (`Times(1.1)` moves it) | Fee cap | `Tracking` (`check` / `bump_after` / `max_bumps`) |
| --- | --- | --- | --- | --- | --- |
| Bitcoin | `None` | the payment, source, coins spent, change address, rate | sat/kvB: base `mempoolminfee`, tip `estimatesmartfee(fee_target_blocks, default 1)` above it; at least `fee_floor`; a replacement pays at least the old fee plus the base over its size, from its change | none yet (`max_fee_rate`, Open 1) | 60 s / 6 blocks / 5 |
| Ethereum | `None` | the payment, source, nonce, gas limit, rate | wei per gas: base the latest base fee, tip the node's; signs `2 × base + tip`, both fees ≥ 1.1 × the saved ones after `load` | `FeeLimits` | 30 s / 50 blocks / 5 |
| Solana | `lastValidBlockHeight` from `getLatestBlockhash` | the payment, source, compute-unit limit, rate; signed on a new blockhash | micro-lamports per compute unit: base 0, tip the network's median recent priority fee, at least 1; signed anew once expired | `max_priority_fee` (new setting) | 10 s / unused / 5 |

A chain sets `expires_height` when it signs, implements its
`FeeRateProvider`, `save` and `load`.
Everything else is `sdk/base` and the server, the same for all.

## Schema

Built:

- `"transaction"`: `expires_at` dropped; `broadcasted_at` (087, was
  `sent_at`) is when this server recorded it, before its first broadcast:
  set exactly on our sends, kept once mined; whether a node accepted it is
  the job's state (`sent`).
- `transaction_forgotten (id, chain, network, txid, reason, forgotten_at,
  bytes, legs)`: every forgotten pending transaction; dropped by 087 (the
  job's state is the record, Resolution).
- `docs/design/084-migrate-1.sql`; not applied to any real database.

From 085: `transaction_index (transaction, broadcasted_at, indexed_at,
found, expires_height, state, reason)`.

## API

- `POST /v1/wallets/{id}/transactions` (`send_payment`) takes one payment,
  `{asset?, destination, amount, fee?}`, and enqueues one job.
- `GET /v1/jobs`: a broadcast shows its `txid` and `accepted`. It succeeds
  once its payment is in a final block; failed says why: `refused`,
  `reverted`, `expired`, `cancelled`, `forgotten`, `insufficient-funds`,
  `invalid`; its reason says `stuck` while even a cancel cannot be signed.
- Triggers enqueue one job per payment, all of a round's jobs in its one
  write.

## Iterations

| # | What | Status |
| --- | --- | --- |
| 1 | No clock: `expires`, `pending_ttl_ms`, `expires_at` and the `expire` job removed; `transaction_forgotten` | built |
| 2 | Ethereum nonce counts our sends in flight (`TransactionIndexStore::find_sent`, was `Storage::pending`) | built |
| 3 | One job = one payment = one transaction; builders sign one transfer; API and triggers | built |
| 4 | The broadcast job owns its payment until final or expired; no `pending` job | built |
| 5 | The job watches its payment's transaction index (085): made in the record write, read by every check; `alive` and `is_displaced` removed | in progress (085) |
| 6 | The stateful builder (`from_transfer`, `from_existing`, `set_fee`, `bump_fee`, `sign`), `FeeLevel`, `Tracking`, the bump; Ethereum | built |
| 7 | Bitcoin bump (the same coins, the change pays); `max_fee_rate` left open | built |
| 8 | Solana: a priority fee on every transaction, levels as percentiles of recent fees, `max_priority_fee`, `from_existing`, `bump_fee`; a payment that expires signed anew once expired | built |
| 9 | `Fee` over a chain's `FeeRate` (`with_fee` replaces `set_fee` and `bump_fee`); one `FeeRateProviderCached` per chain replaces each chain's price cache; levels × 0.5, 1, 2 everywhere; Solana's rate network-wide | built |
| 10 | `FeeRate { base, tip }` one plain struct for every chain (levels scale the tip); `save` / `load` replace `from_existing` and its byte decoding; the job keeps the builder's state | built |

## Tests (iterations 5–8)

- A payment found at a final block is done; found above it, checked
  again; its block reorged, sent again.
- A payment whose index has not read `bump_after` blocks yet (a lagging
  indexer) is never bumped.
- Solana: expired (index read through the final block, not found, the
  final height above its `expires_height`) → dropped, failed `expired`
  with nothing left; the final height not past yet → sent again; found
  before it → done. The indexer never changes a state for expiry.
- Ethereum and Bitcoin: never expire; sent again at every check.
- A given-up index (a Bitcoin jump) → failed `forgotten`.
- Bump: `bump_after` blocks read, not found → replaced: the row swapped,
  the funds held once, a new index; refused → undone; at the cap or
  `max_bumps` → sent again, reason `stuck`; the old version found instead
  → done, the current row forgotten (`replaced`).
- Solana bump: expired → dropped and signed anew, sent by the next run;
  the new version found final → done; expired at `max_bumps` → failed
  `expired`. In the chain: each level its
  percentile, both ComputeBudget instructions, the priority fee in the fee
  leg, `FeeCap` above `max_priority_fee`, `save` / `load` of a SOL and a
  token transfer, the bump's step, a new blockhash.

## Open

1. Bitcoin `max_fee_rate`: what default?
2. Cancel (a replacement paying the sender itself): a manual endpoint,
   later.
3. `FeeLevel` per chain. Decided (iteration 9): × 0.5, 1 and 2 of the
   network's rate on every chain; the percentiles and Bitcoin's 12/1-block
   targets are gone.

## As built (iteration 6)

Decisions beyond the design:

- **`sdk/base`:** `FeeLevel` (`low`, `normal`, `high`; `normal` by default)
  and `ErrorKind::FeeCap`. `Builder` keeps its payment: `from_transfer`
  checks address and amount; `from_existing` defaults to `Unsupported`;
  `bump_fee` defaults to nothing (a chain that cannot replace); `set_fee`
  has no default, so every chain says what a level means. `sign(&self)`
  replaces `sign(self: Box<Self>, transfer)`.
- **`Tracking`** is on `Service`, from each chain's `Config::tracking`,
  whose `settings` keys are `check_ms`, `bump_after` and `max_bumps`.
  `Tracking::default()` (checked every minute, never bumped) stands in for a
  network that did not build. `TransactionIndex::bump_due(bump_after)`: the
  index says `Missing` and `indexed_at.position - broadcasted_at >=
  bump_after` (087: never for one that expires). A job checks every `Tracking::check` of its chain: the
  fixed `CHECK` and `Broadcast::new`'s `check` are gone (the mock Ethereum
  network of the server suite sets `check_ms` 1000).
- **Fee levels** (Open 3): Ethereum tips at 0.5×, 1× and 2× the node's
  (the max fee follows: `2 × base fee + tip`). Bitcoin asks
  `estimatesmartfee` for 12 blocks, `fee_target_blocks` (6 by default, so
  `normal` is today's) and 1 block; its cached estimator keeps one rate per
  level. Solana paid no priority fee then, so `set_fee` did nothing
  (iteration 8 gives it one).
- **Ethereum:** `from_existing` decodes the EIP-1559 bytes (nonce,
  recipient or the token `transfer`'s, amount, gas limit, fees) and the
  sender from the signature; `sign` refuses a source that is not that
  sender. `bump_fee`: each of `maxFeePerGas` and `maxPriorityFeePerGas` is
  the level's or the old one plus a tenth (rounded up), whichever is
  higher. `FeeLimits::total` now checks each fee per gas too; a quote or a
  signed fee above any ceiling is `FeeCap`. Continued without a bump, a
  signed transfer signs to the same bytes.
- **The job's state** keeps `txid` (the current version) and `sent`, and
  adds `replaced` (oldest first, left out while empty) and `bumps`, which
  counts replacements sent, refused ones too: a node that keeps refusing
  them stops at `max_bumps`. `SendIntent::fee` defaults to `normal`. Both
  decode what was queued before.
- **A check** reads each version's index: one `Final` → done (the current
  row, when another version landed, forgotten `replaced`); one `Reverted`
  (final, reverted: only its fee paid) → failed `reverted`, ended the same
  way; one found above
  the final block → wait (no resend, no bump); then the current one's
  status as before; `Missing` and `bump_due` → step 4.
- **Step 4, the bump:** at `max_bumps` → stuck. Signed as designed; a
  transient failure checks again with why; any other (`FeeCap`,
  `Unsupported`, a decode failure) → stuck. One write
  (`transaction_indexes::replace`): the current row forgotten `replaced`
  (`forget` gives up no index for `replaced`: the old one keeps watching),
  the new one recorded (`transaction_indexes::record`, the funds check),
  its index from the tip now, and the job's state; short → stuck
  `insufficient-funds`, nothing written. Then sent once. Stuck: the same
  bytes again, reason `stuck: …`.
- **Undo** (every node refused the replacement), one write
  (`transaction_indexes::restore`): the new row forgotten `refused` and its
  index deleted, the old one's `transaction_forgotten` row dropped, and the
  old row and legs written back exactly as the run read them before the
  bump (`broadcasted_at` too), with the funds check; the state names the old
  version again, `bumps` kept. Only the run that sent the replacement can
  undo it (it holds the old row); after a crash between the write and the
  send, the replacement stays current, is sent again at each check and may
  be bumped in turn, while the old version's index still watches.
- **The job's end** deletes every version's index.
- **API:** `fee` on `POST /v1/wallets/{id}/transactions` (schema
  `FeeLevel`); triggers send at `normal`; the UI send dialog has a fee
  select. `docs/openapi.json` was printed by `api::openapi()` from a
  throwaway test (the binary was not run).
- **Not built:** Bitcoin `from_existing`, its `bump_fee` and `max_fee_rate`
  (iteration 7). Until then a Bitcoin payment due a bump (6 blocks) is sent
  again each check with reason `stuck: …`.

## As built (iteration 8: Solana)

Decisions beyond the design:

- **Priority fee:** every transaction carries `SetComputeUnitLimit` and
  `SetComputeUnitPrice` (micro-lamports per unit) first, encoded by hand
  (no new crate). Its fee leg is the signature price plus
  `ceil(price × limit / 1 000 000)` lamports; a new token account's rent
  is still paid beside it, as before.
- **Compute-unit limit:** fixed per kind, not simulated: 40 000 for SOL,
  100 000 for a token, about twice what each uses (the 44-byte memo alone
  takes some 16 000: the Memo program spends about 350 a byte; creating a
  token account about 25 000, `TransferChecked` about 6 500). The same
  instructions every time, so a fixed limit fits; it needs no node call,
  cannot fail on a sender that cannot run the transfer yet, and lets the
  estimator price the fee leg exactly before signing. A token transfer
  always carries the idempotent create, so its one limit covers both. The
  costs are the programs' known ones, not measured on a cluster here.
- **Levels:** `getRecentPrioritizationFees` for the accounts the
  transaction writes (the sender and recipient; for a token the sender and
  both token accounts), the 25th, 50th and 75th percentile of its slots.
  The cached estimator keeps each set of accounts' answer `fee_ttl_ms`
  (dropping older ones as others are asked); an estimate prices `normal`.
- **`max_priority_fee`** (setting, micro-lamports per unit): default
  1 000 000 (one lamport), so at most 0.00004 SOL for a SOL transfer and
  0.0001 SOL for a token one: far above what a payment's accounts (a
  sender and its recipient, rarely contended) usually see, it stops a
  runaway quote or bump, not a busy network. A price above it, quoted or
  bumped, is `FeeCap`.
- **Bump:** `from_existing` decodes the signed bytes (the two ComputeBudget
  instructions, the SOL transfer or the token's create and
  `TransferChecked`, the memo) and the fee payer, which `sign` requires to
  be the source. `bump_fee`: the level's price or the old one plus a
  tenth, rounded up and at least 1 micro-lamport (a tenth of nothing is
  nothing), whichever is higher. `sign` takes a fresh blockhash, so a new
  `expires_height`.
- **When:** `TransactionIndex::bump_due(bump_after)`: an index with an
  `expires_height` is never due; one without, as before. Since 087 an
  expired version (`TransactionIndex::expired`) is dropped by the job and
  signed anew while `max_bumps` allows, else the job fails `expired`;
  missing at `max_bumps` is due still, so the job reports `stuck`. Solana
  `Tracking`: 10 s, 5 bumps, `bump_after` unused.

## As built (iterations 7 and 10)

- **`sign(&mut self)`**: it keeps what it chose as its state, so a second
  `sign` continues the first, as after `load`; `from_transfer` clears it.
  `save` before any sign, or `load` of another chain's state: `Failed`.
- **A state names its source by address**; `sign` refuses a signer of
  another address. Each chain's is a private serde struct.
- **Bitcoin bump**: inputs already signal replaceability (sequence
  `0xfffffffd`). The fee is the higher of the rate's and the old fee plus
  the new base over the size; the state saves the rate that fee pays, so
  the next bump steps from it. No change, or change that would fall below
  dust: `Failed` with `InsufficientFunds` (the job: stuck).
- **Ethereum**: `FeeLimits::fees` and `EthRate` are gone; the provider
  only reads the two quotes, `FeeLimits::total` checks every ceiling.
- **Server**: `Progress::builder` (`serde(default)`); without it a check
  never finds a bump due, so such a payment is sent again (or, on Solana,
  fails `expired`) as before bumps.

## Notes

- A trigger round that falls due while a check holds the sender's topic is
  skipped (076); rare at these intervals.
- A sign-time failure retries three times, then fails; nothing was
  recorded, so nothing is held.
