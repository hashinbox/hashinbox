# 086: Test support: one `testing/` module per package

Status: proposed (doc only). Builds on 074 (mock chains and the server suite):
074 covers the mock nodes; this doc covers the other test doubles and fixtures.

Each test file writes its own fakes today. The workspace has ten hand-written
`Chained` chains, eleven hand-written store impls, seven wallet or broadcaster
fakes, three tip-only info clients and five temp-database helpers, and the
same queue store rules are tested twice. Test support should live in the package
that owns the trait, behind its `testing` feature, written once. Test files
then keep only their scenarios and assertions.

## Principles

- **What goes in `src/testing/`:** doubles of the package's own traits;
  fixtures built from its own types; and builders of its own composites (a
  real `Indexer` over doubles). This is what a second test file, or another
  package's tests, needs.
- **What stays in a test file (or `tests/common`, `tests/support`):**
  assertions, setup used by one file, and harnesses only that package's tests
  use (trigger's `World`, the server's `App`).
- **One owner per double.** The package that defines a trait owns its
  doubles. A test never implements a foreign trait by hand when the owner has
  a double. If the double lacks a knob, the knob goes into the owner.
- **Gating:** a `testing` cargo feature, off by default (as `rpc`, `base`,
  `chain-ethereum` and `trigger` already do). A crate turns it on for its own
  tests through the self dev-dependency (`base = { path = ".", features =
  ["testing"] }`), which also reaches its `#[cfg(test)]` units. Dependents
  turn it on in `[dev-dependencies]` only. A crate's `testing` turns on its
  dependencies' (`chain → base → rpc`). A CI step checks that a release build
  enables no `testing` feature (e.g. `cargo tree -e normal -f '{p} {f}'` per
  binary).
- **Deterministic and inspectable:** no sockets, wall clock or randomness.
  Answers are scripted (a queue or a map). Calls are recorded (`sent()`,
  `reads()`, `asked()`). A call nothing scripted fails with an error that
  names it; the double never invents an answer.
- **Only the rules the SDK relies on.** A double implements a trait's rules
  only as far as SDK logic depends on them. Where the rule itself is under
  test (SQL), the server tests it, not the double.
- **Tests for doubles only when they carry rules:** memory stores, the mock
  chain, the tasks contract. Scripted answers need none.
- **Names say what it is** (`Served`, `Memory`, `Broadcasts`), never
  `Fake`/`Mock`, and never the trait's own name.

## Per package

### `packages/rpc`: done, no change

| Holds | Purpose |
| --- | --- |
| `Script`, `Reply` | Scripted node: replies in order, asserts every call, records what was asked |
| `Faults`, `Switch` | Makes any client fail on demand: down, delay, fail next, rate limit |

Stays: the loopback HTTP node in `tests/http.rs`, which only `Http` needs.

### `packages/tasks`: new `testing` feature

| Holds | Purpose | From | Replaces |
| --- | --- | --- | --- |
| `store_contract!(make)` | Every `Store` rule, one test per rule, for any store `make` builds | `tests/store.rs` (9 tests, 5 helpers) | The 6 restated rules and 5 identical helpers in `apps/server/src/usecase/tasks.rs` tests (~200 lines) |
| `claim(store, kind, limit, now)` | Claims jobs as the executor would (`Claim` with a 60 s lease) | `apps/server/tests/{send,check,routes}.rs` | 5 `Claim { … }` literals |
| `run_once(worker, store, id) -> Ack`, `end(store, id, ack)` | Runs one job by hand and stores how it ended | `send.rs` `Fixture::{run_job,end}`, `check.rs` `check` | 2 copies of the `Context::new` + `run` + `settle` dance |

- `src/storage.rs` still links to `crate::testing::store_contract`. That link
  has dangled since 6cc2939 removed the suite; this brings it back.
- `MemoryStore` stays public (see Open questions).
- No manual clock: `Claim::now` and `Job::settle(…, now)` already take the
  time, and the executor runs on real time with 5 ms ticks.
- Its own tests: the contract is a test suite. It runs on `MemoryStore` in
  `tasks` and on SQLite in the server, which keeps only `pruning_*`.

### `sdk/base`: `base::testing` gains a second half (core crate: needs approval)

The mock chain stays as it is (`Chain<M>`, `Node<M>`, `TestChain`, `Caps`,
`Topology`, `Clock`, `Recording`; see 074). New:

