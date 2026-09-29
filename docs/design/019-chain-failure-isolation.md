# 019: Chain failure isolation — one chain's failure must not stop the others

> "this is clear, goal is to produce callback to which we can react; it should
> definitely not stop other chains."

> **Status (superseded in part by `025`).** The terminal class `ReorgTooDeep`
> no longer exists: the rollback journal it named is deleted, and nothing the
> store holds is ever undone. Its place in the classification table (§5.1,
> §5.3) is taken by `ConfirmedReorg`, with the same posture — non-retryable,
> quarantines that one network, repaired only by §5.3's `--rescan` — but a
> different trigger: the chain reorganized *past* a block committed as
> irreversible. That is impossible on Ethereum and Solana, whose confirmed
> lines are consensus guarantees, so it is Bitcoin-only and means a reorg
> deeper than the configured `confirmation_depth`. The count of terminal
> classes is unchanged. §5.3's `purge_network` survives and gets simpler: with
> no journal there are four tables to purge, not six. §6.3's blanket refusal
> was already superseded by `024`.
>
> **§5.3 is now landed** (see its own note below). §6.3 is the only part of
> this document still unimplemented, and `024` owns what replaces it.

Today one chain's failure stops indexing for every chain, and the HTTP API keeps
serving history that silently ages. This document specifies the isolation unit,
the callback contract, the per-scope health model, the recovery paths, and the
migration. It also covers an adjacent silent-data-loss defect found on the same
trace (§6: a birthday in the past indexes nothing).

