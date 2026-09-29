# 017: What `016`'s punch list left, and why

`a retired design note (016).md` ends in a prioritized punch list. This document
records the outcome of every item so that nothing was dropped silently: what was
implemented, what was deliberately not done, and what a later change should pick
up. It is the closing record of `refactor/chain-unification`.

All eight gates were green before these changes and are green after them.

---

## 1. Implemented

| 016 item | What landed |
|---|---|
| **1** (`D1`, the redb default) | `apps/server/src/storage.rs` resolves a redb path against the working directory (`std::path::absolute`) in one place, `Redb::resolved_path`, which `Config::validate` runs, so a bad path fails at configuration time instead of after every chain has connected. `a_relative_redb_path_opens_against_the_working_directory` opens a real relative path. `docs/API.md` now states the resolution rule |
| **2** (`D2`, custody) | Behaviour unchanged. `docs/SYSTEM_REQUIREMENTS.md` gained a `### Key custody` section stating what the code does — one rule, no per-chain exception; the false "No Solana wallet-table row" sentence is gone, and `docs/API.md` and `ARCHITECTURE.md` say the same thing and cross-reference it. The open decision that section carried (encryption at rest / a registry-less mode) is now **closed**: custody is the API caller's problem. The SDK persists what it is given so a wallet survives a restart, and protecting the store and choosing what keys reach the API belong to the operator and the caller |
| **3** (`D5`, CI) | `.github/workflows/check.yml` runs `git diff --check` over the whole committed tree. `AGENTS.md` no longer claims seven of eight |
| **4** (`A9`/`U2`, amounts) | One shape on all three chains: private field, `from_atomic`/`units` (the chain's own atomic width), `atomic() -> base::Decimal` (the atomic count, the form indexing stores, identical on all three), `from_decimal`/`decimal`, `checked_add`, `checked_sub`. Ethereum's old `atomic(decimals)` is `decimal_with(decimals)`, the ERC-20 reader. All three interpreters now write one expression, `movement.amount.atomic()` |
| **5** (`A1`/`A2`/`A3`, errors) | Solana gained `FeeUnavailable` (used where `getFeeForMessage` returns no price) and `Other` (used for the two encoding invariants), so no `base::TransactionErrorKind` is unreachable from any chain. All three now have `submission`/`ambiguous`/`definite` with the same `&TransactionId` receiver, and each `error.rs` has `every_kind_is_classified_and_no_transaction_kind_is_unreachable` and `submission_ambiguous_and_definite_agree_across_chains` |
| **6** (`A11`) | Bitcoin's `Reservations::reserve` is `admit`, so one word opens the reserve stage on every chain. `Claim` is documented as differing (section 3 below) |
| **7** (`A4`) | Bitcoin's builder accessor is `from()` like the other two |
| **8** (`A5`) | Solana's `TransferBuilder` has `validate()`, called from `restore`, `snapshot`, and `prepare` as on the other two chains |
| **11** (`A12`) | Ethereum's `TransferBuilder::decode` moved from `wallet.rs` to `builder.rs` |
| **12** (`A8`) | Bitcoin has `native::AssetKind { Native }` with `metadata()` and `identity.rs`'s `id()`, so `AssetKind::id()` is now true of all three `identity.rs` files |
| **14**, **15** | `docs/CHAIN_LAYOUT.md` section 6 carries a row per surviving asymmetry, and the four stale entries of the audit's section 2.3 are corrected |
| **17** (`D8`) | `apps/api/src/api/health.rs` takes its two paths from `http_server::{LIVENESS_PATH, READINESS_PATH}` (the OpenAPI route assertion does too). `http-server`'s duplicate `HealthState`, `service_router`, `ReadinessState`, and `readiness` handler are deleted; its README and `ARCHITECTURE.md` now say it owns the paths and the application owns the handlers |
| **18**, partly | `generate_with`, byte-identical on three chains, is `wallets::Generation::of(factory, secret)`. See section 2 for the parts deliberately not lifted |
| **23** | The root `README.md` has a `## Run it` section: a complete minimal `config.json`, the `cargo run -p payment-server` line, two `curl` calls, and the per-chain example commands with the node each needs |
| **24** | "provider" is gone from doc comments and from the one domain-shaped error string; test fixture literals and test names still use the word as arbitrary text |
| **25** (`D6`, `D7`) | `sdk/indexing/README.md` names `RegistryContract`. The second half named `sdk/indexing/postgres/README.md`, which `027` deleted with the crate; the same facts now live in `sdk/indexing/README.md`'s API map and in `docs/INDEXING.md` §Storage |
| **26** | `docs/adr/readme.md` states the ADR policy for readers; `AGENTS.md` states it for agents (`docs/adr/` is deliberately outside the source-of-truth order) |
| `A14` | Solana's `sender/stub.rs` is gated the same way as the other two (`#[cfg(test)] pub(crate) mod stub;`) |

---

## 2. Duplication deliberately not lifted (016 item 18)

`generate_with` was lifted. These were looked at and left, each for a stated
reason rather than for lack of time.

**`P2`, the btc/eth `error.rs` bodies (~100 lines each).** A macro or a
kind-trait bridge would work, and it would move the mapping table out of the
file `docs/CHAIN_LAYOUT.md` section 1 assigns it to ("`error.rs` … the only
error-kind mapping") and section 2 repeats ("`error.rs` converts; nothing else
maps errors ad hoc"). The table *is* the specification of what a chain does
with a fee failure or an unknown outcome, and a reader who wants it should find
it in the chain's own `error.rs`, not in a macro in another crate. The
duplication is also now *checked* rather than merely repeated: the parity test
added in each file fails if a chain drifts. A kind trait would additionally add
a seventh type named `ErrorKind` to a graph where six is already a complaint
(016 section 7.2).

**`P3`, the btc/eth `BlockSource::blocks` walk.** The shared part is twelve
lines of dense loop; the rest of the audit's "97 lines" is the surrounding file,
which is parallel *by requirement*. A `walk_range` helper in `sdk/indexing`
would need the chain's name for its error message and would serve two of three
chains (Solana's node pages natively). The helper plus its documentation and
tests would be larger than what it replaces.

**`WalletFactory::create`, also byte-identical ×3.** It is three lines, but the
lines name `KeyPair` and `attach`, which are chain types. Lifting it needs an
associated `KeyPair` type on the factory trait — that is `U1` below, which
`a retired design note (018)` has since closed as not worth doing, so these three lines
stay.

**`P5`, the scripted-RPC doubles.** **Closed by `030` step 15.** One double,
`json_rpc::testing::{Script, Reply}` (191 lines), behind a default-off
`testing` feature that appears only in the three chains' `[dev-dependencies]`
— `cargo tree -p payment-server -e normal -f "{p} {f}"` reports `json-rpc
default`, and only the dev graph reports `default,testing`. It keeps every
assertion the three hand-written doubles made: the method, the exact
parameters where a fixture states them, the recorded call log, "script
drained" (`exhausted`), and the `Repetition` pin bitcoin and ethereum gained
with `023`, which is now `Script::once_only(method)` — a request for the
pinned submission method that carries `Repetition::Permitted` fails the test.
`packages/json-rpc/tests/transport/script.rs` (115 lines) is the double's own
contract, including the five failure modes, which no chain had asserted
before. What stays per chain is a `Scripted` trait in
`native/rpc/script.rs` naming that chain's *fixtures* and nothing else:
bitcoin 32 lines (regtest, `sendrawtransaction`), ethereum 35 (`CHAIN_ID`,
`eth_sendRawTransaction`), solana 44 (`GENESIS`, the `localnet` cluster, and
`Scripted::exchanges`, which takes `(method, params, result)` triples so a
Solana fixture *cannot* omit its parameter assertion — the one thing the
shared `Reply` leaves optional, kept as a chain specialisation rather than
weakened). 381 lines of triplicated mechanism become 111. Solana's remaining
161 lines are its loopback HTTP tests, which are about the real transport,
not the double.

**`P6`, ethereum's hand-rolled hex.** Not attempted; a few lines and purely
local. Safe and independent.

---

## 3. Asymmetries that stay, and where they are justified

Every one of the audit's fifteen is now either removed (section 1) or carries a
one-line justification in `docs/CHAIN_LAYOUT.md` section 6: the extra Solana
`ErrorKind::Unsupported`, the amount receiver (`Copy` versus 256-bit), the
`AssetKind` shape and `WalletConfig::asset()`, the absent bitcoin
`WalletFactory::sibling`, the interpreter network guard, `Batch`/`Claim`, `key.rs`
size, and the test seam. Section 8 no longer asks for per-chain byte
symmetry at all: `examples/` is organised by scenario, each scenario sets up
every chain, and `examples/tests/scenarios.rs` asserts that a chain crate is
named only inside that scenario's own setup function (016 item 13 is answered
by the axis change, not by extending the retired `symmetry.rs`).

Two of these deserve to be read as decisions, not accidents:

- **Bitcoin has no `sibling`** (`U3`). `a retired design note (010)` section 4.1 asked for it
  on every chain. Bitcoin is single-asset by construction and its `Outbox` is
  built from the `WalletConfig`, so two configurations could not share one
  anyway. The requirement is amended in section 6 rather than met.
- **The interpreter network guard is bitcoin-only** (`A7`). Only bitcoin's
  `RawBlock` records a network — it needs one to decode addresses. Ethereum's
  and Solana's raw evidence carries no network identity, so there is nothing to
  compare; the `Composer` owns the partition.

---

## 4. Deferred, with rationale (016 items 16, 19–22, 27–30)

Nothing here is blocked; each is a self-contained piece of work.

| # | Item | Why it is deferred |
|---|---|---|
| **U1** | Compiler-enforced chain shape: `base::Chain` / `indexing::ChainIndexer` / `wallets::ChainFamily` traits with associated `Network`/`Source`/`Interpreter`/`KeyPair` types (`a retired design note (001)` section 6.8) | **Closed as not worth doing** by `a retired design note (018).md` section 8, which separated the item in two. The *network identity* half shipped: `base::Networked { fn canonical(&self) -> Network }`, implemented reflexively by `base::Network` and by each chain's native `Network` in its `identity.rs`, so `Repository::new` ×2, `SyncConfig::new` and `BlockInterpreter::new` ×3 take `&impl Networked` and `examples/` contains zero `canonical()` calls. The *bundle* half is refused: no generic function consumes every associated type at once (018 section 3); `WalletFactory::new` legitimately takes different dependencies per chain, so a trait could only hide them behind another associated type the caller still builds (section 6 of this document); `ChainFamily`/`Provider` are banned names; `linter.toml`'s `rust/trait-method-count` caps a trait at three methods; and the required tree, `scripts/check-chains.sh`, `indexing::delegate_indexer!` and `examples/tests/scenarios.rs` already catch what a trait cannot. Reopen only if a fourth chain shows the tooling missing something a type could have caught. This entry is the record `016` asked for (item 27) |
| **U4** | `solana/src/sender/outbox.rs` (1136 lines) and `ethereum/src/sender/reservation.rs` (534) exceed "≤ 500 lines including tests" | **Closed: the rule was wrong, not the files.** `linter.toml`'s `rust/file-length` counts *production* lines, and always has; the files are at 424 and 393, well inside 500, so `linter check .` reports nothing and no `linter:disable` is warranted — a directive suppressing a finding that does not exist would be noise. The "including tests" sentence was a doc-only stricter claim that nothing enforced; `docs/CHAIN_LAYOUT.md` section 5 now states the measured rule. The limit exists to bound how much *behaviour* one file owns, and a table-driven test module is evidence about that behaviour, not more of it. Splitting these files would split test modules to satisfy a number no tool checks, and `docs/CHAIN_LAYOUT.md` section 1 admits a `sender/` submodule only for a chain-only part of the contract, which neither is. Files unchanged |
| **U5** | Solana's `WalletFactory::new` takes `&Arc<Indexer>` rather than a narrowed port (`a retired design note (014)`) | `docs/CHAIN_LAYOUT.md` section 6 blesses it today. The factory needs three unrelated reads (checkpoint, commit announcements, history), so the narrow port is a new three-method trait in `indexing`; worth doing, but it is a contract change |
| **U6** | `a retired design note (006)` section 6.5's acceptance grep ("`Service::new` outside `sdk/indexing` returns nothing") | Unsatisfiable as written: each chain's `Indexer` newtype is *supposed* to call `Service::new`. The criterion should read "no chain constructs an `indexing::Service` outside its `indexer.rs`", which is true and is what `linter.toml` requires. Recorded here rather than by editing a closed fails document (016 item 28) |
| **U7** | One crate error with a **private** `kind` and a `kind()` accessor | All three expose `pub kind`, consistently, and every consumer matches on it. Making it private is a mechanical but wide change across three crates and their tests, for a gain that is stylistic. Do it with the `ErrorKind` naming decision below, not separately |
| **016-16** | A `//!` header on every `sdk/indexing/src/*.rs` (12 of 15 have none), and the three cross-chain header gaps | Pure documentation; safe to do at any time. The three parallel-file gaps are `native/rpc/submitter.rs`, `native/rpc/accounts.rs`, `sender/stub.rs` |
| **016-19** | Move the test modules out of `packages/json-rpc/src/lib.rs` (775 of 809 lines) and `sdk/indexing/redb/src/lib.rs` (616 of 662), and `synchronizer_test` out of `sdk/indexing/src/lib.rs` | **Closed.** `027` moved `indexing-redb`'s 458-line `repository_test` and `lib.rs`'s `synchronizer_test` to `sdk/indexing/tests/`, which is where they belonged: every assertion in them went through the public API. The `json-rpc` half was overtaken by `023`, which split the 809-line `lib.rs` into eight modules and put 1 192 lines of behaviour into `packages/json-rpc/tests/transport/` (`endpoint`, `failover`, `http`, `node`, `retry`); `lib.rs` is now 41 lines with no test module at all. `030` step 15 finished it: `error.rs`'s 52-line module, the last one written entirely against the public API, is `tests/transport/error.rs`. What stays inline is `http.rs`'s `Error::from_rpc` table and `retry.rs`'s `Backoff::delay` / `Decision::repeats` — private items, exactly the `sdk/indexing/src/storage/redb/**` case below (backend, codec, path, support), and `linter.toml`'s `rust/file-length` counts production lines, so their size is not the argument it looks like |
| **016-20** | `packages/storage/redb/src/backend.rs` injects faults by adding test-only variants to the production `Command` enum and test-only fields to `Backend` | **Closed by `027`.** The four `#[cfg(test)]` `Command` variants, the three `#[cfg(test)]` fields on `Backend`, the four sender methods and the arms in the production `run` loop are gone. In their place is one seam, `storage/redb/backend/probe.rs`: a `Probe` trait with one method, `reached(Step)`, over four named points (`Received`, `Committed`, `Reopening`, `Reopened`). `Backend` holds `Option<Arc<dyn Probe>>` and production installs `None`, so no injected branch exists to fire; the tests install one implementation that blocks the owner thread, arms one ambiguous commit, arms one reopen failure, and counts reopens. Nothing in the backend is `#[cfg(test)]` any more, which matters because the collapse widened `cfg(test)` from a 61-test binary to the whole `indexing` test build |
| **016-22** | Remove the dead exports: `base::{TestnetMap, DecimalParts, DecimalSign, TagError}`, `Decimal::{parts, from_parts}`, `indexing::CommitPermit`, `http_server::AuthenticationModeParseError`, `Redb::open_with_queue_capacity`, `indexing_runtime::TaskError` | **Mostly settled.** `DecimalParts`/`DecimalSign` went with the decimal extraction. The `base` naming pass then removed `TestnetMap` (folded into `ChainCollection { mainnet, testnets }` plus `ChainCollection::testnet(name)`), the `TransactionEnvelope` alias, the `base::Id` root alias, and the public `AssetId::NATIVE`; `AssetTag`/`TagError` moved to `apps/api`, whose routes are their only consumers. `bitcoin::native::rpc::Credentials` (no consumer) and the one-impl `bitcoin::native::rpc::Fees` trait (now an inherent `Client::estimate`) went with them, and `solana::WalletFactory::sibling` (one asset, so it could only clone) with those. The rest stay and are now documented where they live: `CommitPermit` is `SyncPlan::begin`'s return, `AuthenticationModeParseError` is a public `FromStr`'s error, `Redb::open_with_queue_capacity` has callers. `TaskError` is **now gone**: the supervisor (formerly `indexing-runtime`) returns typed `IndexError`, per `a retired design note (022)` section 7. **`027` closed the `open_with_queue_capacity` half**: it had no cross-crate caller after all, only two tests, so it is now the private `Database::with_queue`, which is also where the fault-injection probe is installed |
| **016-29** | Six types named `ErrorKind` across the graph (`base`, `base::transaction`, `wallets`, `crypto`, `storage`, `json-rpc`, `http-server`), so every consumer aliases on import | **One fewer.** `027` deleted `storage::ErrorKind` with the crate: the redb backend now returns `IndexError` directly, and its five internal classes collapse onto the three outcomes a caller could ever observe (`Conflict`, `Store` retryable, `Store` terminal). The rest is still a naming decision with a wide blast radius, worth taking together with `U7` |
| **016-30** | Whether Solana's factory should take a narrowed index port | Same item as `U5` |

## 5. Queued by `docs/design/019` (chain failure isolation)

`019` sections 2-4 shipped: per-unit supervision, the `SyncMonitor` /
`SyncControl` pair, the `NetworkState` / `NetworkFailure` / `NetworkHealth` /
`SyncHealth` vocabulary, the readiness rule, the per-chain cadence, the
backoff and quarantine policy, and the `state`/`failure` fields on
`GET /v1/status`. Two pieces of `019` were deliberately **not** implemented in
that pass and were queued for a separate decision. **`019-5.3` has since
landed**; `019-6.3` has not, and `024` owns what replaces it. There is no
`TODO` in the source, because `AGENTS.md` and `linter.toml` forbid one.

| # | Item | State |
|---|---|---|
| **019-5.3** | The offline rescan: `BlockStore::purge_network(network)` on both backends and a `payment-server <config.json> --rescan <chain>/<network>` subcommand that purges one network's indexing-owned rows (checkpoint, history, movements, outputs), never the address registry and never another network, prints what it removed, and exits | **Landed**, as the *only* remedy for a reorg past a committed block. `025` deleted the rollback journal and with it `ReorgTooDeep`; the class that reaches this row is `ConfirmedReorg`, Bitcoin-only (a reorg deeper than the configured `confirmation_depth`) and impossible on Ethereum and Solana. What shipped: `BlockStore::purge_network -> PurgedRows` (a third method on the existing trait, which `linter.toml`'s cap of three allows, so no new trait; it returns per-collection row counts rather than `()` because the subcommand has to print them), one atomic removal per backend (`storage/redb/blocks.rs` scans the network's history and output prefixes and its checkpoint key into one conditional batch and decodes nothing, so a record this build cannot read is still removable; the PostgreSQL one ran four bound `DELETE`s in one transaction under the network's advisory lock, and went with that crate when `027` deleted it), and `apps/server/src/rescan.rs`, which refuses a malformed argument and a network the file does not configure *before* opening storage, prints what it will do, then the counts, and never binds the listener or starts the supervisor. Covered by `indexing::testing::PurgeContract`, which every backend's contract suite runs, and by `a_rescanned_network_reindexes_from_its_registered_start` in `sdk/indexing/tests/service_contract.rs`. `PurgeContract` is written against the port, not a backend, and `027`'s in-memory second implementation now runs it too |
| **019-6.3** | Refusing a wallet birthday below the network checkpoint in `Wallets::store`'s explicit-birthday branch, with the `already_registered_at` refinement so a restart of a correctly configured server does not fail | **Not implemented, and will not be implemented as written.** The site it named is gone: `029` moved observation to the index, so `Wallets::store` no longer exists and the explicit-birthday branch is now `Watching::watch(id, subject, Start::At(p))` in `sdk/indexing/src/engine/watches.rs`. The behaviour is unchanged — an explicit `start_position` is accepted without comparing it to the checkpoint, and `Synchronizer::catch_up` consults `SyncPlan::earliest_position()` only through `initialize_checkpoint`, which runs only when there is no checkpoint — so a birthday below an existing checkpoint is still accepted and still indexes nothing. No longer true: the `InvalidBlock` it used to arm. `024`'s reported-spend half has landed, so a spend of an output this network never indexed is returned in `BlockOutcome::Applied { unknown_spends }` and never fails the chain. `024`'s other half, "accept and backfill", is **not** in the tree yet and is what replaces the refusal; refusal survives only for a range the source cannot serve, and `024` owns it — and `029` §5.4 puts `024`'s `max_backfill_blocks` refusal at this same new site. The rescan this row was queued with now exists, so the silent-data-loss case has an operator remedy today: a startup `wallets[]` entry with the lower `start_position` plus `--rescan <chain>/<network>`, which `docs/API.md` §Routes states |

Knowingly skipped by their own documents, needing no action: `012` step 10
(PostgreSQL asset index), `012` section 3.3 B (redb secondary keys), `015` T7
(`WalletFactory::balance`).

`029` retired two more rows' worth of vocabulary wherever this file names it:
`Wallets::store`, `Wallets::filters`, `FilterSource` and `Observer` no longer
exist, and `Supervisor::new` takes no selection. Where a row above names one of
them, read the site it now lives at: the watch set on `Service`
(`engine/watches.rs`), `Watching::watch`, and `Listener::committed`.