| Holds | Doubles | From | Replaces |
| --- | --- | --- | --- |
| `Memory` | `BlockStore`, `WalletIndexStore`, `TransactionIndexStore`, `InputStore`, `WalletStore`, all on one shared state; `storage(holdings)` returns a `Storage` | `indexer.rs` tests `Fake`; `ethereum/tests/chain.rs` `Fresh`; `ethereum/tests/wallet.rs` `Pending`; `bitcoin/tests/wallet.rs` `Store` | 9 store impls in 4 files (`Pending` alone forwards 7 methods to `NoTransactionIndexes` to override one) |
| `Served` | `Chained` + `Dated` over a list of blocks: tip is the last, a finality lag, a `line`, skipped positions, block times, `prune(below)` (`NotServed`), `reads()` | base: `indexer.rs` `Fake`, `block.rs` `Even`/`Pruned`, `index/wallet.rs` `Lined`; server: `tests/mod.rs` `Empty`/`Days`/`Lined`, `storage.rs` `Served`/`Noted`/`Pruned` | 10 chains, 3 of them `Dated` |
| `Held` | `Balances`: a holding per address; `None` means not answerable yet, unset means the read fails | `storage.rs` `Held` | (moves) |
| `Confirms` | `Confirmer`: a verdict per txid, same rules | `storage.rs` `Confirms` | (moves) |
| `Holders` | `Holdings`: `all()`, `of([addresses])`; records what was asked | `ethereum/tests/wallet.rs` `Held`, `solana/tests/wallet.rs` `HeldByAll` | 2 |
| `Broadcasts` | `Broadcaster`: records every transaction sent; answers a queue of outcomes, accepted by default | `tests/mod.rs` `Fake` (broadcast half, `Script.answers`/`sent`), `storage.rs` `Silent` | 2 |
| `Keyed`, `Keys` | `Wallet` + `Builder` + `WalletFactory`: a wallet whose key is its address text; its builder signs deterministic transactions (txid = build count, optional fee leg) and counts builds | `tests/mod.rs` `Fake`/`Build`/`Factory`, `storage.rs` `Keyed`/`Factory` | 5 impls |
| `TipInfo` | `InfoClient` over an rpc client; its tip is the `tip` call | `info.rs` tests `Node`, `usecase/transport.rs` `Tip`, `tests/mod.rs` `Node` | 3 |
| `indexer(chain, storage, balances, confirmer)`, `idle(&Indexer)` | A real `Indexer` with both kinds of index at interval zero, and stepping it until idle. Not a mock: the real indexer over doubles is cheap and exact | `indexer.rs` `indexer()`; `storage.rs` `lined`/`indexer`/`settling`/`jumping`/`idle`; the indexer part of `Scripted::service` | 6 builders |
| `fixtures::{network, config, header, block, run, quiet, coin, payment, signed, spends_to}` | Values of base types: a test network and `NetworkConfig`; header `position` on branch `fork` (the `storage.rs` hash scheme, which server ids assert on); blocks, runs, payments; a pending transaction carrying given bytes | `storage.rs` (`header`, `block`, `run`, `empty`, `quiet`, `coin`, `payment`, `mined`, `send`); `routes.rs` `paid`; `check.rs` `sent`; `header()` in `indexer.rs` and `index/transaction.rs`; `signed()` in bitcoin and ethereum wallet tests; `spends_to()` in ethereum and solana | 3 `header()`, 2 `signed()`, 2 `spends_to()`, 4 `NetworkConfig` literals, ~10 header and ~10 transaction literals |

- **`Memory`'s rules:** inserts that link onto what it keeps; the progress
  rule of every index (birthday or `broadcasted_at`, `indexed_at` at the
  parent); `found`; `delete` rewinds both kinds; `move_start`, `set_state`,
  `settle`, `restart`, `give_up`; `find_next` and `find_behind` by default.
  Seeds: `unspent(inputs)`, `sent(transaction)` (for `find_sent`). Inspect:
  `wallet_index(address)`, `transaction_index(txid)`, `blocks()`.
  **Not** legs, running totals, coin archive or adjustments: those are the
  server's SQL.
- `Keyed` follows the stateful builder (084 iteration 6: `from_existing`,
  `set_fee`, `bump_fee`) once that lands. Another agent is editing
  `transaction.rs` right now.
- Its own tests: `Memory` gets a handful (progress, rewind, settle).
  `Served` and the answer doubles get none.
