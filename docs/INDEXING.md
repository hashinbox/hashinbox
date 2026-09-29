# Indexing

Indexing turns chain-native blocks into canonical transaction history and, where a chain needs
it, a live UTXO projection. It runs embedded in `payment-server` (`apps/server`). It is not a
service, event feed, raw-block archive, or migration tool, and stores no pending-transaction
state. It owns the list of what it observes: `Watching::watch` writes the durable `WatchStore` row,
`Service` keeps the hot set every pass reads, and nothing calls back into the caller to ask
(docs/design/030 §8 (ex-029)).

## Object graph

```text
chain RPC -> BlockSource -> native block -> BlockInterpreter(+active addresses) -> InterpretedBlock
          -> Service<S, I, R> (one network: its watches, its overlay, its source
             and interpreter, its store reference; implements Indexing)
          -> WatchStore + IndexStore
          -> one Repository -> one SQLite file, every network

Bitcoin/Ethereum/Solana Service -> Index<dyn Indexing> (routes by Network)
Service::history()  -> ObservedHistory (the repository's page + this overlay)
Service::balances() -> ObservedBalance (the repository's fold + this overlay)
Service::outputs()  -> ObservedOutputs (the repository's UTXOs + this overlay)
Watching::watch(id, &dyn Holder, Start) -> Service's own watch set + WatchStore row
Indexing::sync() (no selection)                HTTP/wallets -> Checkpoint; per-network queries
Listener::committed(&BlockCommit)              after each commit, synchronous, no veto
```

`Index::sync` asks **every** unit and never stops at the first failure: one chain's node
refusing a call must not cost the other chains their pass (docs/design/019), and that holds for a
caller driving bounded `sync` by hand, not only under the supervisor. The answer reports the worst
failure it met -- terminal before retryable -- and a caller that needs each network's outcome
separately drives `Index::units`, which is what the supervisor does.

`indexing::Supervisor` (cargo feature `supervisor`) drives one task per `SyncUnit` (one per
composed chain) and owns the per-network state machine, backoff, quarantine, and the `SyncMonitor`
callback. It lives in the same crate as the vocabulary it fills -- `NetworkState`,
`NetworkHealth`, `SyncHealth` -- which the HTTP layer reads. A failing network carries the
`IndexError` itself, not a restatement of it. With the feature
off the crate names no async runtime; `apps/api` takes it that way.

## Caller contract

| Trait | Role |
|---|---|
| `Checkpoint` | a `Network`'s current `BlockRef`: its **confirmed line**, not its tip |
| `History` | `history(&dyn Holder, Page)` for confirmed transaction pages plus the pending ones. Answered by `ObservedHistory`, never by an indexer |
| `Holder` | whose history, balance or observation a call is about: an address and its asset. A `wallets::Wallet` is one; `Subject` is one with no wallet behind it |
| `Balances` | `balance(&dyn Holder) -> Balance`: the settled fold over the movements the store holds, plus what the overlay still holds above the line, with a `Completeness` beside it. Answered by `ObservedBalance`, never by an indexer. Atomic units only |
| `Watching` | `watch(id, &dyn Holder, Start) -> Watch`, `unwatch(id) -> Option<Watch>` and `watched(&Network)`: the index's own durable list. `unwatch` stops inspection, deletes no indexed row, takes the same publication permit a watch does, and is idempotent; it drops that observation's own `indexed_through`, because the claim holds only while the observation exists, so a re-watch reads its range again |
| `Backfilling` | `cancel(&Network) -> Vec<Watch>`: stop asking for the history that network owes. Each affected watch is left as one created now would be — same id, birthday at the checkpoint's successor, **nothing inspected** — which settles the range without claiming to have read it, and nothing indexed is removed. A durable write, which is why it is not on `SyncControl` |
| `Indexing: Backfilling + Checkpoint + Watching` | `network()`, `listen(Arc<dyn Listener>)` and `sync()` — one network, one `SyncStatus`, and no selection argument. The minimum: every method is answerable by a `Repository` alone, so a sink that answers no query still gets an index it can observe wallets with and drive. Many networks is `Index`, which aggregates these and is not one of them |
| `History` | `history(&dyn Holder, Page) -> TransactionPage`: confirmed *plus* what is still above the line. **On no indexer and routed by no `Index`** — no repository can answer it and no index can either, so `ObservedHistory` joins one repository's `ConfirmedHistory` pages to that index's overlay. Each chain builds one for its own network (`Service::history`); `wallets::Composition` collects them into the one `Arc<dyn History>` a consumer declares. A balance is collected the same way, over `ObservedBalance` (`Service::balances`), and a spendable set is `ObservedOutputs` (`Service::outputs`). `apps/api` holds the composition itself, `Arc<Index>`, for what the aggregate does answer: watches, checkpoints and readiness |
| `Listener` | `committed(&BlockCommit)` after each committed block: synchronous, notification-only, zero or more |
| `OutputStore` | live UTXOs; a store port, answered for a consumer by `ObservedOutputs` over it, and on no indexer |

- `AddressFilter` = `base::Address` (network + canonical text) + the next native position the pass
  must inspect for it: what one pass needs of a `WatchProgress`. That position is the successor of
  whatever was last inspected for the address, or its birthday when nothing was — not the birthday
  itself, which would re-read what is already read. The `Watch` holds the address and birthday plus the id
  and the `AssetId` it is read in, and **no key material**: observation is indexing's and custody is
  the application's (docs/design/030 §8 (ex-029) §3.4). An interpreter turns the filter set into native addresses
  with `indexing::select_addresses`, which converts strictly (`TryFrom<&base::Address>`: exact
  canonical text of that network).
- A blank address, an address of another network, and an asset of another chain are refused by
  `watch`, where they are offered, so no pass ever carries one and no caller needs a reachable node
  to be told. Re-stating one observation under the same id and start is a no-op; what collides is a
  name that is taken — the same id over another observation, or the same `(address, asset)` under
  another id — and that is a `Conflict`.
