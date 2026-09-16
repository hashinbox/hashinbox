# Feature validation

This file records evidence, not aspirations. The project is in active design;
an implementation is considered present only when the cited source and tests
exist in the current workspace.

## Implemented reusable capabilities

| Capability | Owner | Evidence |
|---|---|---|
| Exact chain/network/asset metadata and decimal amounts | `sdk/chains/base` | focused crate tests |
| Small signing, transaction snapshot, and ambiguity-carrier contracts | `sdk/chains/base` | focused crate tests prove ordinary errors remain ID-free, provider prose supplies no ID, and explicit typed ambiguity preserves the canonical ID |
| Bitcoin addresses, RPC, UTXO transactions, signing, indexing translation | `sdk/chains/bitcoin` | chain unit tests and deterministic stack test |
| Local Bitcoin/Ethereum key generation and operational wallet attachment | `sdk/chains/bitcoin`, `sdk/chains/ethereum` | local `KeyPair` tests cover address derivation, formatting, invalid secrets, and non-debuggable secrets; provider tests prove attachment preserves address/signing identity without I/O and rejects invalid configuration |
| Ethereum native/allowlisted ERC-20 balances, typed transfers, EIP-1559 signing, sender-keyed nonce coordination, exact-envelope ambiguity, and indexing translation | `sdk/chains/ethereum` | 81 chain unit tests, external adapter test, and deterministic ETH/USDC stack tests |
| Exact wallet history mapping, transaction-ambiguity preservation, truthful batch failure metadata, and bounded batch admission | `sdk/wallets` | history and conversion tests, four scoped `SendError` tests, direct 0/1/50/51 boundary tests, and a four-case competing-error table proving authored itemwise amount, lookup, and family precedence before sender invocation |
| Native block coordinates and sparse synchronization | `sdk/chains/base`, `sdk/indexing` | every `BlockRef` carries native position, produced height, hash, atomic parent, and timestamp; deterministic fixtures prove sparse `100 -> 103 -> 107` traversal, strict height increments, skipped birthdays, prefix restart, retained reorg, and `ReorgTooDeep` |
| Atomic indexing persistence | `sdk/indexing/redb` | complete-coordinate repository tests cover add/remove/reopen and legacy height-only record rejection, including pre-repository duplicate-output rejection |
| Position-aware PostgreSQL indexing repository | `sdk/indexing/postgres` | scope-bound `Blocks`, `Transactions`, `Outputs`, reusable registry, read-only configured-schema validation, typed zero-pool rejection, address-qualified spends, duplicate-output rejection, serialized add/remove, checkpoint-stable pages, sparse-coordinate add/remove/reopen, and one-pool multi-scope isolation; initializer and repository contracts run against owned schemas on pinned PostgreSQL 18.6 |
| Canonical fresh-schema initializer | `sdk/indexing/postgres/migrations`, deployment documentation | one checksum-locked `0001_init.sql` directly creates the final catalog without upgrade/backfill DDL; owned contracts verify final coordinates and constraints, exact catalog shape, registry preservation, and refusal to replay over an existing schema |
| Atomic runtime wallet/filter admission | `sdk/indexing`, `sdk/wallets` | one coordinator per scope serializes commit and publication permits without holding mutex guards across `.await`; deterministic coordinator and real-wallet fixtures prove both orderings, revision invalidation, cancellation recovery, checkpoint reload, and successor birthdays |
| Reusable wallet registry and restoration ownership | `sdk/wallets`, `sdk/indexing`, `sdk/indexing/postgres` | `Registry`, `RegisteredAddress`, and `Wallets::adopt`/`restore` remain reusable SDK capabilities; owned initializer and registry tests preserve the physical `payment_wallets` schema/content and its existing column mapping |
| Solana architecture ownership | `lint.toml`, `packages/design-lint` | `chain-solana` is mapped to an exact package/base/indexing/wallets layer consumed only by application and acceptance; `solana`/`sol` vocabulary is restricted to `apps/` and `sdk/chains/solana/`, with two reasoned line-local Alloy exceptions in Ethereum; positive/negative policy tests and the repository policy check pass |
| Native Solana chain and runtime composition | `sdk/chains/solana`, `apps/api`, `sdk/wallets`, `sdk/indexing`, `sdk/indexing/postgres` | Canonical `SOL` metadata, native-only `AssetKind`/`WalletConfig`, Base58 values, checked lamports, Ed25519 provider/wallet adapters, exact finalized balance/history, immutable System-plus-Memo envelopes, ambiguity reconciliation, bounded sparse finalized-slot indexing, public native-SOL routes/OpenAPI, closed environment-backed configuration, identity/Memo-before-storage ordering, one central pool, tracked readiness/submission tasks, and ordered ambiguity-preserving shutdown are implemented. Focused doubles and owned PostgreSQL 18.6 contracts pass. The exact Agave end-to-end run remains unavailable until the checksum-pinned archive is present. |
| Generic JSON-RPC/HTTP/crypto/storage mechanics | `packages/*` | package tests |