> **Implementation status.** Sections 2-4 are implemented on
> `refactor/chain-unification`: per-unit supervision, the `SyncMonitor` /
> `SyncControl` pair, the `NetworkState` / `NetworkFailure` / `NetworkHealth` /
> `SyncHealth` vocabulary in `indexing`, the readiness rule, per-chain cadence
> with backoff and quarantine, and the `state` / `failure` fields on
> `GET /v1/status`. Section 5.3 (`BlockStore::purge_network` and the offline
> `--rescan` subcommand) and section 6 (refusing a birthday below the
> checkpoint) were queued for a separate decision;
> `docs/design/017-remaining-work.md` section 5 records both, and 5.3 has since
> landed (next paragraph). Where the text below names section 6 as the
> remedy, read it as pending. Two deliberate
> divergences: `run` became `Supervisor::run` with a caller-side builder (as
> `a retired design note (022)` section 4.1 predicted), and the status route keeps reading
> each checkpoint from the index rather than from the snapshot, because the
> monitor is deliberately silent during catch-up.
>
> **Section 5.3 has since landed**, as specified: `BlockStore::purge_network`
> on every backend (a third method on the existing trait, inside
> `linter.toml`'s cap of three, so no new trait was needed) returning
> `PurgedRows` so the tool can print per-collection counts, and
> `payment-server <config.json> --rescan <chain>/<network>` in
> `apps/server/src/rescan.rs`. Two additions to the shape below: the port
> returns the counts rather than `()`, because the subcommand has to report
> what it removed; and the subcommand refuses a malformed argument and a
> network the configuration file does not configure, before storage is
> opened. Section 6 is still not implemented and, per `024`, will not be
> implemented as written.

## 1. What actually happens today

One loop drives the whole `Composer`:

```rust
// apps/server/src/main.rs:71-77
let mut synchronization = Some(tokio::spawn(indexing_runtime::run(
    indexer,            // Arc<dyn Indexer> = Composer over every configured chain
    wallets.clone(),    // Arc<dyn FilterSource>
    interval,           // Config::interval() = min over chains (chains.rs:82-93)
    shutdown_rx,
    sync_state,         // watch::Sender<SyncState>, one state for the process
)));
```

`Composer::sync` walks its children in order and `?`-es out of the first
failure:

```rust
// sdk/indexing/src/composer.rs:113-124
let mut statuses = Vec::with_capacity(self.indexers.len());
for indexer in &self.indexers {
    let narrowed = UnitSelection { selection, networks: indexer.networks() };
    statuses.extend(indexer.sync(&narrowed).await?);   // <- first error aborts the rest
}
Ok(statuses)
```

`indexing_runtime::run` then classifies that one error for the whole process
(`sdk/indexing/runtime/src/lib.rs:31-61, 101-143`):

| Failure on any one chain | Effect on the other chains | Effect on the process |
|---|---|---|
| retryable (`Source`, `CannotConnect`, `Conflict`) | chains ordered **after** it are not synced at all this pass | `Retrying`, readiness false, retried forever |
| non-retryable (`InvalidBlock`, `NetworkMismatch`, `InvalidRequest`, `ReorgTooDeep`, corrupt `Store`) | indexing **stops for every chain** | `run` returns `Err`; during startup the listener never binds (`main.rs:90-97`), at runtime the process exits (`main.rs:147-153`) |

Reachable example, end to end: a Bitcoin address is adopted with a birthday
after its funding transaction; a later block spends an output that was never
indexed; both stores raise the same non-retryable error
(`sdk/indexing/redb/src/repository/blocks.rs:196-203`,
`sdk/indexing/postgres/src/projection.rs:219`):

```
IndexError { kind: InvalidBlock, message: "block spends an unknown indexed output", retryable: false }
```

Ethereum and Solana stop with it. Their `/v1/status` checkpoints freeze while
`GET /v1/wallets/{id}/transactions` keeps answering `200` from them.

Three secondary consequences of the single loop, worth fixing in the same pass:

- **one cadence for every chain.** `Config::interval()` takes the `min` over
  chains (`apps/server/src/chains.rs:82-93`), so Solana's 400 ms poll also polls
  Bitcoin every 400 ms.
- **head-of-line latency.** A chain whose node is timing out holds the pass, so
  a healthy chain's next batch waits for the slow chain's RPC timeout.
- **failures are not addressable.** `SyncState::Retrying { error: String }`
  carries a message with no scope, so `readiness.rs:18` prints
  `synchronization is retrying: node busy` without saying which chain.

## 2. Isolation: one task per sync unit

### 2.1 Options

| | Shape | Verdict |
|---|---|---|
| (a) | one task per scope instead of one loop over the `Composer` | **recommended** |
| (b) | keep one loop; `Composer::sync` collects per-scope results instead of `?` | rejected |
| (c) | a supervisor that restarts a failed scope's task with backoff | folded into (a) |

**(b) is rejected** on three grounds. First, it changes the `Indexer` trait
contract for every implementor: `sync` returns
`Result<Vec<SyncStatus>, IndexError>` (`sdk/indexing/src/indexer.rs:164-167`),
and collecting failures means returning something like
`Vec<Result<SyncStatus, IndexError>>` from `Service` too
(`sdk/indexing/src/service.rs:164-172`), where exactly one scope exists and the
extra shape is noise. Second, it fixes only the *abort*, not the head-of-line
latency or the shared cadence — the children still run sequentially inside one
future. Third, it leaves `run`'s per-process `SyncState` unchanged, so the
callback of §3 would still have no per-scope identity to report.

**(c) alone is not enough and not needed as a separate layer.** A restart with
backoff is what a *task* needs after a panic; a *failure* needs a state machine,
and the state machine is cheaper to keep inside the unit loop than to encode as
task death and rebirth. Supervision is kept for exactly one job: a unit task
that panicked or ended early is recorded as `Quarantined` rather than taking
`run` down.

### 2.2 The unit

The isolation unit is the child `Arc<dyn Indexer>` and the scopes it owns, not
the `Network`. Every chain's indexer is a `Service` over exactly one scope
(`sdk/indexing/src/service.rs:154-172`, `delegate_indexer!` at `service.rs:240`),
so in practice unit == scope today, but the trait admits a multi-scope child and
the design must not assume otherwise.

`Composer` already knows the partition and already owns the narrowing wrapper
that makes a child safe to drive with a shared selection — `UnitSelection`
(`sdk/indexing/src/composer.rs:133-162`). That narrowing is load-bearing, not
cosmetic: `Synchronizer::sync` calls `selection.filters()` and rejects any
filter belonging to another scope as a non-retryable `NetworkMismatch`
(`sdk/indexing/src/synchronizer.rs:82, 216-226`). Handing `wallets::Wallets`
straight to a `Service` would therefore fail permanently.

So `Composer` exposes its partition, and `UnitSelection` stays private behind it:

```rust
// sdk/indexing/src/composer.rs — new
/// One child indexer and the scopes it owns: the unit of failure isolation.
pub struct SyncUnit { /* Arc<dyn Indexer> + &[Network] */ }

impl SyncUnit {
    pub fn networks(&self) -> &[Network];
    /// Syncs this unit alone, narrowing `selection` to its own scopes.
    pub fn sync<'a>(&'a self, selection: &'a dyn FilterSource)
        -> BoxFuture<'a, Result<Vec<SyncStatus>, IndexError>>;
}

impl Composer {
    /// The independently drivable units, in construction order.
    pub fn units(&self) -> &[SyncUnit];
}
```

`Composer::sync` stays as it is (still an `Indexer`, still used by
`examples/*` and the contract suite) and is simply no longer what the server
drives.

### 2.3 What per-unit tasks do and do not change

| Concern | Evidence | Effect of per-unit tasks |
|---|---|---|
| shared `FilterSource` (`Wallets`) | `sdk/wallets/src/selection.rs:12-35` | none. `filters()` is a read under `RwLock` (`wallets.rs:286-303`); `plan()` already takes a `&Network`. |
| `NetworkAdmission` | `sdk/wallets/src/wallets.rs:156` (`admissions: BTreeMap`), `sdk/indexing/src/admission.rs:44-49` | none. There is already one admission **per scope**, so a publication on chain A never blocked a commit on chain B; the single loop was the only thing serializing them. |
| the `running` guard | `sdk/indexing/src/synchronizer.rs:46, 209-214` | none. It is per-`Synchronizer` and already documents that only one pass per scope runs at a time; units are one pass per scope by construction. |
| redb single-owner thread | `packages/storage/redb/src/backend.rs:34-46, 151-200` | safe. Every operation is one command on one bounded `mpsc` to one OS thread, and `commit(WriteBatch)` evaluates its conditions and writes in one critical section. Concurrency moves from "one scope at a time by construction" to "one command at a time by the channel"; total work is unchanged. Cost: a long `Scan` (a history page) can sit ahead of another scope's `Commit`. Queue bound is 256 (`backend.rs:31`), far above three units. |
| PostgreSQL | `sdk/indexing/postgres/src/revert.rs:54` (`lock_network`) | safe; already scope-locked per network. |
| `Composer` as `History`/`Checkpoint` | `composer.rs:78-100` | unchanged. Reads still route by scope, and a quarantined scope keeps answering from its frozen checkpoint — which is exactly why §4 must report the state on the wire. |
| shutdown ordering | `apps/server/src/shutdown.rs:47-49`, `apps/server/src/main.rs:195-203` | unchanged shape. `run` still returns one `JoinHandle`; it owns a `JoinSet` of unit tasks internally and returns only when all have ended. **A quarantined unit must stay parked and alive until shutdown**, not return — otherwise `run` ends early and `main.rs:150` reports "index synchronization stopped unexpectedly". |

### 2.4 Recommendation

**Option (a), with supervision folded in.** `indexing_runtime::run` becomes a
supervisor: it spawns one task per `SyncUnit`, each running today's loop body
against its own scope with its own interval and its own state machine, and it
returns `Ok(())` only on shutdown. No unit failure ends `run`. A unit task that
panics is recorded as `Quarantined` and parked.

## 3. The callback

### 3.1 Where it lives

The vocabulary goes in `indexing`; the trait goes in `indexing-runtime`.

`Retrying`, `Quarantined`, and `attempts` are properties of a *loop*, and
`indexing` deliberately owns no loop and no async runtime — its manifest has
`base` and `futures-channel` only, no tokio. But `apps/api` depends on
`indexing` and **not** on `indexing-runtime` (`apps/api/Cargo.toml`), and §4
needs the state on the wire. Splitting the two keeps both true:

| Item | Crate | Why |
|---|---|---|
| `NetworkState`, `NetworkFailure`, `NetworkHealth` | `indexing` | plain data, no clock, no runtime; readable by `payment-api` without a new dependency |
| `SyncMonitor` trait, `NetworkEvent`, the state machine | `indexing-runtime` | only the loop can count attempts or decide quarantine |

Putting the trait next to `Observer` (`sdk/indexing/src/observer.rs:29`) was
considered for symmetry and rejected: `Observer` reports a fact the
synchronizer produced (a committed block); a failure event reports a decision
only the driver makes.

### 3.2 Contract

```rust
// sdk/indexing/src/health.rs — new, re-exported from lib.rs
/// What one scope's indexing is doing. Cached, unlike `SyncPhase`, because the
/// process no longer exits on failure and the failure must live somewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkState { CatchingUp, Ready, Retrying, Quarantined }

/// Why a scope is `Retrying` or `Quarantined`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkFailure {
    pub kind: IndexErrorKind,
    pub retryable: bool,
    pub message: String,
}

/// One scope's current indexing health.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkHealth {
    pub network: Network,
    pub state: NetworkState,
    /// The last durably committed block; frozen while quarantined.
    pub checkpoint: Option<BlockRef>,
    pub observed_tip: Option<BlockRef>,
    pub failure: Option<NetworkFailure>,
    /// Consecutive failed passes; 0 while `Ready` or `CatchingUp`.
    pub attempts: u32,
    /// Unix seconds at which `state` was entered. `u64`, matching
    /// `BlockRef::timestamp` (`sdk/base/src/block.rs:98`); `indexing` stays clock-free.
    pub since: u64,
}
```

```rust
// sdk/indexing/runtime/src/lib.rs — new
/// Notified as each scope's indexing state changes.
///
/// Called inline on the failing unit's task, between passes. It must return
/// promptly: hand work to a queue or a channel rather than doing I/O here. It
/// is synchronous by construction — `Observer::observed` is an awaited future
/// (`sdk/indexing/src/synchronizer.rs:320-328`) and a slow observer already
/// slows indexing; a `-> ()` method cannot repeat that mistake.
pub trait SyncMonitor: Send + Sync + 'static {
    fn network_changed(&self, event: &NetworkEvent);
}

#[derive(Clone, Debug)]
pub struct NetworkEvent {
    pub previous: NetworkState,
    pub health: NetworkHealth,   // carries network, state, checkpoint, failure, attempts, since
}
```

```rust
// sdk/indexing/runtime/src/lib.rs — changed signature
pub async fn run(
    units: Arc<Composer>,               // was Arc<dyn Indexer>
    selection: Arc<dyn FilterSource>,
    schedule: Schedule,                 // was Duration; see §5.2
    monitor: Arc<dyn SyncMonitor>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), TaskError>;
```

`state: watch::Sender<SyncState>` is removed: the monitor supersedes it, and
apps/server builds its own `watch` from monitor events (§3.4). `run` still
returns `Err` for one thing only — a unit reporting statuses that do not cover
its scopes exactly once (`lib.rs:63-88`), which is a contract violation of the
`Indexer` implementation itself, not a chain failure.

### 3.3 When it fires

| Transition | Fires | Why |
|---|---|---|
| `CatchingUp` → `CatchingUp` | no | one event per batch is per-block noise |
| any → `Ready` | yes | recovery is the event operators wait for |
| any → `CatchingUp` (from `Ready`/`Retrying`/`Quarantined`) | yes | falling behind, or resuming |
| any → `Retrying` | yes | |
| `Retrying` → `Retrying`, same failure | **yes** | `attempts` changed. A wedged loop at attempt 10 000 must be distinguishable from one at attempt 2; transitions-only would hide it |
| any → `Quarantined` | yes | |
| unit task panicked | yes, as → `Quarantined` | |

Notification-only. It cannot return a decision (retry / quarantine / stop) for
three reasons: the decision is already fully determined by `IndexError.retryable`
and `kind`, which the SDK owns (§5.1); a decision-returning callback moves
policy into the embedder and makes the state machine untestable on its own; and
the one decision an operator genuinely makes — *resume this scope* — is
inherently out-of-band and asynchronous, so it belongs on a handle, not on a
return value:

```rust
// sdk/indexing/runtime/src/lib.rs — new, returned by `run`'s caller-side builder
#[derive(Clone)]
pub struct SyncControl { /* ... */ }

impl SyncControl {
    /// Clears a quarantine: `attempts` resets and the next pass runs at once.
    /// `NetworkMismatch` when the scope is not configured. A scope that is not
    /// quarantined is a no-op.
    pub fn resume(&self, network: &Network) -> Result<(), IndexError>;
}
```

That pair — a notification you react to, and a handle you react *with* — is the
whole of "produce callback to which we can react".

### 3.4 How `apps/server` wires it

There is no logging or metrics framework in the workspace (no `tracing`, `log`,
or `metrics` in the root `Cargo.toml`); today's diagnostics are `eprintln!` in
`apps/server/src/readiness.rs:18`. So `apps/server` implements one monitor that
does all three jobs, and it replaces `readiness::publish` entirely:

```rust
// apps/server/src/readiness.rs — replaces `publish`
/// Turns scope events into the process's log line and the health snapshot the
/// HTTP layer serves. Non-blocking: one format and one `watch` send.
pub(crate) struct Health {
    snapshot: watch::Sender<SyncHealth>,   // indexing::SyncHealth { ready, scopes }
}

impl indexing_runtime::SyncMonitor for Health {
    fn network_changed(&self, event: &NetworkEvent) {
        eprintln!(
            "sync {}/{}: {:?} -> {:?}{}",
            event.health.network.chain, event.health.network.name,
            event.previous, event.health.state,
            event.health.failure.as_ref()
                .map(|f| format!(" ({:?}, attempt {}): {}", f.kind, event.health.attempts, f.message))
                .unwrap_or_default(),
        );
        self.snapshot.send_modify(|health| health.apply(&event.health));
    }
}
```

- **logging**: the one `eprintln!`, now with the scope, the kind, and the attempt
  count — strictly more than `readiness.rs:18` prints today.
- **metrics**: this is the extension point. When a metrics crate is adopted, it
  is one more line in `network_changed`; nothing in `indexing` or
  `indexing-runtime` changes.
- **readiness / status**: `SyncHealth::apply` recomputes the readiness boolean
  (§4.2) and the per-scope rows `GET /v1/status` serves.

## 4. Health model

### 4.1 Per-scope state machine

| From | Event | To | Wait before next pass |
|---|---|---|---|
| any | pass ok, `checkpoint < observed_tip` | `CatchingUp` | none (`lib.rs:131-133` today) |
| any | pass ok, `checkpoint == observed_tip` | `Ready` | the unit's `interval` |
| any | `IndexError { retryable: true }` | `Retrying`, `attempts += 1` | backoff (§5.2) |
| any | `IndexError { retryable: false }` | `Quarantined` | parked |
| any | unit task panicked | `Quarantined` | parked |
| `Quarantined` | `SyncControl::resume` | `CatchingUp`, `attempts = 0` | none |

A scope never auto-quarantines on attempt count. A node down for an hour must
heal by itself, which is today's documented behaviour ("retried forever",
`sdk/indexing/runtime/README.md:38`); `attempts` and `since` are what an
operator alerts on instead.