- **The watch set is keyed by watch identity, not by address.** One address may be observed once per
  asset, so a wallet holding a chain's own currency and a token at that address is two observations
  with two ids. Scanning is deduplicated instead: one filter per address, from the earliest birthday
  any observation of it names, because `BlockInterpreter::inspect` takes addresses and no asset and
  one inspection therefore proves every asset held there. Progress, in contrast, is **one row per
  observation**, earned under that observation's own immutable birthday: two observations of one
  address advance together because one inspection serves both, not because they share a row
  (docs/design/030 §8 (ex-032) §4, and the first defect its record names).
- **Progress is one contiguous, hash-verified block per observation.**
  `WatchProgress::indexed_through` means every applicable block from that observation's birthday
  through that block was inspected for it, empty ones included; `next_required()` is where it
  resumes, and native positions may be sparse, so what is recorded is the highest block the source
  actually produced. There is no second position and no gap — which is exactly what one reference can
  honestly describe, and why the per-address `(start, resume, anchor)` triple it replaced is gone
  rather than relabelled (docs/design/030 §8 (ex-032) §2). It is always at or below the checkpoint, so the one
  `canonical_at` a pass already makes on the line vouches for every one of them.
- **`WatchProgress::caught_up(checkpoint)` is the one predicate behind the design's central trade**,
  and both of the behaviours it could have are written out in its own doc comment. Chosen: an
  observation whose own history has not reached the confirmed line is inspected for the blocks it
  owes and **not** for the ones arriving now, so a wallet imported with an old birthday does not see
  a payment arriving today until its catch-up reaches the present. What that buys is contiguity. The
  same predicate gates every read that touches the address, through
  `Completeness::while_backfilling`, so "still catching up" cannot mean one thing to the scan and
  another to a balance. Flipping the trade is that one comparison.
- `Index` requires at least one child, rejects duplicate networks, routes each `watch` and each
  read to the unit that owns the address's network, and syncs every child in order. A unit with
  nothing watched still follows its tip. `Index::over(repository).chain(..).chain(..).build()?` is
  the assembly flow: the store is handed to each chain by the index rather than by the caller, and
  `build` validates — a duplicate network, a chain that could not compose — **without launching a
  task**. `Index::of(indexers)` is the same check over children composed some other way, which is
  what `wallets::Composition` uses because its chains compose asynchronously.
- Runtime watch admission: each sync captures a plan (checkpoint + filter revision); each commit
  takes a permit that rechecks both. A new watch waits for an in-flight commit, uses the resulting
  checkpoint's successor as a `Start::Fresh` birthday, then bumps the revision so an older plan
  cannot commit. The list has one owner now, but two concurrent actors still reach it — a running
  `watch` and the pass on the supervisor's task — which is why the handshake stays. Mutex guards
  never cross `.await`, which is also why the set stays hot in memory: `NetworkAdmission::plan`
  captures it through a synchronous closure while `WatchStore::all` is async, so the store is the
  mirror, read once before the first pass.
- A plan also carries its **address selection**, which is not that revision: the revision counts
  publications, and the selection names which addresses cover which blocks above the line. Every
  held block is stamped with it, and a plan whose selection *widened* — an address published, an
  observation finishing its catch-up and joining the front, a reload of the mirror restoring
  progress — makes every held block interpret again before it commits. One number would have made
  admission the only thing able to invalidate held facts, and a catch-up finishing publishes
  nothing.
- A registration, withdrawal or cancellation whose durable write may have landed unanswered — a
  store error, or a cancelled future — takes the rows out of the hot set and marks the mirror
  stale, so the next pass re-reads `WatchStore::all` and restores each of them whole. `add` is
  documented as leaving an uncertain outcome; this is what retries it rather than assuming a
  rollback.
- `SyncConfig::new(&impl Networked, SyncPolicy::new(batch_size))`; `batch_size` MUST be > 0.
  `SyncPolicy` carries one other number, `max_backfill_blocks` (default `100_000`, set with
  `.with_backfill(n)`): how far below the checkpoint a *new* watch may start. A `Start::At` further
  down is refused by `watch` itself with `InvalidRequest`, naming the earliest start it would
  accept — synchronously, before any block is fetched, because a mistyped start that fails in the
  background an hour later is not a refusal (docs/design/024 §7). Where the confirmed line sits is
  still the source's own answer (`ChainTip::confirmed_tip`), not a configuration value, and
  rollback retention described a journal that no longer exists (docs/design/030 §8 (ex-025)).

## Block coordinates

Every `BlockRef` has:

| Field | Used for |
|---|---|
| `BlockPosition` | native RPC coordinate (Solana slot): traversal, canonical lookup, restart, readiness, birthdays |
| `BlockHeight` | produced-block count: confirmations, ordering |
| hash | canonicality |
| optional parent (position + hash, atomic) | absent only at genesis |

Bitcoin/Ethereum positions are dense (`position == height`); Solana slots are sparse, height still
increments by exactly one per committed child. Never compute a parent as `position - 1` and never
pass a height to RPC as a position.

## Chain producers

- `BlockSource`: produced tip, bounded ordered fetch of produced blocks in a position range (limit
  counts returned blocks; skipped positions omitted), canonical ref at a position.
- `BlockInterpreter`: one native block + addresses active at its position -> `InterpretedBlock`
  (`BlockRef`, transaction drafts with every stable movement, `OutputChanges`). No storage types.
- Movements: transfer, input, output, mint, burn. Bitcoin inputs/outputs are separate (3-in/2-out
  = 5 movements). Native and token assets have distinct `AssetId`s. Values are exact integers.
- What each chain sees per block. Bitcoin's block *is* the state transition and Solana's block
  *carries* it, so both are complete from what they already fetch. Ethereum's block carries only the
  instructions, so its source fetches a third call per non-empty block --
  `debug_traceBlockByHash` with the `prestateTracer` in `diffMode` -- and a node that cannot serve
  it is refused at startup (docs/design/021 section 6). Ether a contract moved then becomes a
  one-sided `Mint`/`Burn` with a `<txid>:residual:<address>` id, attributed to its transaction: the
  runtime's balances are the truth and the decoded `value` and `Transfer` logs are attribution over
  them, exactly as Solana reads `preBalances`/`postBalances`. Gas is inside those balances, so it is
  added back when residuals are computed and `NetworkFee` stays the one record of it. ERC-20 needs
  no trace: a `Transfer` log is emitted wherever the transfer originates.
