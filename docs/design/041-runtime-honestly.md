# 041: Runtime, honestly — the orchestration stays, the implementation goes

> "this sounds to me that adding runtime is just patch to crappy design"

Status: **proposal.** Verdict up front: the orchestration layer in
`apps/server` is **required by `SYSTEM_REQUIREMENTS`** (durable settings,
persisted run/pause, revision-checked node switching with drain, online
reindex with persisted phase, per-network independence) and its placement is
**already correct** — the ownership table gives supervision and lifecycle to
the composition root. What is crappy is the implementation, not the
placement. There is no `sdk/runtime` package in this document. Two small SDK
supports are needed; the rest is server-internal repair.

---

## 0. What was audited

- Runtime's five jobs (composition lifecycle, desired-state reconciliation,
  publication, health merge, recovery/admission) against SDK surfaces.
- SDK audit: per-network supervision exists (`Supervisor` per single-unit
  `Index`, used at `runtime/network.rs:320`); pause/resume exists
  (`SyncControl::request_pause`, `resume`); per-call retry exists
  (`json-rpc::Retry`). What the SDK lacks is below.
- Requirements audit: `SYSTEM_REQUIREMENTS.md`, "Composition, health and
  lifecycle" items 1–7 and the UI-reindex paragraph. Every dynamic behavior
  the Runtime has is a stated requirement.

## 1. SDK support 1 — online reindex in `indexing`

**Gap.** `Purging`'s contract says offline only: "a network emptied under a
live reader serves an empty history that looks authoritative." The
requirement demands the opposite: quiesce admissions and passes, purge,
reset, resume — online, phase persisted.

**Change, contract level.** The quiesced flow becomes a stated, tested mode
of the port: a network whose admissions are blocked and whose unit is paused
may be purged by its controller; the purge must refuse or the controller must
block reads for that network during the window; the next pass re-derives from
registered birthdays. Wording lives on `Purging` and `SyncControl`.

**Change, test level.** A contract test in `sdk/indexing/tests/` proves it
on both stores: pause → purge → resume re-indexes from the earliest
birthday; other networks' rows untouched; a read attempted mid-window is
refused, not served empty history; a restart mid-phase resumes scanning
without purging again.

**Not here:** the phase persistence, the UI, the admission blocking — those
are the application's (`038`'s registry-first world already persists phases
for wallets; the reindex phase lives in `settings`, application data).

## 2. SDK support 2 — submissions survive connection replacement