`checkpoint` is the last good one and is carried through `Retrying` and
`Quarantined` unchanged — it is read straight from the store
(`sdk/indexing/src/synchronizer.rs:95-98`), so it is exactly the point a resume
or a rescan starts from.

Note this contradicts a documented norm: "`SyncStatus` reports only progress
(`CatchingUp` or `Ready`). Failures are typed errors, not cached status
variants." (`ARCHITECTURE.md:561-563`). That norm held because a failure ended
the process, so nothing had to remember it. Once a failure is survivable, it has
to be cached somewhere; `SyncStatus` and `SyncPhase` stay exactly as they are
(pass results), and the cache is `NetworkHealth`. `ARCHITECTURE.md` must be
amended rather than worked around.

### 4.2 What "ready" means

Today readiness requires **every** scope `Ready`
(`sdk/indexing/runtime/src/lib.rs:80-87`), and the server will not bind the
listener until readiness is true (`apps/server/src/main.rs:80-134`,
`AGENTS.md:394-395`). Keeping that rule unchanged would replace "the process
exits" with "the process hangs forever, unbound, with no HTTP surface at all" —
strictly worse than today. So the rule changes:

```
ready = scopes.all(|s| s.state == Ready || s.state == Quarantined)
     && scopes.any(|s| s.state == Ready)
```