- Solana's residual is the same rule and the same reason: recording an unattributed lamport delta
  one-sided is what makes a folded balance equal the account's real change, and `reconcile` is the
  assertion that it does, never a filter on which value effects a block may contain.
- What no transaction names. Ethereum's block body credits consensus-layer withdrawals directly to
  an execution address, with no transaction, receipt or trace entry, so a fold over transactions
  alone understates a withdrawal-funded address permanently and silently.
  `chain_ethereum::native::block::RawBlock::withdrawals` parses them out of the payload already
  fetched (Gwei converted to wei once, at the parse) and `InterpretedBlock::credits` carries them
  as `BlockCredit { id, asset, amount, to }` — a record of its own, never a synthetic
  `TransactionRef`, so a caller asking which transaction caused one gets no answer because there is
  none (docs/design/021 §4.4). `CanonicalBlock` validates them (blank identity,
  foreign chain or network, negative amount, or a repeated `(to, id)` inside one block are all
  `InvalidBlock`), the store keeps them in a collection of their own keyed `(address, height, id)`,
  and `Ledger::apply_credit` folds one as an unconditional credit of its own asset. It cannot
  double-count against a transaction that also paid the address: the two are different types read
  from different collections, and `apply` never sees a credit. Bitcoin has no such cause — every
  satoshi arrives as an output, coinbase included — and Solana's block-level rewards go to stake
  and vote accounts, which this SDK does not manage, so `credits` is empty on both.
- A credit above the confirmed line counts as `PendingValue::incoming`, for the same reason it
  counts once settled. It is the one thing a balance reports that the pending half of a *history*
  page cannot, because that page lists transactions.

## Completeness, and the one thing that can prove it

`Completeness` answers how much of the history behind a settled amount the index can prove it holds.
`Incomplete` is the honest default everywhere: an address watched from a start position later
than its first funding folds to a smaller number and **nothing in the rows says so**, so a fold that
"succeeded" proves nothing about its own start (docs/design/021 §7.1). `Overdrawn` is the one
negative proof the rows can give, and only a *recomputation* can give it -- which is the whole
reason a balance is recomputed per read rather than maintained forward on the observation's own
row. A running total has to be a netted number or a pair; netted, it clamps at nothing and reads as
an honest small balance, and as a pair it is a second answer to a question the rows already
answer.

**Two claims, two axes, and they used to share a name.** `Complete` was documented as "the start is
proven to be at or before the address's first use" while the only thing that produced it was a node
read agreeing with the fold at one block -- which proves the *total* there and says nothing about
the start. They are named apart now: `VerifiedOpening` is what the agreement proves, and
`CompleteHistory` is what the name claimed. A store may report `CompleteHistory` only from its own
row -- observation began at the chain's first block **and** something has actually been inspected
for it -- and the index checks the first half again independently
(`Completeness::without_a_birthday_proof`), so a store claiming more than its watch row supports is
answered rather than believed.

**One answer, both reads.** A balance and a history page project one address's rows two ways over
one observation row, so the store answers the range question once (`CanonicalPage::completeness`
beside `Balance::completeness`) and the index weakens each the same way. Assembling a page's claim
at read time instead is what made its gates decorative: a constant that both gates map to itself is
a value no gate can be observed to have touched, and deleting either left the suite green.

**Two values a page never carries**, and for one reason: each is evidence about a *total*, which a
list of transactions is not. `Overdrawn` is what the arithmetic proved, and no store puts it on a
page. `VerifiedOpening` is what an independent node read agreeing with the fold proves, and the
index narrows it to `Incomplete` on the page path (`Completeness::without_a_reported_amount`) --
a third narrowing beside the two gates, taken on `History::history` and never on
`Balances::balance`, where the agreement is about the very number being returned. A separate
two-valued enum for pages would say the same thing in the type, and it would split the one
vocabulary `apps/api` serves both reads from
(`openapi_contract::the_balance_and_the_history_page_state_completeness_the_same_way`), so the
boundary enforces it instead. Both stores are held to the result by
`contracts::HistoryContract` (`an_overdrawn_fold_does_not_reach_the_page_of_the_same_rows`,
`a_witnessed_total_never_reaches_the_page_of_the_same_rows`).

**And it needs the rows to be all there.** A fold is only as complete as what has been written, so
while any observation of an address is still reading its own past the index passes the store's answer
through `Completeness::while_backfilling` before returning it: every claim falls back to
`Incomplete`, and `Overdrawn` survives, because it is a proof rather than a claim and filling in
history only ever adds credits. The gate is on `Balances::balance` and on `History::history`, the two
reads every caller takes, so a store that learns to claim more cannot leak that claim through a
half-written range. **The fact it is taken against is the same
`WatchProgress::caught_up(checkpoint)` the pass's own selection uses**, read with the very checkpoint
the store's answer carried — so the latency the trade buys and the honesty that justifies it cannot
come apart, and the address-scoped answer is the conservative one: a token whose native sibling is
behind is reading rows that are still arriving.

An independent read of the same holding at the *same block the fold settled at* is that evidence.
On Ethereum `balanceOf` through `eth_call`, and `eth_getBalance`, pinned by `blockHash` with
`requireCanonical`, are deterministic EVM execution rather than a node's opinion: two honest nodes
at that block must return the same word. `chain_ethereum::BalanceWitness` (from
`WalletFactory::witness`) compares and reports an `Attestation` — `Agreed`, `Disagreed` with both
amounts, or `Unavailable`. It never changes the amount and it cannot fail a balance read; agreement
promotes `Incomplete` to `VerifiedOpening`, and only for that block. **It is not persisted, and
there is no column for it**: the witness reads a node and mutates a value on its way to a caller
rather than holding a store, so nothing in the persistence layer could ever write the attestation.
A `watch.balance_basis` column existed to hold it and was removed for exactly that reason -- a
column nothing can write is dead schema, and it costs nothing to add beside the writer that needs
it. Giving the witness a store is an ownership change (docs/design/034 steps 9-12), not a schema
one. Neither other
chain has an oracle to pin: Bitcoin Core keeps no address index, and Solana cannot read an account
at a past slot (docs/design/021 §7.4).

