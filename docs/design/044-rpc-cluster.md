# 044: Choosing RPC nodes — one interface, one node or many

> "we need to consider 3 things, block height and failures and capabilities
> possibly rate limits … each node should be probed on block height, higher
> wins … transaction should be broadcasted to all nodes … if high failures
> on primary, secondary should be used if not too behind … if rate limits
> are approached we should be able to switch … we can initialize two rpc
> nodes and compose them to act as one or just use one, either way both
> will work as argument."

Status: **phase A implemented (cluster, broadcast, chain send paths); phase B (composition, config, healthz) pending.** One new type in `packages/json-rpc`: `Cluster`, a
managed composite of 1..=N clients that implements the same `Client`
interface as a single node. Reads go to a probe-selected primary; writes
are broadcast to every node; three signals — tip height, failure counters,
rate-limit classifications — drive primary selection. One node is a
`Cluster` of one; every constructor that accepts a client accepts either.

---

## 0. What already exists (and is kept)

- **`Client`** — the interface: `request`, `batch`. This is the argument
  type every chain client is built over.
- **`Failover<C: Client>`** — ordered per-call walk; advances only on a
  retryable transport failure; a `Repetition::Forbidden` call reaches the
  first client only. Kept as the dumb primitive: static order, no
  probing, no policy. `Cluster` is the managed composite; the two do not
  merge (`039`'s rule — two tools, two jobs).
- **`Statistics` / `Snapshot`** — per-node attempts, failures,
  consecutive failures, and a sanitized `last_failure` code. Already the
  healthz feed. No new counters are needed; the rate-limit signal rides
  the existing code.
- **`Endpoint` / `Http`** — connection construction, unchanged.

## 1. The shape

```rust
pub struct Cluster<C> { /* first + rest, like Failover: 1..=N in the type */ }

impl<C: Client> Client for Cluster<C> {
    // reads and ordinary calls: unicast to the current primary
}

impl<C: Client> Cluster<C> {
    pub fn new(clients: Vec<C>, probe: Probe<C>, policy: Policy) -> Result<Self, Error>;
    pub fn one(client: C) -> Self;                       // the single-node case
    pub async fn broadcast(&self, request: Request) -> Result<CallResult, Error>;
}
```

- `Cluster` implements `Client`. Anything built to take one node takes a
  cluster — a chain client's transport slot, a test double, anything.
- `Probe<C>` is the chain's tip reader, injected: a closure
  `|&C| -> BoxFuture<Result<u64, Error>>`. Chain-neutral here; each chain
  passes its own height call. Probe results are the same unit the chain
  reports (blocks, slots), so lag tolerance is comparable units.
- `Policy` names the tunables: `max_lag`, `failure_threshold`,
  `rate_limit_codes`, `refresh`. Nothing else in the type is tunable.

## 2. Selection — the three signals

- **Tip height decides rank.** At construction and on every reconsider,
  all nodes are probed; the highest tip is primary. Ties keep the
  incumbent to avoid flapping.
- **Failures trigger reconsideration.** When the primary's
  `consecutive_failures` reaches `failure_threshold`, the cluster
  re-probes. The best secondary within `max_lag` of the highest tip takes
  over; a secondary too far behind never wins. The demoted primary stays
  in the pool and can win again on a later reconsider.
- **Rate limits trigger the same path.** `Snapshot.last_failure` carries
  the sanitized classification (for example `http 429`). A code in
  `rate_limit_codes` counts as a reconsideration trigger on its own —
  the cluster sheds load to a healthy in-lag peer before the limit
  becomes failures.
- **Periodic refresh.** `refresh` re-probes on an interval so slow tip
  divergence is caught even without failures.

Reconsideration is one code path fed by three triggers. Switching is
never per-call guesswork: the primary is a role, not a preference.

## 3. Broadcast — writes to all nodes

- `Client::request` is unicast to the current primary. `Cluster::broadcast`
  is the deliberate multi-node path: the request reaches **every** node
  exactly once, concurrently; the call succeeds if any node succeeds, and
  fails only if all fail (the last failure is the error).
- **The once-rule is not broken, it is chosen.** `Request::once` exists so
  no decorator can *repeat* an envelope a node may have accepted.
  Broadcasting is not repeating: each node receives one execution of a
  deliberately shared envelope (the same signed bytes — one txid
  everywhere), and only the chain's submission path calls `broadcast`.
  Reads and all other calls stay unicast. The distinction stays visible in
  the API: a broadcast is a decision at the call site, not a transport
  behavior.
- Chains name their broadcast methods themselves — `sendrawtransaction`,
  `eth_sendRawTransaction`, `sendTransaction` — in their sender, beside
  the one endpoint that signs. `json-rpc` never learns method semantics.

## 4. Healthz and secrecy

- The cluster exposes the current primary as an **opaque node id** (its
  configured alias or index) — never a URL, header, or credential.
  Per-node `Statistics` remain readable per node for the
  attempts/failures/last-failure body the requirements already specify.
- No request content, remote prose, or endpoint material crosses the
  cluster's public surface. `Debug` shows node count and primary id only,
  like `Failover` today.

## 5. Composition

- Each chain verifies every configured candidate at startup (the existing
  network-identity check, unchanged) and builds `Cluster::new(verified,
  tip_probe, policy)`. One configured node is `Cluster::one` — same type,
  same argument, no branching anywhere above.
- `Policy` values come from config with defaults: `max_lag` small (the
  units are the chain's tip unit), `failure_threshold` around 3
  consecutive, `rate_limit_codes` seeded with the common transport
  classifications, `refresh` on the order of minutes.
- `Failover` remains for callers that want static ordered fallback with
  no probing — its job is unchanged.

## 6. Work order

1. **`cluster.rs`.** The type, `Client` impl, `broadcast`, selection,
   policy. Unit tests with scripted clients: controllable tips, failure
   sequences, injected codes.
2. **The six behavioral tests of §7.**
3. **Chain wiring (one chain first).** Bitcoin: verified candidates into a
   cluster, submission method to `broadcast`. Then ethereum, then solana —
   each names only its own tip probe and its own send method.
4. **Config.** Node lists with per-node aliases (for the opaque id); policy
   defaults; `max_lag` overridable per network.
5. **healthz.** Primary id + per-node statistics from the cluster; the
   secrecy rules of §4 pinned by test.

## 7. Verification

The eight gates from `AGENTS.md`, plus the Linter MCP `check`, and six
behavioral tests:

- **Rank.** Higher tip wins at construction; a one-node cluster is a valid
  `Client` and answers normally.
- **Same argument.** A function generic over `C: Client` accepts both a
  bare client and a cluster without modification — compile-time proof.
- **Failure switch.** Primary failing `failure_threshold` times
  consecutively moves primaries to the in-lag secondary; a secondary
  behind `max_lag` never wins; the demoted node can win again after
  recovery.
- **Rate-limit switch.** A classified limit code on the primary triggers
  reconsideration without waiting for the failure threshold.
- **Broadcast.** All nodes receive exactly one execution; one success
  answers the call; total failure reports the last failure; a forbidden
  request is never re-executed at any node.
- **No flap.** Tied probes keep the incumbent; switches happen only on
  triggers, and refreshes bound their frequency.

## 8. Out of scope

- **No chain knowledge in `json-rpc`.** Tips, send methods, and finality
  stay with the chains; the cluster is selection and fanout only.
- **No automatic repetition of forbidden work.** Outside deliberate
  `broadcast`, a `Request::once` reaches one node, as today.
- **No quorum or result-comparison.** Broadcast takes first success; the
  chains' exact-bytes guarantee makes results agree, and comparison is not
  this layer's question.
- **No removal of `Failover`.** Two primitives, two contracts.
