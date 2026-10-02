# 040: Queries are indexing's — the reads leave wallets

> "i don't need wallets to have balance or history that is none of it business"

Status: **proposal.** The pending-inclusive read layer — `History`, `Balances`,
the spendable-outputs read, `PendingJoin`, and their read models — moves from
`sdk/wallets` into `sdk/indexing`, beside the repository vocabulary it
composes. Wallets keeps factories, wallet capabilities, and batch
orchestration; it loses every read. This supersedes the landing spot chosen
in `035` (the composition still does not live on the indexer's front door;
it moves from the pass-through consumer to the owner of both halves).

---

## 0. Why

- **Both halves are indexing's.** The settled half is the repository's own
  answer — `BalanceStore`, `ConfirmedHistory`, `Fold`, the page models, all
  in `indexing/src/storage/`. The pending half is `indexing::Pending`, the
  blocks the engine holds above the confirmed line. A join of two
  indexing-owned halves is indexing's composition.
- **Wallets owns none of it.** What `wallets/src/balance.rs` and
  `history.rs` add is read-time and nothing else: a status, a confirmation
  count, the pending fold, the two-folds doctrine. There is no wallet
  knowledge in any of it — no key, no factory, no send. A fold over indexed
  movements needs no key; the file that says so should not live in the
  wallet crate.
- **The dependency direction says it already.** Every chain crate builds
  the join inside its own `indexer()` constructor, and does so over
  `indexing` types. `apps/server` holds the traits through wallets only
  because wallets re-exported them. The imports point at the owner.
- **The original requirement said it first.** "History, balance and output
  queries belong to repositories." The reads were parked in wallets because
  that was the only consumer when the boundary was drawn. The consumer set
  is now the whole SDK; the parking spot is overdue.

## 1. What moves

From `sdk/wallets/src/` to `sdk/indexing/src/storage/`:

| From | To | Contents |
|---|---|---|
| `pending.rs` (486 lines) | `storage/join.rs` | `PendingJoin` and its three composed impls |
| `history.rs` | merge into `storage/history.rs` | `History` trait, `TransactionStatus`, `ObservedTransaction`, `TransactionPage` — beside `HistoryQuery`, `Page`, `ConfirmedHistory` |
| `balance.rs` | merge into `storage/balance.rs` | `Balances` trait and its `PendingJoin` impl — beside `Fold`, `Balance`, `BalanceStore` |
| `spendable.rs` (385 lines) | `storage/spendable.rs` | the spendable-output read and its cursor machinery — beside `OutputStore` |

Re-exported at the `indexing` root, next to the other query vocabulary:
`History`, `Balances`, `PendingJoin`, `TransactionStatus`,
`ObservedTransaction`, `TransactionPage`, and the spendable read.

**Wallets after the move:** `factory.rs`, `wallet.rs`, `wallets.rs`,
`sender.rs`, `submissions.rs`, `error.rs`, `composition.rs` (minus its read
handles), `lib.rs`. The read modules and their re-exports (`Holder`, `Page`,
`Subject`) leave; consumers import them from `indexing`, where the types
live.

**The per-network routers.** `Composition`'s `Histories`/`Folds` wrappers
move with the join as one network-routed reader in `storage/join.rs`, and
the surviving unification from `039` §1.2 item 1 (shared `owner` lookup)
lands there — one router type, three capabilities, instead of two copies.

## 2. Soundness catches

- **The traits do not become ports.** `Repository` does not grow them. A
  store that answers no query stays a legal store, and
  `tests/minimum_contract.rs` must keep proving it. The join stays a
  composed type: `PendingJoin::new(repository, pending)` — a constructor
  argument, never a registration.
- **The engine front door stays closed.** No `Indexer::history()`, no
  `Indexer::balance()`. The engine exposes exactly one thing for this:
  its `Pending` handle. Nothing in the engine imports the join, so the
  module direction stays acyclic in meaning: the join is a leaf over
  `{storage ports, Pending}`.
- **The two-folds doctrine moves as indexing's read policy.** The settled
  fold nets a self-transfer; the pending fold reports it as both arriving
  and leaving. That paragraph moves with the code and stays indexing's
  documentation, not wallets'.
- **No feature or dependency growth.** The join is pure composition over
  ports and `Pending`. It compiles with `default-features = false`; no
  store, no runtime, no new crate edges. `wallets` after the move depends
  on indexing one direction, unchanged.
- **The wire does not change.** `apps/server` maps the same SDK answers to
  the same DTOs. Only import paths move.

## 3. Who changes

- **Chain crates.** `indexer()` constructors build the join from indexing
  types instead of wallets types. Import change; the construction is the
  same. Solana's recovery holds `Arc<dyn History>` — import change only.
- **`apps/server`.** `Payments` fields become `Arc<dyn indexing::History>`
  and `Arc<dyn indexing::Balances>`; the route handlers' imports follow.
- **`wallets`.** The four read files delete; `Composition` sheds its
  history/balance/spendable handles and keeps index, wallets, factories,
  units, schedules. Its README's read sections move to indexing's.
- **Tests.** `wallets/tests/pending_reads.rs` becomes an indexing
  integration test. `service_contract.rs` and the route contract update
  their imports.
- **Examples.** Any example holding a read handle updates its import.

## 4. Work order

One step per change set; every step leaves the workspace green. Indexing
gains before wallets loses, so nothing is ever missing.

1. **`storage/join.rs`.** `PendingJoin` moves into indexing with its impls;
  the engine exposes its `Pending` handle (add the accessor if the chain
  constructors are reaching into internals today). Root re-exports.
2. **History merge.** `History`, `TransactionStatus`, `ObservedTransaction`,
  `TransactionPage` into `storage/history.rs`.
3. **Balance merge.** `Balances` into `storage/balance.rs`.
4. **Spendable.** `spendable.rs` into `storage/spendable.rs` with the root
  re-export. The network-routed reader from §1 lands here, with the shared
  `owner` lookup (`039` §1.2.1) done once, not twice.
5. **Consumers switch.** Chains' constructors, solana recovery, and
  `apps/server` point at indexing.
6. **Wallets loses the reads.** Delete the four files, trim `Composition`
  and `lib.rs`; the workspace compiles with wallets read-free.
7. **Docs.** `AGENTS.md` ownership rows (`wallets` loses "query readers";
  `indexing storage` gains "pending-inclusive reads"); `sdk/indexing/README.md`
  gains the query-layer section; `sdk/wallets/README.md` loses it;
  `ARCHITECTURE.md` crate map.

## 5. Out of scope

- **No new query features.** The reads move as they are.
- **No port growth.** §2's catches stand; nothing here widens what a store
  must implement.
- **No engine API.** The indexer gains no read method; the join is a
  separate value the composition root builds.
- **No wallet API beyond losing the reads.** Factories, capabilities, and
  batch orchestration are untouched.

## 6. Verification

Per step: the eight gates from `AGENTS.md`, plus the Linter MCP `check`.

Behavioral proof for the move itself:

- `wallets/tests/pending_reads.rs` passes at its new home unmodified except
  imports — the semantics moved, not changed.
- `tests/minimum_contract.rs` still passes: a store with no query
  capability whatsoever still indexes; the moved traits asked nothing of
  it.
- The interleaved-commit read tests (a commit landing mid-read reports a
  transaction exactly once) pass against both stores at the new home.
- The full route contract passes with `Payments` holding indexing traits:
  the wire is byte-identical, demonstrated by the committed
  `docs/openapi.json` staying unchanged.
