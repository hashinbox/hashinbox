# Handoff

Written 2026-09-29 for whoever continues this work. Read `AGENTS.md` first; it
governs. The plans are in `docs/design/`, numbered `035` to `038`.

## State in one paragraph

Branch `main`, last commit `0a80d25`. Everything below is **uncommitted working
tree**: about 270 changed paths. The workspace compiles
(`cargo check --locked --workspace --all-targets` is clean). The last complete
run of every gate, taken before the final two agents started, was green. A new
full run was started after those agents were stopped; its result is in the
section "Gates" and must be read before trusting the tree.

**First action:** run the gates in "How to verify", fix what is red, then
commit. Nothing should be built on this tree before it is committed, because
three separate losses of work today came from uncommitted state.

## What is done

| Work | Plan | State |
|---|---|---|
| `apps/api` merged into `apps/server`; one binary, no `lib.rs` | user request | done, verified |
| Query vocabulary moved to `sdk/indexing/src/storage/` | 035 step 2 | done, verified |
| Read joiners deleted; engine exposes pending blocks only | 035 step 2 | done, audited by mutation |
| Storage layout: each folder under `storage/` is one implementation | user request | done, enforced by linter and check 12 |
| First renames (`Birthday`, `NativeBlock`, `ChainTips`, `CommitGate`, ...) | 035 step 3 | done, verified |
| `CannotConnect` split into `Disconnected` and `InvalidBlock` | 035 step 6 | done, verified |
| Contract suite files renamed | 035 step 8 | done, verified |
| One block shape: `Facts<T>` over `base::Block<T>` with its network | 036 | done, verified |
| Stage pairs, shared renames, snapshot DTOs in all three chains | 037 steps 1-4, 6-8 | done, verified |
| Examples self-contained; `examples/local.rs` deleted | user request | done, compiled only |
| `wallets::Submissions` and the readiness waiter moved into the SDK | user request | done, verified |
| Every README cut to purpose, usage, notes | user request | done, doctests pass |
| `scripts/crate-map.py`: generates the crate dependency map | user request | done |

"Verified" means a full gate run passed with that work in the tree. It does not
mean the scenarios were run against live chain nodes; no local node was running
at any point today.

## What was stopped partway

Two agents were stopped by hand. Their edits are in the tree and **no report
exists for either**. Treat everything in this table as unverified.

| Work | Plan | What is in the tree |
|---|---|---|
| `Service` to `Indexer` | 035 §3.1 | `pub struct Indexer<S, I, R>` in `sdk/indexing/src/indexer.rs`; `service.rs` is gone |
| `overlay` to `pending` | 035 §3.2 | module `pending.rs`; the block collection is `Blocks` |
| `Ledger` to `Fold`, `Effect` to `MovementEffect` | 035 §3.1 | both renamed in `storage/balance.rs` |
| `AddressFilter`, `Set` | 035 §3.3, §3.1 | now `ForwardRange` and `WatchRows`; names chosen by the agent, not reviewed |
| `FeeSettlement` moved beside `BlockInterpreter` | finding | now in `sdk/indexing/src/source.rs` |
| `PendingSample::above` takes the store's answer | audit finding | signature is `above<A: Checkpointed>(self, answer: &A)`, with a `compile_fail` doctest |
| Density §4.2, page probe | 035 §4.2 | helper `page` in `storage/history.rs` |
| Density §4.3, block linkage | 035 §4.3 | predicate `extends` in `block.rs` |
| Density §4.4, `canonical_at` check | 035 §4.4 | unclear: one call site left in `scan.rs`, no `require_canonical` method found |
| Density §4.1, completeness rule | 035 §4.1 | probably already satisfied by `Inspection` in `storage/reads.rs`; not confirmed |
| `NetworkHealth` composes `SyncStatus` | 035 §3.4 | done in `health.rs`: the struct holds `status: SyncStatus` |
| Wire status triple | 037 §4 | **not done**: `PendingStatus`, `ConfirmedStatus`, `FailedStatus` are still three structs |

**Known risk:** the renames agent was in the middle of a mutation test when it
was stopped. Its last message was "rerun the output-only mutation to confirm
both stores go red, and also a mutation of `rows()`". A deliberate bug may be
left in `sdk/indexing/src/storage/` or `sdk/indexing/src/watch_set.rs`. A red
test in the output or watch-row suites is the sign. Compare those files against
what the tests expect before anything else.

## What is not started

- **Plan 038, all of it.** Wallet registry with keys encrypted at rest,
  `healthz` with per-RPC statistics, sweep, and the removal of five routes. Two
  agents were launched and stopped within seconds; they wrote nothing.
- **Docs pass.** `ARCHITECTURE.md` still describes the removed submission
  queue. `examples/README.md` still describes `local.rs`.
- **CI.** No step runs `wallets` with `--features submissions`, so five
  registrar tests are skipped there.

## Decisions waiting on the user

1. **`docs/` is mostly deleted in the working tree.** `API.md`, `INDEXING.md`,
   `CHAIN_LAYOUT.md`, `DEFECT_PATTERNS.md`, `findings.md`, `adr/` and
   `reviews/` are gone; git still has them. It is not known who deleted them or
   whether it was deliberate. `AGENTS.md` still tells readers to open four of
   them. Two were put back today because gates need them: `docs/openapi.json`
   (regenerated, identical to the committed copy) and
   `docs/SYSTEM_REQUIREMENTS.md` (restored from git).
2. **`verified_opening_balance` is still in the HTTP completeness enum.**
   Nothing can produce it since `Completeness::VerifiedOpening` was deleted.
   Removing it is a wire change.
