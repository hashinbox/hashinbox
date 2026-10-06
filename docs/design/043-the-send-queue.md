# 043: The send queue — a `Sender` decorator with bounded delivery

> "make simple queue that will allow us to broadcast transactions … wrap
> Sender and send() with some queuer … just simply enqueue transactions and
> ensure delivery. Problem is that we cannot send for example 1000
> transactions to rpc at single moment."

Status: **implemented.** Deviation: the queue wraps `Authorship`, so both
batch and single sends wait in it, not only `Sender`; and its pause between
retries is injected (`Pause`), so `wallets` names no runtime.

Proposal as written: one new type in `sdk/wallets`: `Queue`, a transparent
decorator over any `Sender`. It admits batches, dispatches them FIFO with
bounded in-flight concurrency, retries only what is honestly retryable, and
never drops an admitted batch while the process lives. No chain crate
changes. No spawned runtime. No durable log.

---

## 0. The shape

```rust
impl<S: Sender> Sender for Queue<S> { … }
```

`Queue` implements the exact trait it wraps. `Wallets::send_all` routes to
the per-asset sender exactly as today; if the composition wrapped that sender
in a `Queue`, every send is queued — and no caller can tell. The batch
contract is unchanged:

- `send(transfers)` resolves to the accepted transaction ids, or the same
  `SendError`, only later.
- Ordering, accepted-prefix reporting, ambiguity reporting — all still the
  inner sender's, untouched.

Construction: `Queue::new(inner, policy)` where `policy` names the in-flight
bound, the queue capacity, and the retry budget. Composition wraps each
asset's sender before it enters the factory profile, or wraps the sender the
profile already holds.

## 1. Semantics

- **Admission.** `send()` enqueues the batch and resolves when the batch has
  been dispatched *and* the inner sender acknowledged it. The future is the
  backpressure: if the queue is full, the future waits for capacity. No
  fire-and-forget, no second API.
- **Dispatch.** FIFO. At most `max_in_flight` batches are with the inner
  sender at any moment — that is the answer to "not 1000 at once." A batch
  completes (acknowledged or finally failed) before its slot frees.
- **Retry.** Only a whole-batch failure that is all of: retryable
  (`WalletErrorKind` says so), with an empty `accepted` prefix, and with no
  `ambiguous_transaction_id`. Such a failure goes to the back of the queue
  with jittered backoff, bounded attempts, then resolves the error. Anything
  that accepted something, or went ambiguous, resolves immediately — the
  accepted prefix and the chain's own ambiguity recovery own those outcomes,
  and retrying either risks a double-send.
- **Shutdown.** `close()` refuses new admissions with a typed error and lets
  the queue drain: admitted batches dispatch to completion (bounded by the
  caller's deadline). In-memory only — an unadmitted batch never existed, an
  admitted one is never silently lost while the process runs.

## 2. No spawned task

The queue is self-pumping. Enqueue attempts to dispatch inline; each
completion pumps the next waiter. This keeps `wallets` executor-free — the
crate names no runtime today and this adds none — and makes the queue a pure
value: shared state, two public methods, no background anything. Tests drive
it with `futures::executor` alone.

## 3. Policy

```rust
pub struct Policy {
    pub max_in_flight: NonZeroUsize,   // default: small, e.g. 4
    pub capacity: usize,               // default: e.g. 256 admitted max
    pub attempts: NonZeroU32,          // retry budget, default e.g. 3
    pub backoff: Backoff,              // jittered, capped doubling
}
```

Shared nodes: one `Limit` (the in-flight semaphore) can be handed to every
queue whose asset shares a node, so per-asset queues still cap per-node
pressure. The policy owns everything tunable; the type owns everything
invariant.

## 4. What the queue deliberately is not

- **Not a durable job log.** In-memory admission control, not a persistence
  mechanism. Durability of sends is the outbox's and the submissions
  drain's business, both of which exist.
- **Not a reorderer.** FIFO, always. Per-source ordering under concurrency
  stays with the inner senders, whose reservation and nonce-floor machinery
  already owns it.
- **Not a chain concern.** No chain crate changes. Protocol semantics —
  fees, nonces, UTXO reservation, ambiguity recovery — stay where they are;
  this is admission control one layer up, where chain-neutral is honest.

## 5. Where it lands

- `sdk/wallets/src/queue.rs` — `Queue`, `Policy`, the internal state.
  Re-export `Queue` from the crate root beside `Sender`.
- Composition wraps per-asset senders; the apps/server wiring is one line per
  asset profile (and one shared `Limit` per network where assets share a
  node).
- Observability: three atomics — depth, in-flight, completed — readable for
  healthz without any hook into the dispatch path.

## 6. Work order

1. **`queue.rs`.** The decorator, policy, self-pumping state. Unit tests with
   a scripted fake `Sender`: slow, failing, ambiguous — every branch of §1.
2. **Re-export and doc.** Crate root, README capability row.
3. **Composition wiring.** apps/server wraps each profile's sender; healthz
   gains queue depth/in-flight from the atomics.
4. **Behavioral proofs.** The six tests of §7.

## 7. Verification

The eight gates from `AGENTS.md`, plus the Linter MCP `check`, and six
behavioral tests that pin the contract:

- **Bound.** With `max_in_flight = 2` and a slow fake, five admitted batches
  never put more than two with the sender at once.
- **FIFO.** Dispatch order equals admission order.
- **Retry.** A retryable whole-batch failure re-enters the queue, succeeds on
  the next attempt, and resolves with its ids.
- **No retry on partial.** A failure with a non-empty `accepted` prefix
  resolves immediately with that prefix — nothing is resent.
- **No retry on ambiguous.** An ambiguous failure resolves immediately; the
  chain's recovery remains the only owner of that outcome.
- **Shutdown.** `close()` refuses new admissions and drains admitted ones;
  a batch admitted before close always resolves.

## 8. Out of scope

- **No persistence.** A durable queue is a different product; revisit only
  with a stated requirement.
- **No priority lanes, no per-wallet fairness.** FIFO is the feature.
- **No changes to `SendError`, `Sender`, or any chain sender.** The decorator
  works because the contract already says everything it needs.
