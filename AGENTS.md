# Repository guide

Build a small, reliable SDK for third parties and `apps/server`; prefer less code,
clear ownership and observable correctness.

Nobody wants to read long messages, speak like caveman short sentences, bulltet points, fast to mentally grab.
This is for interaction with user.

## Ground truth

- Follow explicit user decisions first; current source, manifests and tests establish
  implemented behavior and exact APIs, not proof that every design is correct.
- Read `docs/SYSTEM_REQUIREMENTS.md`, `ARCHITECTURE.md` and the owning README;
  the generated `docs/openapi.json` describes HTTP.
  Resolve stale documentation against the code and user requirements; do not preserve
  obsolete architecture because a document describes it. ADRs record history.
- Pre-release: replace obsolete internal APIs/formats directly; no compatibility
  aliases, legacy readers or invented V1/V2 DTOs. External protocol contracts remain.
  SQLite schema changes are transactional and versioned; reject unknown file versions.
- Leave `old/` and `reference/` alone unless requested. If `.codegraph/` exists, use
  `codegraph explore` before broad source searches; otherwise use `rg`.

## Scope and ownership

The product runs one API process over Bitcoin, Ethereum and Solana: generate/import
wallets, observe public addresses, return exact balances and history, submit transfers
or ordered batches, and recover from restarts and reorgs. No deposit accounting,
business ledger, sweep jobs, payment state machine, remote/production custody,
separate indexer service, raw-block archive or multi-writer deployment.

| Owner | Responsibility |
| --- | --- |
| `apps/server` | The one application, a binary with no library target; composition root: concrete chains (`src/chains.rs` only), configuration, storage choice, supervision, readiness and shutdown |
| `apps/server/src/api/` | Thin HTTP/OpenAPI adapters, wire models and process signing map; names no concrete chain, `sdk_config` or storage backend (`scripts/check-chains.sh` check 8) |
| `sdk/base` | Small chain-neutral values and signing/broadcast contracts; no chains, RPC, indexing or wallets |
| `sdk/chains/*` | Native protocol, RPC, addresses, signing, transaction construction/broadcast and indexing translation |
| `sdk/wallets` | Factory collection, wallet capabilities and batch orchestration; no custody or durable watch ownership |
| `sdk/indexing` | Watches, block inspection, continuity, confirmation, pending blocks and atomic commit coordination |
| `sdk/indexing/src/storage/` | Repository contracts/implementations and balance, history and output queries |
| `sdk/indexing/src/supervisor/` | Optional Tokio driver, retry, quarantine and network status |
| `sdk/config` | Configuration parsing and chain registration without concrete chain dependencies |
| `packages/*` | Reusable mechanics; never import SDK/application crates |

Concrete chains are selected at composition, never in business/HTTP code; chains do
not depend on each other. Native chain modules know no indexing/wallet SDK models.
Keep chain layouts and scenarios consistent with sibling crates and
`scripts/check-chains.sh`. Construct long-lived dependencies once, not in handlers.

## Indexing requirements

These are ownership requirements, including where current code still differs.

- Indexing is an embedded tool, not a service the consumer must deploy: bounded
  `sync()` is fundamental; continuous Tokio execution is optional. The caller owns
  spawning, cancellation and joining; one network's failure must not stop another.
- One caller-supplied repository serves all networks. Never create a database,
  schema, pool or repository per asset; isolate operations by exact network.
- Required persistence is `WatchStore` plus `IndexStore`: registrations, restored
  watch progress, checkpoints and atomic commits. Third-party repositories need no
  balance/history/output query API and can write application reactions transactionally.
- A watch contains public identity, address, asset and inclusive birthday, never keys.
  Persist registration before publishing it to live scanning; coordinate additions
  with active passes. Unwatch stops inspection without deleting indexed facts.
- Track contiguous, hash-verified progress per watch, including inspected empty blocks.
  Resume after that progress, or at its birthday. New watches do not reset existing
  ones; share fetches across overlapping ranges without skipping any watch's history.
- Native position governs traversal and birthdays; produced height governs confirmation.
  Positions may be sparse. Preserve block hash and paired parent position/hash.
- Chain adapters own RPC, interpretation and finality: Bitcoin depth, Ethereum finalized,
  Solana rooted blocks. Keep native payload/cache separate from validated block facts.
- Commit confirmed facts and affected watch progress atomically; check expected state,
  preserve network isolation, and make identical retries harmless. Reconcile uncertain
  writes; cancellation or an error is not proof that a transaction rolled back.
- Pending blocks stay in memory; pending reorgs truncate them without storage rollback.
  Verify saved hashes on restart. A confirmed reorg requires explicit offline recovery
  of that network, preserving registrations, unrelated networks and application data.
- Balance/history/output queries, pagination, calculations and response models belong
  to repository ownership under `storage/`, not the indexing engine. Consumers needing
  pending-inclusive answers compose repository results with pending facts coherently.