- With `Memory`, `Served`, `Held` and `Confirms` in base, the indexer's gap
  closing (settle, jump, give up) can be tested in base. Today it is only
  tested through SQLite in `apps/server/tests/storage.rs`.

Layout: `testing/{mod,chain,node,shapes}.rs` (as now) plus `served.rs`,
`memory.rs`, `answers.rs` (`Held`, `Confirms`, `Holders`, `TipInfo`),
`wallets.rs` (`Keyed`, `Keys`, `Broadcasts`), `indexer.rs` and `fixtures.rs`.

### Chains

| Chain | `testing/` today | Add now | Later (074) | Replaced in tests |
| --- | --- | --- | --- | --- |
| Ethereum | `Model`, `render`, `record`, `recordings/`, `Ethereum: TestChain` | `fixtures`: `network()`, `config()`, `atomic()`, the test token address: each defined in both `tests/chain.rs` and `tests/wallet.rs` | — | `Fresh` → `Memory`; `Pending` → `Memory::sent`; `Held` → `Holders::of` |
| Bitcoin | none | feature `testing = ["base/testing", "rpc/testing"]`; `fixtures`: `network()`, `key_of(seed)`, `address_of(seed)`, `funded(owner, sats, index)` | `Model` + `Bitcoin: TestChain`, testnet4 recordings, shapes check | `Store` → `Memory::unspent` |
| Solana | none | same feature; `fixtures`: `base58(byte, len)`, `config()`, `network()`, `MINT`, `SEED` (all defined in both test files) | `Model` + `Solana: TestChain`; `tests/fixtures/devnet-*.json` moves to `testing/recordings/` | `HeldByAll` → `Holders::all` |

JSON builders used by one file (`block`, `receipt`, `log`, `coinbase`) stay
in it. Each `Model` has its own test: the shapes check against recordings
(`tests/recorded.rs` for Ethereum).

### `sdk/trigger`: done, one move

| Holds | Purpose | Change |
| --- | --- | --- |
| `Wallets` | The trigger `Store` in memory; the reference for selectors | stays |
| `Fees` | A `FeeEstimator` at fixed prices that keeps what it was asked | moves to `base::testing` (base owns `FeeEstimator`; see Open questions) |

`tests/common` (the test language, the recording `send` action, `World`,
the UI templates) stays. Only trigger's tests use it, and the server has its
real language.

### `apps/server`: `tests/support` (binary crate, no feature)

The server has no library; its tests compile into the binary (`#[path]`), so
its `testing/` is `tests/support/`. It holds only what is server-specific.

| Holds | Purpose | Change |
| --- | --- | --- |
| `App`, `Api`, `Wallet`, `until`, `scenarios!`, the chain-mounting trait | The suite over mock chains (074) | stays; the trait is renamed from `Served` to `Mounted`, to free the name |
| `db.rs`: `fresh()`, `seeded()` | A migrated temp file, alone or with the test network's rows | new; replaces `tests/mod.rs` `database`/`migrate`, `storage.rs` `storage`, `pipe.rs` `seeded` (keeps its `SEED`), `usecase/tasks.rs` `store`, and `App::boot`'s open and migrate: 5 fixtures become 1 |
| `scripted.rs` | The scripted network (`Scripted` service factory, `container`, `limited`), composed from `Keyed`, `Keys`, `Broadcasts`, `Served` and `Held` | moved from `tests/mod.rs` (~400 lines → ~80); `tests/mod.rs` becomes the module list |

Stays in the test files: the SQL readers in `storage.rs` (`history`,
`blocks`, `rows`, `reads`, `balance`, `totals_hold`, `opened`, `progress`),
`pipe.rs`'s seed, and the case helpers in `cases/mod.rs`.

## What tests get smaller