## Persistence ports

**Two are required, and together they are `Repository`.** They are the whole of what an index asks
of a store, so a third party writing its own index sink implements registration and committing and
nothing else (docs/design/030 §8 (ex-032)). `pub trait Repository: WatchStore + IndexStore {}` is blanket
implemented, so a type that implements both halves *is* one without saying so.

| Port | Operations | Meaning |
|---|---|---|
| `WatchStore` | `add`, `all`, `forget`, `reanchor` | the durable list of what this index observes, handed back with the block through which each observation has been inspected; `reanchor` moves an observation's birthday and discards its progress in one transaction, which is how a cancelled backfill settles without the address ever being unwatched |
| `IndexStore` | `checkpoint`, `commit` | checkpoint lookup, and the **one** atomic write: a batch of blocks, the per-observation progress they prove, and whatever the blocks do to the line |

**The rest are consumer capabilities**, each its own trait, so that needing none of them is a legal
store rather than a stubbed one. They are what *this* workspace's consumers read — `apps/api`'s
balance route, Bitcoin's change selection, the rescan tool — and an index neither calls nor requires
them.

Querying history is **not** one of them. A page has a length and a resume position, which are the
reader's own and not a store's, so a sink should not carry that model to be a sink: a repository that
chooses to be queryable exposes its own `ConfirmedHistory::history(HistoryQuery) -> CanonicalPage`,
and `ObservedHistory` joins it to one index's overlay for the consumers that need pending too
(`apps/api`'s history route, Solana's absence proofs).

| Capability | Operations | Meaning |
|---|---|---|
| `BalanceStore` | `balance` | the settled fold over the movements the store holds |
| `OutputStore` | `list` | current live outputs for an address |
| `Purging` | `purge_network` | the offline rescan, which is not indexing at all |

- Every port has a forwarding impl for `Arc<T>`, and none of them asks for `Clone`: a sink that owns
  a connection, a channel or anything else that cannot be duplicated reaches an index as
  `Arc<MySink>`.
- `WatchStore::all` returns `WatchProgress { watch, indexed_through }` from **one consistent
  snapshot**. A watch and how far it has been inspected are one fact about one observation, so a store
  that answered them separately could hand back progress earned under a registration it no longer
  holds, and the caller would have no way to tell. `indexed_through: None` means nothing has been
  inspected for it yet, so it resumes at its own birthday — a complete, durable answer, because a
  birthday is immutable and nothing is derived against a checkpoint that moves.
- A capability a store offers, it owes whole, including the one consistent read `IndexStore` states:
  the rows and the checkpoint they are narrowed by come from one transaction, or the pending overlay
  admits a block into both halves or neither (docs/design/030 §8 (ex-032) collision 5).

- `CanonicalBlock::new` validates one block's *facts* before storage is touched: unique transaction
  and output IDs, and movements, fees and credits of that network. It carries no precondition, which
  is what lets the pending overlay hold one and a read answer from it. **It is the only way facts
  reach a batch**: `IndexedBatch` takes `Vec<CanonicalBlock>`, so there is one validation path in
  both directions rather than a historical one that checked less.
- `IndexedBatch::new(network, expected_checkpoint, blocks, progress)` is the one write, and it
  derives what the write *does* from what it carries. Blocks ascend; one above
  `expected_checkpoint` must chain onto the line and becomes the new one; one at or below it is
  filling in a block already committed for an observation that was not watched at the time, and moves
  nothing. `IndexedBatch::checkpoint` is that answer, which is why there is no second port method and
  no flag. Each `Progress { watch, expected, indexed_through }` advances one observation, must stay at
  or below the line the batch leaves behind, and may not retreat from the value its own author read.
  The pass forms the pair at the write, not when a block is admitted.
- `IndexStore::commit` atomically: compare the expected checkpoint against its own, inside the write
  transaction, compare each claim's `expected` against what is held, write every block's rows
  **insert-if-absent, verify-if-present** (one `(address, height, transaction)` with two contents is
  corruption and terminal), apply the output changes folded across the whole batch, record the
  claims, and move the line to whatever the blocks made it. Every row it writes is idempotent, so a
  batch whose outcome was lost is simply redone.
- **The progress guard is an expected value, never a monotonic rule.** A batch redone under a smaller
  remaining budget can honestly settle *less* than the write it is redoing, so a store refusing "it
  went down" would wedge that observation for good. What is refused is a claim whose author was
  reading a different value — which is how an index finds out that a write it lost the outcome of
  actually landed. The refusal is a retryable `Conflict`, and it makes the index read its own watch
  list again rather than retrying the same claim forever (docs/design/030 §8 (ex-032), the fourth defect).
- Spends have two miss policies, and neither one fails the block. A *tracked* spend
  (`OutputChanges::tracked_spends`, an outpoint of no selected address) that matches nothing is
  silent. A *required* spend (`OutputChanges::spent`, an outpoint the interpreter attributed to a
  selected address) that matches nothing is returned by `IndexStore::commit` as an unmatched spend,
  naming the address whose history is provably short and the outpoint that proved it. Watching an
  already-funded address makes that the ordinary case, so it is a per-address completeness fact, not
  a verdict on the chain (docs/design/024 §5.1). No live output is written or removed for such a
  key, which is what keeps the block's record exact — but the miss itself is *kept*, so a later batch
  that discovers the output's creation cancels it rather than making a spent output selectable. A
  moved checkpoint is still a retryable `Conflict`; a row or an output re-presented with *different*
  contents is a terminal `Store` failure, because reading the block again cannot change what the file
  holds.
- A running store has no removal. Every block it receives is at or below its chain's confirmed
  line, so no row it holds is ever undone and it keeps no rollback data of any kind
  (docs/design/030 §8 (ex-025)). `Purging::purge_network` is not part of a pass: it empties one network for
  the offline rescan below and reports the row counts it removed as `PurgedRows`.