3. **A source that lies about block content retries forever.** It is
   `InvalidBlock` with `retryable: true`. Quarantine is a one-word change in
   `IndexError::invalid_answer`.
4. **A panicking send task no longer stops the server.** It used to; now the
   other sends finish and the panic is reported at shutdown.
5. **Solana recovery reads the build slot** in `sender/recovery.rs` lines 111
   and 125, where dispatch reads the simulated floor. Judged safe, but it
   contradicts the comment on `Dispatch`.
6. **Seven linter suppressions were added** in `apps/server/src/api/` for type
   names that repeat their module name. Four are OpenAPI schema names.
7. **Plan 038's open decisions** (§7 of that plan). The defaults were assumed;
   none was confirmed by the user.
8. **`base::Units` is keyed by `AssetId` alone.** Two networks of one chain
   declaring the same token with different decimals would collide. Whether any
   configuration can produce that was not checked.

## Where the plans are wrong about the code

Each of these was established from the source. Do not re-investigate.

| Plan | Claim | What the code shows |
|---|---|---|
| 035 §3.6 | the single-file-directory rule does not apply in `sdk/indexing` | it does, in `check-chains.sh` check 11 and in `linter.toml` |
| 035 §3.6 | five watch files become three by folding | `watches.rs` and `watch_set.rs` total about 700 lines against a 500 cap |
| 035 §3.2 | `tracked_spends` and `unknown_spends` are one concept | they are not; `tracked_spends` was left as it is |
| 035 §1.2 | `num-bigint` leaves the crate | the SQLite store uses it too |
| 035 §6 | no behaviour change to the stores | the stores now refuse unsafe reads themselves; §1.2 requires it |
| 036 | the block file shrinks | `block.rs` grew from 281 to 331 production lines |
| 036 | each chain holds its network beside the raw block | only Bitcoin did; the other two gained the field |
| 037 §1.3 | Bitcoin `Input` and `Spend` are one shape | one shape would let an unsigned transaction hold a coinbase input; kept as two |
| 037 §2 | Bitcoin `Record` holds `Option<PreparedTransaction>` | a record holds its inputs before any transaction exists; kept |
| 037 §1.2 and §5 | contradict each other on Solana wire twins | every twin validates or converts on decode; kept |
| 037 §6 | `Witness` and `BalanceWitness` are in Solana | both are in Ethereum |

## Rules that are enforced mechanically

Breaking one of these fails a gate. They cannot be suppressed.

- 500 counted production lines per file, test-only code excluded. A
  `linter:disable` does not suppress it.
- No directory under `src/` with exactly one `.rs` file. No `#[path]`.
- Under `sdk/indexing/src/storage/`, every folder is one storage
  implementation with its root at `mod.rs`. Files directly in `storage/` are
  what all implementations share.
- Nothing outside `storage/sqlite` and `storage/memory` may import from them
  or name `rusqlite` (`check-chains.sh` check 12).
- Nothing under `apps/server/src/api/` may name a chain crate, `sdk_config` or
  a storage backend (check 8). `apps/server/src/chains.rs` is the only file
  that names a chain.
- `sdk/config` depends on no chain crate. Chains are registered by the
  application.
- Type names are nouns of at most two words and must not repeat their module.
- `Type::from_params(params)`, never `params.thing()`.
- The three chain crates keep the same layout.

## How to verify

```sh
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace --no-fail-fast
cargo test --locked -p wallets --features submissions
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps
sh scripts/check-chains.sh
git diff --check
```

Then the linter, through its MCP `check` tool on the repository root.

The wire format must not have changed:

```sh
cargo run --locked -q -p payment-server -- --openapi | cmp - docs/openapi.json
```

The full test run takes about 25 minutes on this machine. One test is known to
be flaky under load: `server_api::every_framework_rejection_answers_json_over_the_wire`
can hit its 10 second readiness deadline when other cargo jobs are running. It
passes alone.

## Gates

Last complete run, before the final two agents started:

| Gate | Result |
|---|---|
| `cargo test --workspace` | 1224 passed, 1 failed (the OpenAPI comparison, because `docs/openapi.json` was deleted; since regenerated) |
| clippy, fmt, workspace rustdoc, `check-chains.sh`, `git diff --check` | pass |
| linter | one finding, the missing `docs/SYSTEM_REQUIREMENTS.md` (since restored) |
| `--openapi` against the committed copy | byte-identical |

Run after all agents were stopped: **incomplete when this was written.** At 40
of about 50 suites it stood at 1218 passed, 0 failed. Clippy, rustdoc and the
linter had not yet run on the final tree.

## How to run the server

```sh
export PAYMENT_API_TOKEN='replace-me'
cargo run --locked -p payment-server -- ./payments.toml
```

The file's top-level keys (`bind`, `bearer_token_env`, `storage`, `wallets`)
are the server's. Each `[chains.<name>]` table goes to the chain registered
under that name in `apps/server/src/chains.rs`. Each chain verifies its node
before storage is opened, so a wrong node leaves no database file. This was
confirmed by running the binary: a missing `bind` is refused, an unregistered
section is refused by name, and with no node running the server stops at
connect without creating the file. A full start to serving was not tested.

## Advice for the next session

- Commit first.
- Work in one checkout with one writer. Seven agents sharing one build
  directory made every build wait on the others and cost hours.
- Do not trust a green test run as proof that a guarantee held. Twice today a
  test passed while the rule it named was broken, because its fixture could not
  express the failure. Break the rule on purpose and watch the test go red.
- When an agent or a tool reports "all gates pass", run them again yourself.