| Duplication | Today | After |
| --- | --- | --- |
| Test chains (`impl Chained`) | 10: base 4, server 6 | `Served` |
| Store trait impls in tests | 11 in 5 files (`Fake` ×2, `Fresh` ×5, `Pending`, `Store`, `Held`, `HeldByAll`) | `Memory`, `Holders` |
| Wallet, builder, factory, broadcaster fakes | 7 in `tests/mod.rs` and `storage.rs` | `Keyed`, `Keys`, `Broadcasts` |
| Tip-only `InfoClient` | 3 | `TipInfo` |
| Indexer assembly | 6 builders in 3 files | `testing::indexer`, `idle` |
| Block, transaction and config fixtures | 3 `header()`, `paid`, `sent`, 2 `signed()`, 2 `spends_to()`, 4 `NetworkConfig` literals, ~20 struct literals | `base::testing::fixtures` |
| Queue store rules | written twice: 9 memory tests, 7 SQLite tests, 5 helpers each | `store_contract!`, written once and run twice |
| Hand-run jobs | 5 `Claim` literals, 2 run-and-settle copies | `tasks::testing::{claim, run_once, end}` |
| Temp databases | 5 helpers | `support::db` |
| Chain fixtures | Solana: 4 per test file; Ethereum: 4 per test file | each chain's `testing::fixtures` |

Roughly 1,400 lines of hand-written doubles and fixtures across 16 files
become about 800 in their owners' `testing/` modules (82 of them are `Fees`,
moved as is). Another ~200 lines go with the SQLite store tests the contract
replaces. No test loses coverage. The SQLite queue store runs every memory
rule, where today it runs 6 restated ones.

## Order of work

Smallest and most valuable first; each one is net code removal.

| # | What | Done when |
| --- | --- | --- |
| 1 | `tasks`: `testing` feature, `store_contract!`, `claim`/`run_once`/`end`; plus the CI check that release builds enable no `testing` feature | `tests/store.rs` and the server's `usecase/tasks.rs` tests are one macro call each (the server keeps `pruning_*`); the dangling doc link resolves; `send.rs`/`check.rs`/`routes.rs` build no `Claim` |
| 2 | `base::testing`: `fixtures`, `Served`, `TipInfo`, `Held`, `Confirms`, `Holders` | No test file implements `Chained`, `Dated`, `Balances`, `Confirmer`, `Holdings` or `InfoClient`; `header()` is defined once |
| 3 | `base::testing`: `Memory` (with its tests), `indexer`, `idle` | No test file implements a base store trait; `Fresh`, `Pending`, `Store` and the indexer `Fake` are gone |
| 4 | `base::testing`: `Keyed`, `Keys`, `Broadcasts`, `Fees` (moved); server `support/{db,scripted}.rs` | No `impl Wallet`/`Broadcaster`/`WalletFactory` in server tests; one temp-DB helper; `tests/mod.rs` is the module list |
| 5 | Chain `testing` features and `fixtures` (Bitcoin and Solana new, Ethereum extended) | No fixture is defined in two test files of one crate |
| 6 | (074 iterations 2–3) Bitcoin and Solana `Model` + `TestChain`, with recordings and a shapes check | `scenarios!` runs Bitcoin and Solana rows |

## Open questions

1. **`sdk/base` is a core crate.** May iterations 2–4 add `Memory`, `Served`,
   `Held`, `Confirms`, `Holders`, `TipInfo`, `Keyed`, `Keys`, `Broadcasts`,
   `indexer`/`idle` and `fixtures` to `base::testing`? All of it is behind
   `testing`. Recommended: yes.
2. **How deep does `Memory` go?** Progress rules only (recommended, ~300
   lines). Or a full reference with legs, outputs and totals plus a base
   store contract run against the server's `Stores` (as `tasks` does), which
   would turn much of `storage.rs` into one shared suite but costs ~1k lines
   in base.
3. **Does `Fees` move from `trigger::testing` to `base::testing`?** By the
   owner rule, yes. Only trigger's tests, its example and `pipe.rs` use it
   today.
4. **Do `NoHoldings` and `NoTransactionIndexes` stay public?** Recommended:
   yes. The chain READMEs show them as SDK users' defaults. Tests that need
   state use `Memory`/`Holders` instead of wrapping them.
5. **Does `tasks::MemoryStore` stay public?** Recommended: yes. It is the
   queue's documented default and reference; only its rules suite moves
   to `testing`.
6. **Keep or delete the scripted network?** 074 plans to delete
   `tests/{mod,routes,send}.rs` and the fake network once the suite covers
   them. Recommended: move it onto base doubles now (iteration 4), which is
   cheap. Delete each file only when the suite covers its cases:
   `check.rs` pins exact payment outcomes (084/085) that the mock chain
   cannot script cheaply.
7. **Should chain unit tests move from `Script` to their `Model`?**
   Recommended: no. `Script` pins the exact calls an adapter makes; the
   `Model` serves the server suite (074 keeps `Script` too).