| Situation | `ready` | `/health/ready` | Rationale |
|---|---|---|---|
| every scope `Ready` | true | 204 | unchanged |
| any scope `CatchingUp` or `Retrying` | false | 503 | unchanged: serving a half-indexed history as complete is the thing readiness exists to prevent |
| one scope `Quarantined`, at least one `Ready` | **true** | **204** | the healthy chains work; the dead one is reported per scope. This is the user's decision applied to the probe |
| every scope `Quarantined` | false | 503 | the process can serve nothing useful; let the orchestrator restart it. It does **not** exit — a restart cannot fix the deterministic classes (§5.3) and exiting also kills the status surface that explains why |

**Startup gate** (binding the listener, `main.rs:80-134`): each scope must have
*settled* — reached `Ready` at least once, or become `Quarantined`. A scope that
is `CatchingUp` or `Retrying` still holds the gate, as today. The asymmetry is
deliberate: `Retrying` is by definition expected to heal, `Quarantined` is by
definition not. A startup deadline for a scope stuck `Retrying` is a knob worth
having and is explicitly **not** part of this change.

### 4.3 `GET /v1/status`

The only wire change in this document. `NetworkCheckpoint`
(`apps/api/src/api/sync.rs:17-24`) gains two fields:

```rust
pub struct NetworkCheckpoint {
    pub network: Network,
    #[schema(required)] pub checkpoint: Option<Block>,
    /// ready | catching_up | retrying | quarantined
    pub state: NetworkState,
    /// Present exactly when `state` is `retrying` or `quarantined`.
    #[schema(required)] pub failure: Option<NetworkFailure>,
}
```