**Gap to verify, then close.** The requirement: switching nodes preserves
"existing factories, sender reservations and ambiguous submissions." If
`wallets::Submissions` and sender reservations are owned per connection
(inside `ActiveNetwork`'s construction), a node replacement drops in-flight
ambiguity state.

**Change.** Submissions and reservations become owned by the *network slot*,
not the connection: each new connection for the same network receives the
existing handles (`base::supervision`'s registrar exists for exactly this).
If inspection shows they already are, this item collapses to a test pinning
it: node switch with an ambiguous submission in flight; the recovery task
still resolves it after the switch.

## 3. Server repairs — nine items

Ordered so the state model lands before its callers.

1. **Snapshot publication, not a locked bag.** `Payments` becomes an
   immutable `PaymentsState`. Publication swaps one
   `watch::Sender<Arc<PaymentsState>>`; `Application` holds the receiver.
   `FromRef` hands handlers an `Arc`, not a 12-field clone. A request sees
   either the old or the new composition, never a mix, and never takes a
   lock. This replaces `Arc<RwLock<Payments>>` everywhere.
2. **Publish before bind.** `Runtime::run` currently populates `Payments`
   only inside reconcile, while `main` binds the listener immediately.
   Initial publication happens before the listener accepts; until then
   readiness reports not-ready (already the rule) and requests cannot see an
   empty index.
3. **Poison policy, once.** Six copies of
   `unwrap_or_else(PoisonError::into_inner)` and two stringly
   `"runtime poisoned"` errors. One lock-policy helper; poison becomes a
   typed error at the boundary that can type it.
4. **`Settings` owns its surgery.** `Runtime::command` does
   read-modify-write with manual revision, resume-counter, and rebuild
   checks; `drive` re-derives the chain→network row lookup. Both move to
   `Settings`: `command(network, Action)` and `desired(network) ->
   Option<(revision, Network)>`. Runtime stops spelling table internals.
5. **Derive `networks()` once per settings change**, not in `run`, `drive`,
   and `publish_health` independently.
6. **One timed-wait helper** for the two `select!` loops (`run`, `drive`) —
   shutdown-signal or settings-change or tick, shared.
7. **The reads router stops being a local third copy.** After `040`, the
   per-network read router comes from `indexing::storage` as one type; until
   `040` lands, the local `Reads` gets the shared-lookup treatment from
   `039` §1.2.1 instead of a third spelling.
8. **Flatten `rpc: Vec<(Network, Vec<Statistics>)>`** — the one-element-vec
   artifact of `publish()` — to one statistics value per network.
9. **Group the observability fields.** `errors`, `node_statistics`, `logs`,
   `logger` become one `Status` struct; `Runtime` sheds four fields.

## 4. A contradiction to settle

`SYSTEM_REQUIREMENTS.md` says "store keys and RPC settings **without
encryption**." The registry is encrypted (decision D1 of `038`; delivered by
the handoff). Implementation and requirements disagree.

- **Default:** update the requirement to "encrypted at rest under a
  configured passphrase; plaintext only by explicit configuration" — matches
  `038` and delivered behavior.
- Alternative: strip the encryption. Not recommended; it would be a
  downgrade of shipped behavior on a security axis.

Decided in the same change set as step 3, before docs.

## 5. Work order

One step per change set; every step leaves the workspace green.

1. **SDK: online reindex** (§1). Contract wording + both-stores test.
2. **SDK: submissions survival** (§2). Verify; close or pin with a test.
3. **Server: the state model** (§3 items 1–2). `PaymentsState`, watch-based
   publication, publish-before-bind.
4. **Server: settings ownership** (§3 items 4–6).
5. **Server: observability** (§3 items 3, 8, 9).
6. **Server: reads router** (§3 item 7), sequenced after `040` or as its
   local interim.
7. **The encryption contradiction** (§4) — requirement text or code, one of
   them moves.
8. **Docs.** `SYSTEM_REQUIREMENTS` if §4 updates it; `ARCHITECTURE.md`
   runtime paragraph if shape names changed; crate READMEs untouched (no
   public SDK API changed by §3).

## 6. Out of scope

- **No `sdk/runtime` package.** One consumer; the ownership table already
  places supervision in the composition root. Revisit only if a second
  application appears.
- **No new product capabilities.** Everything here implements existing
  requirements or repairs their implementation.
- **No API surface change.** Routes, wire, and `docs/openapi.json` are
  untouched; `PaymentsState` is internal.
- **No changes to `indexing::Supervisor`'s design.** Per-network
  supervision, retry, quarantine, and status stay as they are; `041` adds a
  tested mode, not a new driver.

## 7. Verification

Per step: the eight gates from `AGENTS.md`, plus the Linter MCP `check`.

New behavioral tests that pin the repairs:

- **Snapshot consistency.** A publish landing mid-request: the request
  completes against the composition it started with; a following request
  sees the new one. Never a mixed `Payments`.
- **Publish-before-bind.** The first request after startup never observes an
  empty index; readiness stays not-ready until the initial publish.
- **Settings commands.** Pause/resume/reindex through `Settings::command`
  refuse stale revisions; `desired` returns exactly one network's row.
- **Online reindex** (SDK step): the §1 contract test, on sqlite and memory.
- **Submissions survival** (SDK step): ambiguous submission across a node
  switch is still resolved.
- **healthz invariants** from `038` still hold after the observability
  grouping: memory-only, no URLs, credentials, or remote prose.