- Durable state per network: checkpoint, history copied under each affected address (no secondary
  index), block credits, live outputs. Nothing else: no confirmations, spent markers, events, raw
  blocks, filters, or rollback journal.
- A batch whose blocks are at or below the line is how an observation reads its own past, and it is
  the **same** `commit`: the affected observation is simply further behind. Insert-if-absent is what
  makes it work — a newly watched address routinely already has rows from every time it was somebody
  else's counterparty — and folding the output changes across the whole batch rather than block by
  block is what stops an output created and spent inside it from ever existing.
- `storage::Error::{conflict, corrupt_data, invalid_request}` carry adapter context; a lock held
  past the busy timeout is the `Conflict` a caller re-reads and retries.

## Synchronization

A pass is `Service`'s own bounded procedure over the state it already owns --- its
watch set, its overlay of blocks above the confirmed line, its source and
interpreter, and its store reference. Nothing hands it a selection and nothing
else holds those blocks, which is what makes `Indexing::sync` one call rather
than a coordination between owners (docs/design/030 §8 (ex-032) gap 3). `Supervisor` calls
that same `sync`: scheduling, retry delays, quarantine and joining on shutdown
are its, and it contains no second scan or commit. Driving `sync` by hand needs
no runner at all --- with the `supervisor` feature off the crate names no async
runtime.

Fresh network: no filters -> anchor at the observed tip; earliest birthday `B > 0` -> find the first
produced block at or after `B` and anchor at its real parent; `B == 0` -> start at genesis.

Each bounded pass:

1. read the source's two lines: its tip, and the newest block it calls irreversible
   (`ChainTip::confirmed_tip`). A confirmed tip above the tip is refused;
2. read the committed checkpoint and verify its `(position, hash)` is still canonical. If it is
   not, the chain reorganized *past* its own confirmed line: terminal `ConfirmedReorg`;
3. drop every held block the source no longer calls canonical — the whole of a reorg above the
   line, and no store I/O at all;
4. fetch produced blocks after the newest known position (bounded by `batch_size`), select the
   filters of the observations that are **caught up** with the line — `next_required <= position` —
   interpret, re-verify the hash, and hold each one above the line;
5. commit every held block at or below the confirmed line, oldest first, one `IndexStore::commit`
   each, carrying the claims that block proves: every caught-up observation whose next required
   block it is or passes, advanced to it. An empty block advances them exactly as a busy one does,
   which is what makes `indexed_through` mean *inspected* rather than *matched*. A block the store's
   own line already covers is dropped from the overlay without a write, because it is already
   written and no listener may hear about it twice;
6. **fill in what is owed**: spend whatever is left of `batch_size` inspecting blocks *at or below*
   the checkpoint for the observations that have not reached it, through the same `commit`.

Steps 4 and 6 differ in *scheduling* — forward first, and with the budget it needs, so a historical
range this node cannot serve can never delay the tip — and in nothing else: one procedure inspects a
block and validates its facts for both (`sdk/indexing/src/scan.rs`), and one owner runs the commit
lifecycle for both (`Service::apply`: the permit, the write, the line the batch leaves behind, the
claims the watch set takes over, the required spends the store could not account for, and the reload
an uncertain outcome needs). Holding a block above the line and notifying listeners belong to step 5
alone.

`SyncStatus::unknown_spends` carries every required spend the pass committed without a matching
indexed output, in block order. It is empty on a pass that accounted for everything; a non-empty
list means those addresses are short, not that the pass failed.

### Reading an observation's own past

`Start::At(p)` below the checkpoint used to index nothing at all, silently: a commit could only
extend the checkpoint, so there was nowhere to put the rows (docs/design/024, docs/design/030 step 12).
There is now one `commit` and one range vocabulary, and the only difference is which blocks a batch
carries.

An observation is behind exactly while `!caught_up(checkpoint)`, i.e. while
`next_required <= checkpoint.position`. One pass takes the lowest `next_required` across every behind
observation as its range start, the **checkpoint** as its range top — there is no per-watch anchor
any more, because a contiguous claim ends where the front of the chain begins — fetches up to its
remaining budget, interprets **each block once for every observation that owes it**, commits one
batch, and advances each one to the highest block it read at or above that observation's own resume
point. Ten observations therefore cost what one does. The `docs/design/030 §8 (ex-032)` X/Y/Z table is that
behaviour written as one test (`tests/backfill_contract.rs`,
`one_pass_serves_three_observations_from_their_own_positions`).

- **One reading, and it must be one.** A historical read is held to exactly what a block arriving at
  the tip is held to, through the same inspection procedure (`sdk/indexing/src/scan.rs`): each block
  follows the one before it by height and parent linkage, and the first block an observation owed
  follows the very block already recorded for it. Both ends matter, because the claim this read
  writes says every block from where that observation resumed through the highest one read *was
  inspected for it*, and nothing downstream ever re-derives that — a produced block a source left out
  of the range would be history the address loses permanently and silently. `IndexedBatch::new`
  cannot catch it: it checks that a claim names a block the batch carries, which says nothing about
  where the observation resumed from. A source that skips one is refused, retryably, and settles
  nothing (`tests/backfill_contract.rs`, `a_historical_range_missing_a_produced_block_settles_nothing`
  and `a_resumed_historical_read_must_follow_what_the_observation_recorded`). The one check the two
  directions do *not* share is canonicality: only blocks above the confirmed line can still be taken
  back.
- **Bounded, two ways.** The forward half spends `batch_size` first, so a network behind its tip
  reads no history at all. And `max_backfill_blocks` refuses a start too far down at `watch`, which
  bounds the range when it is *chosen*. The top now follows the chain rather than being fixed at
  registration — an observation must reach the present before it joins the front — which is the cost
  side of the trade `caught_up` names.
- **Restart.** Progress is written in the same store transaction as the rows that prove it, so it can
  neither claim a batch that was not written nor lose one that was. A restart resumes after
  `indexed_through` and never re-reads a settled prefix; an observation that has reached the line is
  recorded as such and is never swept again. A batch whose outcome is unknown is simply redone,
  because every write in it is idempotent — and if the lost write actually landed, the store refuses
  the redo's claim and the index reloads the value it holds.