```json
{
  "ready": true,
  "networks": [
    { "network": {"chain": "ethereum", "name": "mainnet"},
      "checkpoint": {"height": 20310441, "...": "..."}, "state": "ready", "failure": null },
    { "network": {"chain": "bitcoin", "name": "mainnet"},
      "checkpoint": {"height": 845112, "...": "..."}, "state": "quarantined",
      "failure": { "kind": "invalid_block", "retryable": false, "attempts": 1,
                   "since": 1758614400,
                   "message": "block spends an unknown indexed output" } }
  ]
}
```

The handler stops reading checkpoints one by one through
`Checkpoint::checkpoint` (`sync.rs:53-65`) and reads the snapshot instead: one
borrow of a `watch`, no store I/O, and — the point — a checkpoint that is
reported *together with* the reason it stopped moving. `Payments::readiness:
watch::Receiver<bool>` (`apps/api/src/api/mod.rs:104`) becomes
`watch::Receiver<SyncHealth>`; `Payments::ready()` reads `health.borrow().ready`,
so `/health/live` and `/health/ready` handlers (`apps/api/src/api/health.rs`)
are untouched.

## 5. Recovery

### 5.1 Classification

`IndexError.retryable` already carries the decision; this table makes the
mapping explicit and is the one place it lives.

| Kind | Typical source | `retryable` | Policy | Why |
|---|---|---|---|---|
| `Source` | RPC timeout, node busy (`error.rs:87-91`) | as flagged | retry with backoff, forever | nodes restart |
| `CannotConnect` | `synchronizer.rs:78-80`, reorg in flight | true | retry immediately-ish; next pass reconciles | documented as self-healing (`source.rs` / `error.rs:41-43`) |
| `Conflict` | admission handshake (`admission.rs:85-94, 161-168`) | true | retry at once, no backoff | it means another actor just won a race |
| `Store` (retryable) | pool exhaustion | true | retry with backoff | |
| `Store` (terminal) | poisoned admission lock (`admission.rs:178-186`), "history exists without a checkpoint" (`indexer.rs:92-98`) | false | **quarantine** | a poisoned mutex never heals in-process; a corrupt record repeats identically |
| `InvalidBlock` | parse failure, `"block spends an unknown indexed output"` | false | **quarantine** | deterministic: refetching re-parses the same bytes |
| `NetworkMismatch` | `synchronizer.rs:216-226` | false | **quarantine**, log loudly | a programming or composition error |
| `InvalidRequest` | `composer.rs:62-75`, `selection.rs:37-45` | false | **quarantine**, log loudly | invalid configuration or filters |
| `ReorgTooDeep` | `synchronizer.rs:451-457` | false | **quarantine**, needs rescan | §5.3 |
| panic in a unit task | — | — | **quarantine** | |

### 5.2 Backoff and cadence

Today the retry wait is the poll interval with no growth
(`sdk/indexing/runtime/src/lib.rs:124-141`), and every chain shares the min
interval (`apps/server/src/chains.rs:82-93`). Per unit:

```rust
pub struct Schedule {
    /// Wait after a `Ready` pass. Per unit: each chain's own `poll_millis`.
    pub interval: Duration,
    /// Upper bound on the retry wait.
    pub retry_ceiling: Duration,   // default: 60 × interval
}
```

