# TASK: Sever `sdk/wallets` from `indexing` — zero imports, permanent guard

## Goal

`sdk/wallets` must not import `indexing` — not in a `use`, not in any path, not
behind a feature. After this task: `wallets → base` only. A check in
`scripts/check-chains.sh` makes the ban permanent. Every gate stays green at
each step.

## Why

Wallets is the factory collection, wallet capabilities, and batch
orchestration. It has no business knowing about the indexing engine or its
storage. Today it does — 31 references across 7 files — because `Holder` was
born in indexing, `Composition` was built as the indexer-assembly crate, 040
moved the reads but not the wiring, and re-exports made it look
self-contained.

## Ground truth (verify against the tree before editing)

- `sdk/wallets/src/composition.rs` — 11 refs: `Balances`, `History`,
  `Index`, `Indexing`, `Router`, `Schedule`, `Schedules`, repository wiring.
- `sdk/wallets/src/error.rs` — 10 refs: `From<indexing::IndexError> for
  WalletError` and helpers.
- `sdk/wallets/src/wallets.rs` — 4: re-exports `Holder`, `Page`, `Subject`,
  `Address`, `Network`.
- `sdk/wallets/src/factory.rs`, `wallet.rs`, `queue.rs` — `Holder`.
- `sdk/wallets/src/testing.rs` — `AssetId` (dev-only).

## Constraints

- Behavior preserved. This is relocation and import surgery, not redesign.
- Pre-release rules: replace directly, no compatibility aliases, no legacy
  readers. One definition moves; old paths update.
- The `Type::from_params(params)` pattern survives any moved constructor.
- 1000-line production cap per file; tests don't count.
- `wallets` must not import concrete chains (already true — keep it).
- No new workspace dependencies except `sdk/config → wallets` (added below).

## Steps, in order. Green gates after each.

1. **Move `Holder` and `Subject` to `base`.**
   - New `sdk/base/src/holder.rs`: the `Holder` trait and `Subject` struct,
     unchanged, on `base::{Address, AssetId}`.
   - `sdk/base/src/lib.rs`: `mod holder; pub use holder::{Holder, Subject};`
   - `sdk/indexing/src/holder.rs`: delete; `indexing/src/lib.rs` re-exports
     `pub use base::{Holder, Subject};` (definition moved, not duplicated).
   - `sdk/wallets/src/wallet.rs`, `factory.rs`, `queue.rs`: `Holder` bound
     from `base`. `impl base::Holder for Wallet` unchanged in shape.

2. **Drop wallets' re-exports.**
   - `sdk/wallets/src/lib.rs`: remove `pub use indexing::{Holder, Page,
     Subject};` and any base-type re-exports (`Address`, `Network`).
   - Grep the workspace for `wallets::{Holder, Page, Subject, Address,
     Network}` and repoint each consumer: base types from `base`, `Page` and
     read traits from `indexing`. Files expected: `apps/server/src/**`,
     `examples/**`, `sdk/chains/*` (witness/import paths), tests.

3. **Split `Composition`.**
   - New `sdk/config/src/composition.rs`: move `Connection`,
     `ComposedChain`, `Composition`, `verify_then_build`, and the
     schedule/index wiring, unchanged in behavior. This is config's existing
     "chains register themselves" pattern extended to full composition.
   - `sdk/config/Cargo.toml`: add `wallets = { path = "../wallets" }`.
     Check for cycles: required direction is `config → wallets + indexing`,
     `wallets → base`. No cycle is possible; verify with `cargo check`.
   - `sdk/wallets/src/composition.rs`: delete. Wallets keeps `Wallets`,
     `factory.rs`, `sender.rs`, `queue.rs`, `pause.rs`, `submissions.rs`,
     `wallet.rs`, `error.rs`, `testing.rs` — zero indexing references.
   - Update `examples/**` and `apps/server` to `config::Composition` (or the
     new path) — they are the consumers.

4. **`error.rs`.** Delete `impl From<indexing::IndexError> for WalletError`.
   Grep for its call sites (server layer); each maps explicitly to its HTTP
   error there. `WalletError` kinds unchanged.

5. **`testing.rs`.** `AssetId` from `base`.

6. **`sdk/wallets/Cargo.toml`.** Remove the `indexing` dependency. `cargo
   check -p wallets` compiles with no indexing in its tree — prove it.

7. **Docs.** `AGENTS.md` ownership rows: `wallets` loses nothing from its
   description but gains "imports only `base`"; `sdk/config` gains
   "composition of factories, indexers, and read handles over registered
   chains". `sdk/wallets/README.md`: the Composition section moves to
   `sdk/config/README.md`. `ARCHITECTURE.md` dependency lines updated.

8. **The guard.** `scripts/check-chains.sh`: add a numbered check in the
   style of checks 8 and 12 — no file under `sdk/wallets/src` may contain
   `indexing::` or `use indexing`. It runs in the eight gates automatically
   (the script is a gate). Confirm it fails if you re-add one import
   locally, then stays green.

9. **Gates** (all must pass, report honestly):
   `cargo fmt --all -- --check`; `cargo check --locked --workspace
   --all-targets`; `cargo test --locked --workspace`; `cargo clippy --locked
   --workspace --all-targets --all-features -- -D warnings`; `cargo test
   --locked --workspace --doc`; `RUSTDOCFLAGS="-D warnings" cargo doc
   --locked --workspace --no-deps`; `sh scripts/check-chains.sh`; `git diff
   --check`. Plus the Linter MCP `check` on the absolute repository root.

## Done means

- `grep -rn "indexing" sdk/wallets/src` returns nothing.
- `cargo tree -p wallets` shows `base` and below only.
- `scripts/check-chains.sh` enforces the ban.
- All nine gates green. The six examples compile (and run where a node is
  configured), per AGENTS.

## Report

What moved where, per step. Gate results. Anything you could not verify.