- Repository queries must report incomplete inspection honestly from their own snapshot;
  do not rely on the indexer to repair an unsafe or misleading repository answer.
- SQLite and memory stay inside `storage/`, independently feature-gated; memory is a
  legitimate backend. Fixtures and contract suites belong in `tests/`, not public API.
  The layout and import rules of `storage/` are in `ARCHITECTURE.md`, "Dependency direction".
- Persist signed absolute watched balances atomically with unified balance movements;
  transactionless credits have no transaction identity. Preserve spent-output facts so
  historical replay cannot resurrect outputs. Verification must not silently repair
  totals. Do not invent synthetic transactions or account UTXOs.
- After-commit listeners are optional notifications, not reliable transactional delivery;
  callbacks cannot veto commits. Custom repositories own transactional reactions.

## Design and correctness

- Use clear entities and meaningful collections: `Block`/`Blocks`, `Watch`/`Watches`.
  Behavior belongs to its owner; collections must do more than forward vector methods.
- Justify traits by real chain/storage substitution or a necessary consumer/test seam;
  prefer concrete code otherwise. Keep capabilities narrow (normally at most three
  methods); do not split coherent operations merely to satisfy a count.
- No courier fields, rename-only wrappers, speculative infrastructure, duplicate owners,
  universal chain RPC/transaction models, or abstractions serving no actual need.
  Do not mistake coordination between concurrent actors for redundant ownership.
- Names predict contents. Avoid Manager/Helper/Util, vague catch-all modules and stutter.
  Model closed states as enums; newtypes must enforce an invariant or meaningful boundary.
- Put validation on domain constructors/collections; use standard conversions where
  policy-free. Prefer `Type::from_params(params)` to constructors on parameter bags.
- Keep public surfaces small, implementation modules private, errors typed and retryability
  explicit. Reject unsupported input; do not silently ignore meaningful configuration.
- Preserve every transaction movement, including Bitcoin inputs/outputs; never flatten
  chain facts into fictional transfers. Amounts use checked atomic integers/exact Decimal,
  never floats or SQLite integer sums over u256 text.
- Chains construct payloads, invoke injected signers, validate signatures and broadcast
  exact signed bytes. Validate a non-empty same-chain batch before external effects.
  Preserve Ethereum nonce ordering/accepted prefixes and Solana expiry/ambiguity recovery.
  Solana uses one endpoint and non-repeating RPC requests.
- RPC acceptance means submitted, not confirmed; ambiguous failure does not prove a drop.
  Preserve signed bytes across retries; indexing establishes confirmation.
- Never expose secrets in logs, JSON, Debug or clones; use zeroizing storage. Never use
  funded keys or public broadcast endpoints without explicit authorization.
- Rust 2024, MSRV 1.91, locked dependencies; keep `unsafe_code = "forbid"`. Parameterize
  external values, check arithmetic, avoid unexplained panics, and keep control flow shallow.
- No blocking I/O on async executors, locks across await, or callbacks under internal locks;
  bound concurrency and use Arc/locks only for actual ownership and synchronization needs.
- HTTP owns extraction, authentication, limits and encoding; keep endpoint DTOs beside
  handlers and reusable models with their domain. SDK code knows no Axum or HTTP policy.
- Retry transient network failures with bounded backoff; quarantine terminal failures/panics
  per network until explicit recovery. Application readiness/shutdown policy stays outside
  scanning; preserve pending query and sender recovery behavior during refactors.
- Production Rust limit: 500 counted lines, excluding test-only code; split by responsibility,
  not numbered fragments or include tricks. Comments explain contracts/non-obvious reasons.
  Public APIs need rustdoc; crate READMEs are crate docs with compiling usage examples.

## Workflow and verification

- Inspect git status, owner, callers, manifest and tests before editing; preserve others'
  changes. Make the requested change only; no opportunistic redesign or automatic commits.
- Verify findings against current source and callers; report useful negative findings too.
  Distinguish observed defects, design proposals and unverified assumptions.
- Prove fixes with focused behavioral tests. Keep tests deterministic and offline; run
  shared repository contracts against SQLite and memory. Cover atomic failure/retry,
  birthdays/progress, restart, sparse positions, reorgs, pending reads and network isolation.
- Use the Linter MCP plugin and its software-design skill; preserve `linter.toml`.
  Check the absolute repository root before/after design changes and inspect rule statuses:
  unconfigured or failed analysis is not a pass. Never recreate the old local linter or
  weaken gates/suppressions to hide findings; narrow exceptions need concrete reasons.
- Update affected public-contract docs. Markdown-only edits need path/terminology review
  and `git diff --check`; source/API/storage changes need focused tests and workspace gates:

```sh
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --doc
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps
sh scripts/check-chains.sh
git diff --check
```

Also preserve CI's feature-isolation and OpenAPI checks in `.github/workflows/check.yml`.
Report what ran, remaining failures and limits honestly; a clean linter is not proof of correctness.