`CatchingUp` waits nothing (unchanged). `Retrying` waits
`min(interval * 2^(attempts - 1), retry_ceiling)`, jittered. `Conflict` is
exempt and retries at `interval`, because it is a lost race, not an outage.
`Config::interval()`'s `min` collapse is deleted; each chain keeps its own.

### 5.3 Getting a quarantined scope back

**Restart is not recovery.** For `InvalidBlock`, `ReorgTooDeep`, `NetworkMismatch`
and `InvalidRequest`, a restarted process re-fetches the same block and fails
identically. Today a restart is the only tool; it has never worked for these.

| Path | Fixes | Shape |
|---|---|---|
| **resume** (operator) | a terminal failure whose *cause* was fixed out of band: the node was upgraded, the store was repaired, the configuration was corrected | `SyncControl::resume(&network)`, exposed as `POST /v1/status/resume` with body `{"network": {"chain": "...", "name": "..."}}` → `202`; bearer-protected; changes nothing durable. For `InvalidBlock` it re-fails within one pass — which is fine and now visible |
| **automatic retry** | every `retryable: true` class | §5.2; already the behaviour, now per scope |
| **rescan** (offline) | `ReorgTooDeep`, `"block spends an unknown indexed output"`, and a past birthday (§6) | below |
| **never auto-retried** | `InvalidBlock`, `ReorgTooDeep`, `NetworkMismatch`, `InvalidRequest`, terminal `Store`, panic | they are deterministic; an automatic retry loop on them is a busy-wait that hides the defect |

**`ReorgTooDeep` has no code path back today.** `common_ancestor` raises it
(`synchronizer.rs:451-457`), the documented remedy is "only that scope's
indexing rows may be recreated and rescanned" (`docs/INDEXING.md:111`,
`AGENTS.md:337-338`, `ARCHITECTURE.md:458-469`), and no tool does that. It
cannot be done through the existing ports: `BlockStore::remove` unwinds **one
tip** using the rollback journal (`sdk/indexing/src/block.rs:180-186`), and the
journal retains only `reorg_retention` heights
(`sdk/indexing/src/sync_policy.rs:12-14`, applied at `synchronizer.rs:302-307`).
Rewinding past retention is not a rewind, it is a purge.

> **Landed.** `025` then deleted `BlockStore::remove` and the journal with it,
> which removes the "past retention" nuance above: nothing is ever rewound, so
> a purge is the only shape there was. The class is `ConfirmedReorg`
> (`synchronizer.rs`, `confirm_checkpoint`). What exists now:
>
> - `BlockStore::purge_network(network) -> BoxFuture<Result<PurgedRows, IndexError>>`
>   in `sdk/indexing/src/block.rs`, implemented atomically by every backend
>   (`sdk/indexing/redb/src/repository/blocks.rs`; the PostgreSQL one existed
>   too until `027` deleted that crate).
>   `PurgedRows { checkpoint, history, movements, outputs }` is the row count
>   per collection; redb reports no movement rows because it nests movements
>   inside the history record, which is the asymmetry the contract tolerates.
> - `payment-server <config.json> --rescan <chain>/<network>`
>   (`apps/server/src/rescan.rs`), offline exactly as argued below.
> - `indexing::testing::PurgeContract::purge_network_removes_only_its_own_network`,
>   run by every backend's contract suite over two networks of one store, which
>   asserts the other network and both registries survive, that a repository
>   refuses another network's purge, and that a second purge removes nothing.
> - `a_rescanned_network_reindexes_from_its_registered_start` in
>   `sdk/indexing/tests/service_contract.rs`: quarantine, purge, restart,
>   re-index from the birthday anchor.
>
> `docs/INDEXING.md` §The offline rescan, `docs/API.md` §Clearing a quarantine,
> `ARCHITECTURE.md` §The rescan, and the three crate READMEs carry the
> operator-facing version.

Recommended shape — an **offline** subcommand, not an API route:

```
payment-server <config.json> --rescan bitcoin/mainnet
```

It purges that scope's indexing-owned rows only — checkpoint, history,
movements, outputs, journal — never `payments_wallet` and never another scope
(`AGENTS.md:38`, `docs/INDEXING.md:178-180`), prints what it removed, and exits.
The next normal start finds `checkpoint.is_none()` and re-indexes from
`plan.earliest_position()` (`synchronizer.rs:135-137, 176-185`). It needs one
new port method, `BlockStore::purge_network(network) -> Result<(), IndexError>`,
implemented once per backend.

Offline rather than an endpoint, deliberately: purging a scope while HTTP serves
`GET /v1/wallets/{id}/transactions` from it would hand clients an empty history
that looks authoritative, and the checkpoint-bound cursors
(`sdk/indexing/src/indexer.rs:41-52`) would silently conflict mid-page.

## 6. Adjacent finding: a birthday in the past silently indexes nothing

### 6.1 The defect

`catch_up` consults the plan's earliest birthday **only** when there is no
checkpoint:

```rust
// sdk/indexing/src/synchronizer.rs:134-137
let mut applied = 0_usize;
if checkpoint.is_none() {
    (checkpoint, applied) = self.initialize_checkpoint(observed_tip, plan).await?;
}
// ...only initialize_checkpoint reads plan.earliest_position() (synchronizer.rs:176)
```