- **What an operator sees, and what it does *not* gate.** `SyncStatus::backfill` is `Some` exactly
  while something is owed, carrying how many addresses are pending, the range still to read (`resume`
  from the lowest `next_required`, `anchor` from the checkpoint every behind observation is reading
  towards), how many blocks this pass inspected, and any failure that stopped it short. It rides onto
  `NetworkHealth::backfill` and out to `GET /v1/status` as `networks[].backfill`, so the owed
  range is a thing an operator reads rather than infers.

  It does **not** move `SyncPhase`, and this is the availability floor of `docs/design/024` §7
  answered. An owed range can be one a pruned node will never serve, so a phase that waited for it
  would wait for the life of the process: one such watch held `GET /health/ready` at 503 for every
  chain, and — because configured wallets are imported before the supervisor starts — stopped the
  HTTP listener binding at all. Readiness therefore follows **forward** indexing, which is what
  every client reads; what the gap weakens is each read that touches it, and `Completeness` already
  says so there.
- **A backfill failure never fails the pass, and never changes the network's state.** A node that
  cannot serve a historical range is a property of the request, not of the chain, so a source
  error, a moved checkpoint or a lost admission race leave the forward progress committed and come
  back as `backfill.failure`. Only a terminal `Store` error — the store disagreeing with itself —
  stops the network. The supervisor still *counts* the failure: it waits out the same capped,
  jittered backoff a failed pass gets, so an unservable range costs a bounded trickle of calls
  rather than a spin, and `NetworkHealth::attempts` keeps a repeat distinguishable from a first.
- **An existing store.** A file written before per-observation progress holds a per-address
  `(start, resume, anchor)` triple instead, and that triple could describe a *gap* between what a
  historical read had covered and what the forward pass had — a shape one contiguous reference cannot
  express and deliberately no longer does. The two representations disagree about what is proven and
  the old one carried no block hash to build the new one out of, so the schema step **drops** the
  table rather than relabelling it, and every observation re-derives its range from its own birthday
  (docs/design/030 §8 (ex-032) §2). A deployment that would rather not pay that read has the offline `--rescan`
  and a raised `Start` instead.
- **The output a spend above the reading edge consumed.** A block inspected once for an address while
  another observation of it was still catching up may have spent an output whose creation is only
  discovered later. A store that filled that creation in would hold a live output nothing can ever
  spend, and coin selection would offer it for good. What closes it is not a wider read: the store
  **keeps** every required spend it could not match, and a later create is cancelled against that
  record instead of being written — the same cancellation `IndexedBatch::changes` already makes
  inside one batch, across the boundary one batch cannot see. `testing::SpendContract` pins both
  directions, because the other order needs no record at all: a batch that fills the creation in
  first leaves the output live for the next block to spend normally.

Restart resumes at the checked successor of the checkpoint position after verifying its hash. The
blocks above the line are not durable, so a restart rebuilds them — which is simply the ordinary
catch-up pass over `[line + 1, tip]`, bounded by `batch_size`.

A new address's birthday is the checkpoint's successor, and the checkpoint is now the confirmed
line, so the birthday covers blocks already held above it. Those blocks are therefore inspected
again against the current selection before any of them commits; only an address insertion reaches
that path, and only the blocks the fetching path's bounded native-block cache no longer holds cost
a refetch. That cache belongs to fetching, not to the overlay: the blocks above the line are this
crate's own interpreted facts, and the chain's raw blocks behind them are a discardable head start
on refetching.

## Failure isolation

One chain's failure is only its own. The supervisor drives one task per `SyncUnit`, so a network
that fails does not stop another network's passes, hold its cadence, or end the
process.

| Pass result | Network state | Next pass |
|---|---|---|
| below the observed tip | `CatchingUp` | at once |
| at the observed tip | `Ready` | after that network's own poll interval |
| retryable `IndexError` | `Retrying`, `attempts += 1` | `interval x 2^(attempts-1)`, capped and jittered |
| `Conflict` | `Retrying` | at `interval`: a lost race is not an outage |
| non-retryable `IndexError`, or a panicked unit task | `Quarantined` | never, until `SyncControl::resume` (`POST /v1/status/resume`) |

`InvalidBlock`, `NetworkMismatch`, `InvalidRequest`, `ConfirmedReorg`, a terminal `Store` failure
and a panic are deterministic, so they are never auto-retried: a restart re-fetches the same block
and fails identically. A quarantined network keeps its last checkpoint, keeps answering reads from
it, and reports the reason on `GET /v1/status`. Once the cause is fixed out of band,
`POST /v1/status/resume` with that one network clears it in place -- no restart, no other chain
touched -- and the next pass either succeeds or fails again the ordinary way.

`ConfirmedReorg` means the chain took back a block that was committed as irreversible. It is
impossible on Ethereum and Solana, whose lines are consensus guarantees; on Bitcoin, which has
none, it means a reorg deeper than the configured `confirmation_depth`. It has no in-process
remedy at all -- a resume re-quarantines within one pass, correctly -- and the
offline rescan below is the only way back.

A network never auto-quarantines on attempt count: a node down for an hour heals by itself, and
`attempts` and `since` are what an operator alerts on. Readiness is
`all(Ready | Quarantined) && any(Ready)`, so a quarantined chain does not hold `/health/ready` at
503 forever, and every network quarantined answers 503 without exiting.

### The offline rescan

```
payment-server <config.json> --rescan <chain>/<network>
```

It purges exactly that network's indexing-owned rows through `Purging::purge_network`:
checkpoint, history, movements, block credits, unmatched spends, and live outputs, in one atomic
removal. It touches no other network and never the watch list, so every observation and its birthday
survive; the progress recorded for them does not, because that claim is about rows this purge just
removed. It prints what it is about to do,
then the row counts it removed, and exits non-zero on failure.

It is deliberately a subcommand and not an HTTP route. The process reads the configuration,
opens storage, deletes, and exits: no chain connects, no supervisor starts, and the listener is
never bound. Purging while the API served `GET /v1/wallets/{id}/transactions` from that network
would hand clients an empty history that looks authoritative, and the checkpoint-bound cursors
would conflict mid-page.