## Remaining system evidence

| Evidence | Current boundary |
|---|---|
| Owned Agave native-SOL stack | `solana_stack` is declared behind the explicit `solana-stack` manual feature and compiles; v3.1.14 commit and platform SHA-256 values, unsupported-platform handling, corruption rejection, isolated validator/PostgreSQL resources, the end-to-end scenario, and a retained-rollback/restart refill rehearsal are checked in. The real scenario is not claimed because the exact archive could not be acquired at a usable transfer rate. |
| CI ownership | No checked-in workflow owns the pinned validator and PostgreSQL 18.6 fixture, so `solana_stack` is a required manual integration target rather than automated CI evidence. |

## Runtime composition release evidence

Design-lint adoption on 2026-09-03 combines the original eleven SDK rules with
seventeen implemented reusable rules from local Husklet. Thirteen adopted
rules are enabled; the default scan has zero errors and 116 advisory warnings.
Four additional error rules remain explicit-review selections with 287 source
candidates, rather than being downgraded or suppressed. The SDK business source
and accepted design examples were not changed. All 198 linter tests and the full
workspace suite pass; formatting, workspace checks, strict Clippy, documentation,
and the linter MSRV check also pass. See
[`packages/design-lint/ADOPTION.md`](../packages/design-lint/ADOPTION.md) for
current per-rule status, test evidence, and the full review command.

Historical workspace evidence below predates that adoption; its zero-case and
25-test linter counts are not the current linter status.

Fresh Rust 1.91 evidence recorded on 2026-08-30:

- formatting, locked workspace all-target check, strict all-feature Clippy,
  no-dependency documentation, `git diff --check`, and the design-lint policy
  all pass;
- the complete locked workspace test suite passes with zero failures, including
  117 Solana tests, 81 Ethereum tests, 18 application acceptance tests, five
  runtime-loop tests, and the owned PostgreSQL 18.6 migration and 25-case
  repository contracts;
- design-lint's 25 unit tests pass, case generation produces zero cases, and
  the repository policy reports zero findings in every category;
- the explicit `solana_stack` checksum-manifest and corruption-rejection tests
  pass; and
- the real `solana_stack` scenario was invoked, refused the incomplete archive
  at the checksum gate, and executed no validator binary. Acquisition of the
  exact pinned archive is therefore unavailable external evidence, not a
  passing, skipped, or code-level result.

The final boundary audit confirms that `chain-solana` is consumed only by
`payment-api`, its direct workspace dependencies point to base, indexing,
wallets, and generic JSON-RPC, and existing Bitcoin/Ethereum tests pass without
depending on Solana internals. Application startup contains no DDL, the system
target uses loopback endpoints and fixed unfunded test seeds only, Solana RPC
configuration is redacted and no-retry, and OpenAPI publishes native SOL
without SPL routes or assets. The later predeployment schema consolidation is
recorded in the current persistence-coordinate gate below.

## Native Solana research closeout evidence

- Exact-toolchain checks used `rustc 1.91.0 (f8297e351 2025-10-28)` and
  `cargo 1.91.0 (ea2d97820 2025-10-10)`.
- A scratch-only graph combined the current workspace with the exact modular
  Solana dependency family. Its lockfile SHA-256 is
  `5d578ca06eb117006b5dd518220d741963a1036091b7c756d40e17eb05bfe060`.
- The combined graph contains 472 packages: 367 declare `rust-version`, 105
  omit it, and zero declare a version above Rust 1.91. The current graph keeps
  `alloy-consensus`, `alloy-eips`, and `alloy-rpc-types-eth` 1.8.3;
  `alloy-primitives` and `alloy-sol-types` 1.6.1; and redb 4.2.0.
- The exact legacy System-transfer-plus-Memo-v3 modular fixture passed 1/1.
  Locked offline workspace all-target check and strict workspace Clippy passed,
  as did 163 focused Ethereum, redb, indexing, runtime, wallet, and API tests.
- The proof used no public RPC and made no repository manifest or lockfile
  change. It proves dependency/API feasibility, not a Solana runtime.