Once a checkpoint exists, `start_position` is used only by
`SyncPlan::active_addresses(position)` to *exclude* an address from blocks below
its birthday (`sdk/indexing/src/admission.rs:225-231`). Nothing ever indexes
below the checkpoint. So:

```
apps/server/src/config.rs:79,96   start_position: u64 -> BlockPosition
  -> Wallets::import                sdk/wallets/src/wallets.rs:161-184
  -> Wallets::store(..., Some(pos)) sdk/wallets/src/wallets.rs:320-343
```

and note `store`'s explicit-birthday branch takes **no publication permit** at
all (`wallets.rs:323-329`), so the birthday is never compared with the
checkpoint by anyone. On a restart with an existing store, a configured
`"start_position": 1` matches nothing historical, returns `201`/`Ok`, and logs
nothing. The repository's own acceptance fixtures do exactly this
(`apps/server/tests/acceptance.rs:25-26`). `docs/API.md:121-123` already
promises the opposite — that such a birthday works, given a rescan — and no
rescan exists (§5.3).

The HTTP surface is not affected: `POST /v1/addresses` and `POST /v1/wallets`
carry no birthday and always derive checkpoint + 1
(`apps/api/src/api/address.rs:60-70`, `wallets.rs:331-341`). This is a
configuration-path and SDK-path defect only.

**Interaction with §1's terminal error.** A partially historical address is
worse than a no-op. If the funding transaction is below the checkpoint and the
spend is above it, Bitcoin's store raises the non-retryable
`"block spends an unknown indexed output"`
(`sdk/indexing/redb/src/repository/blocks.rs:196-203`), which today stops every
chain and after §2 quarantines Bitcoin. So the accepted birthday does not merely
lose history; it arms a terminal failure at an unpredictable later time.

### 6.2 Options

| | Option | Verdict |
|---|---|---|
| (i) | reject a birthday below the checkpoint with a clear error | **recommended**, with one refinement |
| (ii) | accept it and warn through the new callback | rejected |
| (iii) | support a real rescan | **recommended as the companion**; it is §5.3's tool |
| (iv) | per-address backfill without rewinding the scope | rejected |

**(ii) is rejected.** Warning about a condition that will later hard-quarantine
the scope is worse than refusing it, and it contradicts a standing rule:
"Reject unsupported meaningful input; never accept ignored configuration."
(`AGENTS.md:391`). A `start_position` below the checkpoint is precisely ignored
configuration.

**(iv) is rejected** on three counts. UTXO state cannot be reconstructed for one
address in isolation — `spend_outputs` requires every relevant creation to be
present (`sdk/indexing/redb/src/repository/blocks.rs:183-211`), so a partial
backfill writes a live output table that disagrees with the chain. History
pagination binds to a checkpoint and rejects a page whose checkpoint moved
(`sdk/indexing/src/indexer.rs:41-52`); inserting older rows under an unchanged
checkpoint makes in-flight cursors skip or repeat rows with no conflict raised.
And it duplicates the whole synchronizer along a second, differently shaped
path.

**(iii) alone is not enough**: it is the remedy, but without (i) nothing tells
the operator they need it.

### 6.3 Recommendation: (i) + (iii)

`Wallets::import` and `Wallets::insert` already hold a `Checkpoint`
(`sdk/wallets/src/wallets.rs:150-157`). They compare:

```rust
// sdk/wallets/src/wallets.rs — in store(), explicit-birthday branch
if let Some(checkpoint) = self.checkpoint.checkpoint(&factory.network).await?
    && start_position <= checkpoint.position
    && !already_registered_at(&address, start_position)    // restored from Registry
{
    return Err(Error::new(
        ErrorKind::Unsupported,
        "wallet birthday precedes the scope checkpoint; \
         rescan the scope (payment-server --rescan <chain>/<network>) or raise start_position",
    ));
}
```

The `already_registered_at` refinement is load-bearing. Without it, a correctly
configured server fails on **every** restart, because the config keeps
`"start_position": 1` while the checkpoint advances — that is
`apps/server/tests/acceptance.rs:25-26` today. An address already present in the
registry at that birthday (restored at startup,
`sdk/wallets/src/wallets/persistence.rs:129`) is re-imported idempotently and
keeps its stored birthday; only a *new* address with an impossible birthday is
refused. A refused startup import fails the process at
`apps/server/src/config.rs:94-98`, which is the correct outcome for
configuration that cannot be honoured.

Already-stored rows on a rescan: the purge removes the scope's indexing rows and
the scope re-indexes them from the earliest birthday, so history is rebuilt, not
merged. Retention is irrelevant to the purge — the journal is discarded with the
rest and rebuilt as blocks re-apply.

## 7. Migration

### 7.1 Changes by crate