The next ordinary start finds `checkpoint.is_none()` and re-indexes the network from
`SyncPlan::earliest_position()` -- the earliest registered address start position -- exactly as a
fresh deployment would, so the history is rebuilt rather than merged.

It refuses, before deleting anything: an argument that is not `<chain>/<network>`, and a network
the configuration file does not configure (the refusal names the ones it does). Both refusals
happen before storage is opened.

## History and pagination

- Canonical storage records inclusion (or failure) at a `BlockRef`. `Confirmed` is a fact rather
  than a threshold: a transaction is confirmed when the durable store holds it, which it only does
  for blocks at or below the chain's confirmed line, so it will not revert. A transaction in a
  canonical block *above* the line is `Pending` and is reported in the page's `pending` list, which
  the first page carries and later pages leave empty. The `confirmations` number counts from the
  newest block the index knows of, so it keeps meaning "how deep is this" on both sides of the
  line.
- `Balances::balance` answers both halves of an operator's question — what has settled, and what is arriving and
  what is leaving above the line, reported apart and never netted, because `Decimal` has no
  subtraction and because in and out are two different facts. Network fees are not folded in; a
  pending transaction's fee is reported with it in history.
- A history page resumes after a `HistoryPosition` and nothing else -- there is no cursor type
  wrapping it. Confirmed history is append-only, so a checkpoint that moved can only have appended
  *above* that position: an ascending keyset page is stable however long a caller holds it, and
  there is no conflict to raise. An output cursor still binds to its checkpoint, because a spend
  removes a row -- and it binds to the pending overlay as well, because a block held *above* the
  line spends rows too without moving the checkpoint. Its opaque position carries the overlay
  revision the page was folded over, and a later page whose overlay has moved is the same retryable
  `Conflict` a moved checkpoint raises. Nothing is pinned and replayed: the sample is not
  reconstructible once a reorg has discarded it, and a paginated read of the spendable set is coin
  selection, which must be refused rather than answered from a set half of which is stale. A read
  that fits one page issues no cursor and can never see it.
- `OutputStore::list` on the index is also **refused outright** while the queried address's own
  history has not been inspected up to the checkpoint the page would be stamped with, with the same
  retryable `Conflict`. A balance and a history page can be weakened instead, because they carry
  `Completeness`; a spendable set cannot, because its consumer turns a row into a transaction input
  and an unspent row is only unspent as far as the index has read. The refusal clears as the
  backfill half of each pass reaches the line; Bitcoin's `Outbox::prepare` maps it to a retryable
  `Unavailable` before a send forms.
- History is stored once per address (every native and token fact it touches) and queried per
  asset: `HistoryQuery { address, asset, after, limit }`, whose rustdoc is the one contract. A
  page holds the transactions that move `asset` for the address, plus those whose fee the address
  paid when they failed or `asset` is the fee asset; each carries only its `asset` movements and
  its native fee. The repository evaluates this where the data lives (SQLite: one keyset page per
  chunk, its movements fetched with it), so `limit` counts matching transactions, every page but the last is full, and `next` is
  `None` exactly when nothing else matches. `after` is a position in the address's history and
  resumes any asset of that address. Every store runs the shared scenarios in
  `sdk/indexing/tests/contracts/`: `WatchContract` and the commit contracts are the minimum every
  store passes, and `HistoryContract`, `BalanceContract`, `SpendContract` and `PurgeContract`
  state what one owes once it offers that capability.
- A network fee is always the chain's native asset; `ObservationDraft` validation rejects any
  other before storage.
- Solana submission reconciliation reads the same network's `Checkpoint` and its `History` reader
  (passed in, so the chain names no store) plus a
  checkpoint-advance notification. Indexing stores nothing about submissions.

## Storage

One embedded SQLite file holds every network: checkpoints, history, movements, live outputs,
per-observation progress, and the watch list, every table framed by `(chain, network)` as its
leading key columns, created on first open and needing no external service.
`indexing::sqlite::Database::open` opens it once and brings its schema to the version this build
expects; `indexing::sqlite::Repository::new(database)` speaks the domain over it and cannot fail.
**One repository serves every network.** Each operation names the network it is about, so isolation
is the row key rather than a bound handle, and a read of a network holding no rows answers empty
rather than refusing. That one value is the `WatchStore` and `IndexStore` an index requires plus
every consumer capability this workspace reads — `BalanceStore`, `OutputStore` and `Purging` — plus
its own `ConfirmedHistory` query. `apps/server/src/storage.rs` is the whole composition: `"storage"` in
`docs/API.md` takes `{"path": ...}` and nothing else.

There is no seam for a *shared-database* backend. The one capability one would add is
multi-writer deployment, which `docs/SYSTEM_REQUIREMENTS.md` places under *Out of scope (MUST NOT
exist)*; nothing else in the system is multi-instance-aware either, and a single-writer file is
what that requirement calls for (`docs/design/030 §8 (ex-027)`).

**Two backends ship, and a build may take either, both or neither.**
`indexing::memory::Repository`, behind the `memory` feature, is the same ports on `BTreeMap`s:
it needs no file, no C library and no async runtime, and it holds every network exactly as one
file does. It is for a process that indexes for as long as it runs and wants nothing on disk --
an embedding with no writable path, a short-lived worker, a demonstration, and any test that
would otherwise open a file -- and it is not durable: a restart re-indexes from each
observation's birthday. Every shared contract scenario runs against it beside the file, and that
pairing is the proof the ports are implementable more than one way.
`sdk/indexing/tests/minimum_contract.rs` holds the other end: a non-`Clone` sink implementing the
two required ports and no read method at all observes, commits and restarts, through the `Arc`
forwarding impls alone.

### The stored layout

The layout is **not** a compatibility contract, and `docs/SYSTEM_REQUIREMENTS.md` says why it
stopped being one: the redb store it used to describe is gone (`docs/design/030 §8 (ex-031)`), no deployment
exists, and nothing reads any earlier format. A file states what it is and how far it has come:

