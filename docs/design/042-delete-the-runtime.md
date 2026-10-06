# 042: Delete the runtime — static composition returns

> "I hate everything about apps/server/src/runtime how can we get rid of it?"

Status: **implemented**, with two deviations: the browser page stays (Wallets and
the API reference; only its settings page is deleted, as §1 allows), and D4 is moot —
the key registry is plaintext and the requirement already says so. Original text follows.

Status at writing: **proposal.** This document deletes the dynamic control plane —
durable settings, HTTP pause/resume/reindex, node failover switching, the
settings UI — and with it `apps/server/src/runtime/` and `runtime.rs`
entirely. Composition returns to static: read config once, compose once,
serve, shut down. It supersedes `041` except `041` §4 (the encryption
contradiction, carried here as D4), and it edits `SYSTEM_REQUIREMENTS.md`
where that file described the control plane.

---

## 0. Why this is safe

- Every capability the runtime implements is control-plane: changing the
  system while it runs. The product goal (`038`) never asked for that —
  receive funds, count them, send and sweep them.
- The SDK's static composition API already exists and is tested:
  `wallets::Composition::verify_then_build` per chain, `Index::of` once,
  one `indexing::Supervisor` over the composed index, `SyncControl` for
  live quarantine resume. No SDK change is needed anywhere in this
  document.
- The one operation that genuinely mutates indexed state — reindex — is
  the offline `--rescan`, which already exists and never needed the
  online mode.
- Runtime's other jobs are artifacts of dynamism itself: publication (swap
  a static value?), reconciliation (re-read a static config?),
  admission gates (block during swaps that no longer happen). Remove the
  dynamism and the jobs evaporate rather than needing replacement.

## 1. What is deleted

| File | Lines | Why it can go |
|---|---|---|
| `src/runtime/` (5 modules) | ~670 | Nothing left for it to do |
| `src/runtime.rs` | 314 | The orchestration itself |
| `src/settings.rs` | 504 | Durable settings existed only for mid-run change |
| `src/api/chains.rs` | ~360 | The HTTP settings API |
| `src/api/control.rs` | ~100 | HTTP pause/resume/reindex |
| `src/ui.rs` (or its settings page) | ~194 | Served the control API |
| `Payments` fields `rpc`, `inactive`, `logs`, `logger`, `control`, `access` | — | Existed only to publish runtime state |
| `Application::runtime` | — | The API's handle into the orchestrator |

≈ 2,000+ lines. `main.rs` returns to the short static flow.

## 2. What replaces each runtime job

| Runtime job | Static world |
|---|---|
| Composition lifecycle | `main`: parse config → per chain `Composition::verify_then_build` → `Index::of` → build `Payments` once |
| Desired-state reconciliation | Nothing. Config is read once. Chains that fail verification are skipped and logged (`SYSTEM_REQUIREMENTS` item 1 already allows unavailable nodes) |
| Publication | Nothing. `Payments` is immutable after construction |
| Health merge | One `Supervisor` over the composed `Index`; its `SyncMonitor` feeds one health channel; `GET /healthz` reads it |
| Recovery / admission | `operator --rescan` (offline, unchanged); no admission gates because nothing is ever swapped |

## 3. The HTTP surface after

The nine product routes plus one probe, exactly the `038` surface:
`GET /healthz`, `GET /v1/assets`, `POST/GET /v1/wallets`,
`GET /v1/wallets/{id}`, `…/balance`, `…/transactions` (GET+POST),
`POST /v1/transactions`, `POST /v1/addresses`. `docs/openapi.json`
regenerates with the two control routes gone. Handlers stop taking
admission permits; sends proceed against the static composition.

## 4. `SYSTEM_REQUIREMENTS.md` edits

- **Item 3** ("Bind the control UI/API before node verification") — delete.
- **Item 6** ("Persist requested run/pause and revision-checked node
  settings… drain operations before switching") — delete. Node selection
  becomes startup-time: first verified candidate wins; transport retry
  (`json-rpc::Retry`) covers per-call failure.
- **The UI-reindex paragraph** — replace with: reindex is the offline
  `--rescan` subcommand; `Purging` stays offline-only and its contract is
  untouched (which also retires `041` §1).
- Items 1, 2, 4, 5, 7 and the healthz paragraph stand unchanged.

## 5. Open decisions

| # | Question | Default |
|---|---|---|
| D1 | The UI | Delete `ui.rs` entirely; the server serves the API only |
| D2 | Supervisor shape | One `Supervisor` over the whole `Index` — per-unit failure isolation is its own design; one network's failure cannot stop another's |
| D3 | Unavailable node at startup | Skip and log, per item 1; healthz reports over live networks |
| D4 | Key encryption contradiction (`041` §4) | Fix the requirement text: "encrypted at rest under a configured passphrase; plaintext only by explicit configuration" |

## 6. Work order

One step per change set; every step leaves the workspace green. Consumers
are severed before foundations are deleted.

1. **Sever the API.** Remove `control.rs` and `chains.rs` from the router;
  regenerate `docs/openapi.json`; delete their route-contract tests.
  `Application` loses `runtime`.
2. **Shrink `Payments`.** Drop the six runtime fields and `admission()`;
  handlers shed their permit checks; `healthz` sheds runtime-sourced
  sections until step 4 rewires them.
3. **Static `main`.** Rewrite to the static flow: compose once, build
  `Payments` once, spawn one `Supervisor`, serve, coordinated shutdown
  (supervisor join, HTTP drain, submission drain — the `038` behavior).
4. **healthz, statically.** Per-endpoint statistics come from the chain
  clients at composition time (their verify step already produces
  `json_rpc::Statistics`); health from the supervisor's monitor. No lock
  maps anywhere.
5. **Delete the foundation.** `runtime/`, `runtime.rs`, `settings.rs`,
  `ui.rs`. `operator` gains `--reindex` as an alias of `--rescan` if the
  vocabulary matters, else nothing.
6. **Tests.** Delete control-plane tests; add one static-composition test:
  a config whose second chain's node is unavailable composes the first and
  reports it; one supervisor quarantine does not stop the other network's
  unit.
7. **Docs.** The `SYSTEM_REQUIREMENTS` edits of §4; D4; `ARCHITECTURE.md`
  loses the runtime paragraph; crate READMEs untouched. This document and
  `041` are marked superseded/retired as appropriate.

## 7. Out of scope

- **No SDK changes.** Static composition, supervision, purge, and retry
  already exist and keep their contracts — `Purging` especially stays
  offline-only.
- **No replacement orchestration.** Nothing moves to another module or
  crate. The jobs that only dynamism created are deleted, not relocated.
- **No new features.** This is subtraction only.

## 8. Verification

Per step: the eight gates from `AGENTS.md`, plus the Linter MCP `check`.

What proves the elimination:

- Workspace compiles with no `runtime` module and no `settings` table.
- The committed `docs/openapi.json` diff shows exactly the two control
  routes removed and nothing else.
- The static-composition test of step 6 passes on both stores' contract
  suites as they stand.
- healthz answers from memory with no lock, no publication loop, and no
  per-network registry — and still carries per-endpoint RPC statistics.
- A restart mid-catch-up behaves exactly as `038` specified: watches and
  keys restored, scanning resumes, no phase table consulted.