| Crate | Change | Files |
|---|---|---|
| `indexing` | add `NetworkState`, `NetworkFailure`, `NetworkHealth`, `SyncHealth`; add `SyncUnit` and `Composer::units()`; keep `UnitSelection` private behind `SyncUnit`; add `BlockStore::purge_network` | `src/health.rs` (new), `src/composer.rs:102-162`, `src/block.rs:167-187`, `src/lib.rs` |
| `indexing-redb`, `indexing-postgres` | implement `purge_network` for one network only | `sdk/indexing/redb/src/repository/blocks.rs`, `sdk/indexing/postgres/src/revert.rs` |
| `indexing-runtime` | rewrite `run` as a per-unit supervisor; add `SyncMonitor`, `NetworkEvent`, `SyncControl`, `Schedule`, backoff; delete `SyncState` | `src/lib.rs:8-143`, `README.md:19-73` (doc-tested) |
| `wallets` | birthday-vs-checkpoint check in `store()`'s explicit branch | `src/wallets.rs:320-343` |
| `apps/server` | pass `Arc<Composer>`; per-chain interval (delete `Config::interval()`'s `min`); `readiness::Health` monitor replacing `publish`; `--rescan` subcommand; hold `SyncControl` for the resume route | `src/main.rs:45-78,135`, `src/chains.rs:82-93,125`, `src/readiness.rs` |
| `payment-api` | `NetworkCheckpoint.state` + `.failure`; `Payments` takes `watch::Receiver<SyncHealth>`; `POST /v1/status/resume` | `src/api/sync.rs:17-70`, `src/api/mod.rs:100-122` |
| examples ×4 | new `run` signature + a no-op monitor | `examples/{indexing,transfer,watch_only,all_chains}/main.rs` |
| docs | amend the fatal-exit / all-scopes-ready contracts | `ARCHITECTURE.md:555-581`, `docs/SYSTEM_REQUIREMENTS.md:262-264`, `AGENTS.md:392-395`, `docs/INDEXING.md:95-112`, `docs/API.md:121-123`, `docs/openapi.json` |

### 7.2 Stored and wire impact

| Surface | Changes |
|---|---|
| storage schema, records, migrations | **nothing**. `purge_network` deletes rows; it adds no column and no table |
| `SyncStatus`, `SyncPhase`, `BlockRef`, `AddressFilter` | **nothing** |
| HTTP | `GET /v1/status` gains `state` and `failure` per network; `POST /v1/status/resume` is new. `/health/live`, `/health/ready`, and every wallet/history route are byte-identical |
| readiness semantics | changed (§4.2) — a quarantined scope no longer holds `/health/ready` at 503 |
| `indexing_runtime::SyncState` | removed. Pre-release, so it is replaced rather than aliased (`AGENTS.md:32-35`) |

### 7.3 Tests to add

| Test | Where | Asserts |
|---|---|---|
| `one_scope_failure_does_not_stop_the_others` | `sdk/indexing/runtime/src/lib.rs` | with two units, one returning a non-retryable error, the other keeps being called and reaches `Ready` |
| `a_non_retryable_failure_quarantines_only_its_scope` | idem | the failing unit's events end at `Quarantined`; `run` does not return |
| `a_quarantined_unit_still_observes_shutdown` | idem | `run` returns `Ok(())` on shutdown with one unit quarantined |
| `a_panicking_unit_is_quarantined_not_propagated` | idem | |
| `retry_backoff_grows_and_is_capped` | idem | attempts 1..n wait the expected bounded times |
| `resume_clears_quarantine_and_runs_a_pass_at_once` | idem | `SyncControl::resume` |
| `monitor_fires_on_every_attempt_but_not_on_every_catch_up_batch` | idem | the §3.3 table |
| `status_reports_a_quarantined_network_with_its_failure` | `apps/api/tests/route_contract.rs` + `openapi_contract.rs` | payload shape, `ready: true` with one scope quarantined |
| `readiness_stays_204_while_one_scope_is_quarantined` | `apps/server/tests/` (new file needs a `[[test]]` stanza — `apps/server/Cargo.toml` sets `autotests = false`) | §4.2 |
| `a_birthday_below_the_checkpoint_is_refused_for_a_new_address` and `..._is_accepted_for_a_restored_one` | `sdk/wallets/src/wallets.rs` tests | §6.3 |
| `purge_network_removes_only_its_own_network` | both repository contract suites | preserves other networks and `payments_wallet` |

Existing tests that must change: `sdk/indexing/runtime/src/lib.rs:295-511` (the
whole `SyncState` suite — `fatal_failure_is_returned_and_published_not_ready` at
`:323` now asserts the opposite), `apps/server/src/readiness.rs:30-81`,
`apps/server/tests/redb_api.rs:144`, and the doc-tested example at
`sdk/indexing/runtime/README.md:50-61`.

### 7.4 Effort

| Piece | Days |
|---|---|
| `indexing`: health types, `SyncUnit`, `Composer::units()` | 1 |
| `indexing-runtime`: supervisor, monitor, control, backoff | 2 |
| `apps/server`: wiring, per-chain cadence, monitor, shutdown | 1 |
| `payment-api`: status payload, resume route, OpenAPI regeneration | 1 |
| `wallets`: birthday refusal (§6) | 0.5 |
| `purge_network` + `--rescan` on both backends (§5.3) | 1.5 |
| tests and doc amendments | 1.5 |
| **total** | **~8.5** |

§2–§4 alone — the isolation and the callback, which is what the decision asked
for — is about **4 days** and is independently shippable; §5.3 and §6 can follow
as one recovery-tooling change.