- Repository integration now resolves 547 locked packages on Rust 1.91, exactly
  43 more than the immediately preceding 504-package lockfile. The integrated
  lockfile SHA-256 is
  `13965c12958ceb9ac5b2086e3c3dc61a55a82e812e0c995fe62fee5c1b449071`;
  the fixed direct Solana versions match the research set, the established
  Alloy/redb versions remain unchanged, and no monolithic Solana client or SDK
  was added. Locked workspace all-target compilation, strict workspace Clippy,
  and the complete workspace suite pass against the integrated graph.
- [PostgreSQL Schema Baseline](POSTGRESQL_SCHEMA_BASELINE.md) records the single
  predeployment initializer, final catalog, ownership, fresh-only deployment
  boundary, registry sentinel, and startup-validator evidence. No retained
  database migration has run.

## Current application validation

The approved architecture has one `apps/api` process. The previous wallet,
indexer, payment, deposit, accounting, and collection service surfaces have
been removed.

The wallet API integration binary contains eighteen tests. Its process-level
cases start the real application binary against loopback Bitcoin and Ethereum
RPC doubles and one owned temporary PostgreSQL database/schema:

| Behavior | Current evidence |
|---|---|
| Wallet generation/index selection | authenticated BTC, ETH, and USDC API creation with caller-owned address selection; ETH and USDC generation yields distinct addresses |
| Balance and transaction reads | incoming native and allowlisted-USDC node transactions indexed and returned through each generated wallet's selected-asset view |
| Single transfer | BTC, ETH, and USDC partial sends, node inclusion/confirmation, outgoing history, and reduced selected-asset balance |
| Bitcoin batch | two compatible transfers become one broadcast transaction and one ID, then appear in indexed history |
| Ethereum batch | two transfers submit as two IDs in request order, both become indexed, and balance reduces |
| Lifecycle | indexes become ready before serving and both runtimes shut down cleanly |
| Address selection | one per-scope admission boundary coordinates immutable filter plans, checkpoint commits, and runtime wallet publication without holding locks across async I/O |
| No internal transport | dependency/source audit shows no indexing HTTP adapter or second application |
| Restart | configured BTC and ETH wallets reopen the same databases and retain indexed history |
| Reorg | repository-level retained-reorg evidence and the combined generated-wallet Bitcoin/Ethereum canonical-reorg acceptance case pass |
| Multi-source Bitcoin batch | two wallets fund one transaction; each input witness carries its owner's public key |
| Ethereum accepted prefix | an outcome-ambiguous second submission returns HTTP 503 with the first transaction ID and `failed_index = 1` |
| Ethereum nonce coordination | a repeated-source `[A, B, A]` batch submits nonces `[A:0, B:0, A:1]` in request order |
| Ethereum whole-batch preflight | cumulative native overspend fails at the first threshold-crossing input with zero broadcasts |
| Ethereum ambiguous submission | coordinator fault-injection tests retain unknown exact envelopes across retryable failure and cancellation, then reconcile and replay byte-identical transactions |
| Mixed-chain preflight | BTC/ETH batch is rejected before either RPC double observes a broadcast |
| Mixed-asset preflight | ETH/USDC batch is rejected before the shared Ethereum RPC double observes a broadcast |
| ERC-20 fee presentation | USDC history retains the selected token movement with native ETH fee metadata |

## Explicitly absent

- deposit records or address-allocation workflows;
- accounting journals or user credits;
- collection planners, reservations, jobs, sweeps, gas sponsorship, or automatic token-wallet ETH top-up;
- outgoing payment state machines;
- durable native SOL request-idempotency, outgoing-envelope, or cross-process
  source-guard storage (ADR-0025 selects the implemented process-local model);
- durable or cross-process outgoing-envelope coordination; the Ethereum
  coordinator is process-local and requires one active writer per EOA;
- deposit/accounting/collection semantics around ordinary wallet sends;
- separate wallet or indexer processes;
- an indexing HTTP client;
- runtime/public migration commands or `V1`/`V2` compatibility DTOs; canonical
  ordered PostgreSQL migration scripts are deployment inputs, not a runtime API;
- hardware wallets, remote signers, HSM/KMS integration, or production custody;
- HA, multi-process database ownership, or production deployment claims.

## Known validation blockers