| Setting | What |
|---|---|
| `PRAGMA application_id = 0x77335142` | the file belongs to this application; any other value is refused rather than migrated, and a file that is both unstamped and unmigrated is claimed |
| `PRAGMA user_version` | how many ordered schema steps have been applied; a file ahead of this build is refused |
| `PRAGMA journal_mode = WAL` | one writer alongside many readers, which is what lets an HTTP read run while a chain commits |
| `PRAGMA synchronous = FULL`, `fullfsync = ON` | a commit that returned has reached the disk, on macOS too |
| `PRAGMA foreign_keys = ON` | a movement row cannot outlive the history row it belongs to |

Each step in `storage/sqlite/steps.rs` runs exactly once, in one transaction with the
`user_version` bump that records it, and a later step may alter what an earlier one wrote. That is
the whole migration story; there is no frozen encoding, no dead tag to route around, and no
one-release export tool. Today there is exactly **one** step: `docs/SYSTEM_REQUIREMENTS.md` revoked
the durable-format exception in writing, so a build that created an obsolete shape only to copy it
into the current one would be migrating a file nothing ever wrote.

Six tables:

| Table | One row is |
|---|---|
| `watch` | one observation: what to observe, and how far it has been inspected |
| `network_checkpoint` | one network's confirmed line |
| `transaction_fact` | one transaction's block reference, native place in that block, status and fee, stored **once** rather than once per participating address |
| `transaction_address` | one address's participation in one transaction -- required, because a failed transaction has no movements and a fee payer may have none either; it repeats the height and the native place because the page walks this table alone |
| `balance_movement` | one balance change, for one address, in one asset, with `direction` separate from `kind`; a transactionless credit is the same row with no transaction to name it |
| `output` | one outpoint's whole life, creation and spending as columns -- so discovering a creation later can never erase a spending already recorded |

Every one is `WITHOUT ROWID`: each is addressed by its whole primary key, and those keys are the
orders the read ports page in, so an ascending keyset page is an index walk rather than a sort. A
history page is ordered by `(block_height, transaction_order, transaction_id)`, every component of
which is a function of the block alone, which is what keeps a held cursor append-only however much
more of an already-stored block a later watch causes to be written. `transaction_order` is the
transaction's **native** place in its block, and it is written rather than derived: a store is
handed the transactions that touched one selected address and never the block's whole list, so any
rank it computed would move under a second, wider selection. Each interpreter supplies it out of
the complete block-ordered list it holds while it filters -- Ethereum's `transactionIndex`, the
position in Bitcoin's `tx` array and in Solana's `transactions` array -- and it travels on
`ObservationDraft` and `CanonicalTransaction` to the column. Amounts are canonical base-10 **text**, because they are u256-capable and
SQLite's only integer is a signed 64-bit one; the arithmetic stays in Rust (`Ledger`) on every
backend. A settled total is that arithmetic over `balance_movement`, recomputed per read and kept
nowhere else: `balance_movement`'s leading key is `(chain, network, address, asset, ...)`, so the
fold is an index range scan over exactly the rows it sums, and one answer cannot drift from
another. It also keeps credits and debits apart to the very end, because a deficit is a fact to
report and not a number to clamp.

### How a commit is atomic

One process owns the file. SQLite is synchronous, so every access is one
`tokio::task::spawn_blocking` call carrying **one whole operation**, which keeps file I/O off the
executor; the operation borrows a connection from a bounded pool for its length, so a reader runs
alongside the one writer rather than behind it, which is the whole of what the write-ahead log
promises.

A commit is one `BEGIN IMMEDIATE` transaction. `IMMEDIATE` rather than the default deferred begin,
because the commit reads the state it is about to condition itself on: an immediate transaction
holds the file's single write lock from its first statement, so the checkpoint read and the writes
it authorizes cannot be separated by another writer. There is therefore **no compare-and-set**, no
stored version and no condition vocabulary -- the window a condition would guard does not exist.
`docs/design/030 §8 (ex-031)` section 5.2 proposed `UPDATE checkpoint SET ... WHERE position = ? AND hash = ?`
instead; that is unsound and the proposal is struck, because `(position, hash)` is the chain's own
coordinate and a rescan restores it byte for byte, so the condition matches again after exactly the
sequence it was introduced to refuse. `blocks_test::the_proposed_checkpoint_condition_cannot_see_an_aba`
is that sequence, written out.

The same six invariants still hold, and the schema itself states four of them; the other two are
the engine answering inside the write transaction rather than a condition a caller remembered:

| Where it is stated | Invariant |
|---|---|
| unique index on `watch (id)` | an identity is claimed once; a second `add` naming it is refused by the engine |
| primary key `(chain, network, asset, address)` | one watch row per address of an asset in a network |
| the checkpoint read inside the write transaction | the block being committed follows the checkpoint that is actually stored |
| primary key on `transaction_fact` | one transaction has one set of facts, and a re-presented row is compared against it rather than rewritten |
| primary key on `output` | one outpoint has one row, so a creation discovered later fills that row in rather than adding a second |
| the row count the spending `UPDATE` reports | the output marked spent is the exact row that was unspent |

A refused write is a `Conflict` and the whole transaction rolls back, so no table is left holding a
row of a block that did not land. A lock another connection holds past the busy timeout is the same
`Conflict`, retryable, which is the retry semantics callers already implement.

### The offline rescan

`Purging::purge_network` removes one network's checkpoint, transaction facts, memberships, balance
movements and outputs in one transaction and reports the counts as `PurgedRows`. It is one
`DELETE ... WHERE` per table, so the rows never exist as Rust values and a row this build cannot
decode is still removable. The `watch` rows survive and everything they *claimed* does not: the
progress, the running totals and the basis are all reset in the same transaction, because what a
watch observes is application state while all three of those are statements about rows this purge
just removed -- and no `WatchStore` method could reset them afterwards, since `add` preserves
progress on an exact repeat by contract. `payment-server <config.json> --rescan
<chain>/<network>` is the tool: it validates the argument and the configured network *before*
opening storage, connects no chain, starts no supervisor, and binds no listener.