The 2026-09-16 wallet-refactor workspace run reaches the unchanged PostgreSQL
repository suite and fails 25 of its 29 tests. The initializer creates names
such as `checkpoint` and `payment_wallets`, while runtime queries and schema
validation expect `payments_checkpoint` and `payments_wallet`. The catalog
test also embeds a literal backslash in raw SQL and reports SQLSTATE `42601`.
These source defects are present in the branch's base commit and remain outside
the local-wallet refactor. The three initializer contract tests pass.
Finishing the other workspace tests with `--exclude indexing-postgres
--no-fail-fast` also exposes ten API system-test failures at startup against
that incompatible schema; the other twelve API integration cases pass.
Retained migration execution remains a separately authorized deployment action.
The pinned Agave archive is the current external system-evidence blocker.

## Local wallet separation evidence

Recorded on 2026-09-16:

- `cargo test --locked -p chain-bitcoin` passes 161 unit tests and one
  secret-debugging compile-fail documentation test.
- `cargo test --locked -p chain-ethereum` passes 159 unit tests, three integration
  tests, and one secret-debugging compile-fail documentation test.
- Both `cargo run --locked -p payment-examples --bin bitcoin_wallet` and
  `cargo run --locked -p payment-examples --bin ethereum_wallet` ran without a
  node or database and printed only the generated public address at this step.
  Their secrets remained in memory and were dropped on exit. These offline
  snippets are now preserved as commented Sample 1 inside each `main`.
- The node/indexer examples remain available as `bitcoin_indexer` and
  `ethereum_indexer`. They compile; this change does not claim fresh real-node
  execution of those examples.
- Workspace formatting, locked all-target compilation, strict all-target and
  all-feature Clippy, no-dependency documentation, and diff checks pass.
- The Linter MCP check completes all 39 configured rules; 17 are unconfigured.
  Its only unsuppressed finding is the existing `rust/layers` mapping gap for
  `payment-examples`. No policy configuration changed.
- Full workspace test success is blocked by the PostgreSQL defects above.

## RPC configuration separation evidence

Recorded on 2026-09-16:

- Bitcoin and Ethereum construct `json_rpc::Config`, `Http`, and their native
  RPC clients separately from `IndexerSettings`. Application JSON configuration
  and transport defaults remain unchanged.
- `cargo test --locked -p chain-bitcoin` passes 164 unit tests and one
  documentation test. RPC doubles verify readiness before indexer construction,
  network mismatch rejection, and no additional calls during `build`.
- `cargo test --locked -p chain-ethereum` passes 163 unit tests, three integration
  tests, and one documentation test. RPC doubles verify chain ID/genesis,
  source identity checks, and no additional calls during `build`.
- `cargo test --locked -p payment-api --bin payment-api` passes all 36 tests,
  including independent transport/indexing configuration and the existing
  verification-before-storage startup checks.
- Workspace formatting, locked all-target compilation, strict all-target and
  all-feature Clippy, no-dependency documentation, and diff checks pass.
- Updated Bitcoin, Ethereum, and ERC-20 indexing examples compile. They were
  not executed against live nodes during this refactor.
- `cargo test --locked --workspace --no-fail-fast` completes every target and
  reproduces the known blockers above: 25 PostgreSQL repository failures and
  ten API startup failures. All other targets pass; the initializer contracts
  pass all three tests.
- The fresh Linter MCP check completes all 39 configured rules; 17 remain
  unconfigured. Its only unsuppressed finding is the unchanged `rust/layers`
  mapping gap for `payment-examples`; no policy or suppressions changed.

The subsequent example update puts the connected RPC/indexer/provider setup
inside `bitcoin-wallet.rs` and `ethereum-wallet.rs` as active Sample 2. Each
verifies its local node before opening redb, attaches a locally generated key,
prints the public address, and exits without starting synchronization. The
commented Sample 1 remains the offline alternative. The separate
`bitcoin_indexer` and `ethereum_indexer` binaries retain the complete watching
loops. The active wallet samples now require their documented local nodes and
were not executed against them during this update. Locked all-target compilation
and strict all-target/all-feature Clippy pass for `payment-examples`, as do
workspace formatting and diff checks. A fresh MCP check retains the same
39 completed rules, 17 unconfigured rules, and existing examples-layer finding.

## Optional genesis and RPC defaults evidence

Recorded on 2026-09-16 after the sample simplification:

- Bitcoin `RpcClient::connect(transport, network)` skips startup genesis lookup
  while retaining network and readiness checks. `connect_with` accepts an
  explicit optional pin. `cargo test --locked -p chain-bitcoin` passes 169 unit
  tests and one documentation test, including default-call ordering, explicit
  mismatch, malformed pin before I/O, and source/indexer policy consistency.
- Ethereum `IndexerSettings::new(network)` defaults to `genesis_hash: None`.
  Chain ID remains mandatory; an explicit hash enables strict genesis parsing
  and comparison. `cargo test --locked -p chain-ethereum` passes 167 unit, three
  integration, and one documentation test.
- `cargo test --locked -p json-rpc` passes 27 tests. The new default request
  and batch tests failed with the former no-retry default, then passed with
  three endpoint rounds. Explicit no-retry and first-endpoint-only
  `request_once` are covered under the new default.
- `cargo test --locked -p payment-api --bin payment-api` passes 38 tests.
  Missing/null Bitcoin and Ethereum genesis configuration disables pinning;
  explicit valid values reach the SDK and invalid supplied values are rejected.
- Both wallet mains retain their commented offline samples. All five Bitcoin,
  Ethereum, and ERC-20 examples use default retry policy and omit genesis pins.
  They compile; no live-node execution was performed for this update.
- Workspace formatting, locked all-target compilation, strict all-target and
  all-feature Clippy, no-dependency documentation, and diff checks pass.
- The fresh MCP check completes 39 configured rules; 17 remain unconfigured.
  The only unsuppressed finding is the existing examples-layer mapping gap.
- `cargo test --locked --workspace --no-fail-fast` reproduces the existing
  25 PostgreSQL repository failures and ten API system-test startup failures.
  The schema-name mismatch and SQLSTATE `42601` remain unchanged; the three
  initializer contracts pass. These failures are outside this update.

## Existing wallet insertion evidence

Recorded on 2026-09-16:

- `Wallets::insert` retains an already attached wallet and its explicit
  native-position birthday after provider validation. Bitcoin and Ethereum
  examples show local generation, attachment, family registration, insertion,
  and public-address output. Each sample now has a named function in
  `examples/bitcoin.rs` or `examples/ethereum.rs`, with all three calls commented
  in `main` for opt-in execution. Both compile. These two binaries replace the
  earlier wallet, ERC-20, Solana, and standalone indexer examples described in
  the historical evidence above.
  No live-node example execution was performed for this update.
- Focused Bitcoin tests pass 176 unit tests and one documentation test;
  Ethereum passes 173 unit tests, three integration tests, and one documentation
  test. Admission rejects mismatched scope, address/asset configuration, and
  operational dependencies before publishing a wallet or filter. Tests also
  reject ETH/USDC mixups for the same EOA address.
- Focused wallet tests pass 36 unit tests and two documentation tests. They
  prove exact instance retention, explicit birthday handling, no registry I/O,
  duplicate-ID preservation, rejection without publication, and that insertion
  cannot be called through a shared `Arc<Wallets>`.
- The required provider validation contract is also implemented by Solana;
  160 unit tests and one documentation test pass with local loopback networking
  enabled. Solana validation binds the wallet to its existing batch signer
  registry; no local Solana key/attachment API was added.
- The new insertion test first failed because the public method was absent,
  then passed after implementation. Existing batch orchestration moved intact
  into a private collection child module; its regression tests remain passing.
- Workspace formatting, locked all-target compilation, strict all-target and
  all-feature Clippy, no-dependency documentation, and diff checks pass.
  The final MCP check completes all 39 configured rules; 17 are unconfigured.
  Its only finding is the pre-existing examples-layer mapping gap.
- `cargo test --locked --workspace --no-fail-fast` completes with the same
  25 PostgreSQL repository failures and ten API system-test startup failures
  recorded above. The schema incompatibility and SQLSTATE `42601` remain
  outside this change; the three initializer contract tests pass.

## Final gates

Before a handoff, record fresh results for:

```text
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
git diff --check
```

Run the Linter MCP `check` tool with the absolute repository root as well.
`linter.toml` is the current design policy; local design-lint commands in older
evidence above are historical and are no longer available. Record both findings
and configured-rule completion; unconfigured rules are not passes.

If a gate fails, document the exact failure rather than weakening a lint or
describing the workspace as complete.

Historical persistence-coordinate gate recorded on 2026-09-01: the uninterrupted
locked workspace suite passes, including owned PostgreSQL 18 initializer
contracts 3/3, repository contracts 25/25, and application acceptance 18/18.
Formatting, locked workspace all-target compilation, strict
all-target/all-feature Clippy, no-deps documentation, design-lint, and diff
checks pass. The owned proof locks the single initializer checksum and verifies
the final catalog, constraints, scope isolation, registry sentinel, and replay
rejection; no retained database was contacted or migrated.
