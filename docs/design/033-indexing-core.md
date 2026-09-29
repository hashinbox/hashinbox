# 033 — Reducing indexing to its core

> "start reducing indexing, engine needs to be small focused core based of
> description, this bloated nonsense"

This is a read-only audit of `sdk/indexing/src` against `docs/design/030 §8 (ex-032)`'s
concepts table, with a per-item verdict and an ordered reduction plan. It edits
nothing.

> **This plan has been executed. Everything below is the audit as written; §7
> is what happened.** Read §7 before acting on any part of §5 — six of its
> eleven steps landed roughly as planned, **four had their central factual
> claim disproved on contact**, and both of §6's open questions now have
> answers. Line numbers throughout §1–§5 were already stale when the audit
> closed and are staler now; §7 says which arguments survived.
>
> The measurement at the end of execution: **58 files, 13,517 production lines,
> 17,108 physical, 3,789 doc-comment lines, 8,583 lines of code.** Against the
> tree this audit opened on (61 files, 13,605 production, 8,754 code) that is
> **−171 lines of code, +92 lines of recorded reason, −3 files** — which is
> §0.3a's finding happening one more time, now to the plan that was written
> about it.

Verdict grades, as `032` used them: **verified** (read from the code, with the
line), **probable, risk named**, **not determinable without building it**.

---

## 0. The diagnosis: the number the user is reacting to is 28% prose

Measure first, because the instruction contains a quantity claim and the
quantity is not what it looks like.

`sdk/indexing/src` is **62 files, 13,724 production lines, 17,107 physical
lines** (production = lines before the first `#[cfg(test)]` or
`#[cfg(all(test, …))]` attribute, which is what `linter.toml:70-72` counts).
`sdk/indexing/tests/` adds 5,712 more.

Two corrections to the numbers this work was commissioned with: the tree is
**62 files, not 54**, and the root holds **28 files, not 31**. Production lines
(13,724) were close.

### 0.1 Where the 13,724 lines are

| Subtree | Files | Production | Physical |
|---|---:|---:|---:|
| root (`src/*.rs`) | 28 | 5,630 | 7,696 |
| `testing/` | 12 | 3,471 | 3,471 |
| `storage/sqlite/` | 10 | 2,833 | 4,044 |
| `storage/` (ports) | 7 | 979 | 1,085 |
| `supervisor/` | 4 | 517 | 517 |
| `watches/` | 1 | 294 | 294 |
| **total** | **62** | **13,724** | **17,107** |

### 0.2 What those production lines consist of

Counted over the concatenated production halves of all 62 files:

| Kind | Lines | Share |
|---|---:|---:|
| doc comments (`///`, `//!`) | 3,887 | 27.4% |
| blank | 949 | 6.7% |
| non-doc comments (`//`) | 198 | 1.4% |
| **code** | **9,150** | **64.5%** |

So of the 13,724 lines, **3,887 are documentation**. Of the remaining 9,150
lines of code, **3,471 are `testing/`** — contract scenarios shipped behind a
cargo feature, which the `rust/file-length` rule counts as production because
`src/testing/` does not match its layout rule's test globs
(`linter.toml:60-67` names `**/{test,tests}/**/*.rs`, not `**/testing/**`).
Another **2,833 are one SQLite backend**.

**The engine — domain vocabulary, the one network owner, the pass, the ports,
the optional runner — is about 5,900 production lines, of which roughly 1,700
are doc comments. Call it 4,200 lines of engine code.** That is a small focused
core already. The number that reads as bloat is a number in which prose, one
SQL backend and a contract suite together outweigh the engine three to one.

### 0.3a The same measurement after the lane landed

Re-run at the end of this audit, for comparison:

| Subtree | Files, before | Production, before | Production, mid-lane | Production, after |
|---|---:|---:|---:|---:|
| root | 28 | 5,630 | 5,584 | 5,584 |
| `testing/` | 12 | 3,471 | 3,392 | 3,392 |
| `storage/sqlite/` | 10 | 2,833 | 2,833 | 2,833 |
| `storage/` | 7 | 979 | 1,227 | 905 |
| `supervisor/` | 4 | 517 | 517 | 517 |
| `watches/` | 1 | 294 | 203 | 203 |
| **total** | **62** | **13,724** | **13,756** | **13,373** |

Read the middle column first, because it is the finding. **Collapsing the two
write paths, measured on its own, did not reduce the tree — it moved it.**
`storage/backfill.rs` (292 production) became `storage/commit.rs` (466),
`watches/set.rs` lost 91 and `testing/` lost 79, for a net of **+32**. That is
not a criticism of the lane: it replaced a two-path write with a one-path write
and a contiguous progress model, which is a correctness and vocabulary win, and
`032` never promised it would be a line win. It is the second piece of evidence
for §0's conclusion — **this tree does not give lines back when its structure
improves**, because its bulk is vocabulary, documentation and contracts rather
than machinery.

The last column is the same collapse **plus one deletion**: the 322 production
lines of the superseded `storage/block.rs`, which sat orphaned in the working
tree — `storage.rs:31-37` had stopped declaring it — until the lane removed it.
One `git rm` is 91% of the net reduction the whole collapse produced.

This is not a defence of the tree. It is the constraint on what "reduce" can
mean: §5 finds about **1,000 removable production lines, 7.3%** of the total,
and no honest reading of this code finds ten times that. If the instruction
means "the file count and the reading order are wrong," §3.2 is the finding it
wants; if it means "delete a third of it," the answer is that `025`, `027`,
`030` and `032` already did — the rollback journal, `BlockStore::remove`,
`ReorgTooDeep`, `reorg_retention`, the whole redb backend, three crates,
`Synchronizer`, the private read-side `Index`, `FilterSource`, `Uncommitted`,
`NetworkFailure`, `HistoryCursor`, `RegisteredAddress`, `Lines`,
`SyncConfig::validate_filters` and `wallets::Repository` are all gone, and
what is left resists.

### 0.3 The one number that is genuinely bad

`sdk/indexing/src/storage/sqlite/tables.rs:38` declares a struct with **one
field** (`network: Network`). It carries **67 inherent methods across eight
`impl Tables` blocks in eight different files** — `tables.rs:42` (18),
`history.rs:52` (13), `outputs.rs:42` (13), `blocks.rs:109` (6),
`watches.rs:124` (6), `backfill.rs:23` (6), `credits.rs:19` (4),
`balance.rs:40` (1). Nine of those methods take no `self` at all
(`tables.rs:164,177,182,187,192,198,204,216,236`).

`sqlite::Repository` (`storage/sqlite.rs:132`) has five. Every one of its ten
port methods validates an argument, hops to a blocking thread, and calls one
`Tables` method under a different name. **`Tables` is the design object of this
backend and it has no boundary.** `testing::Memory` has the same shape at 856
lines across four files (§3.1).

That is where a reader's "plenty of garbage put together" lands, and it is one
type, not thirty-one files.

### 0.4 Which citations in this document are stale

Verified against the tree at the end of the audit. Everything not listed here
was unaffected.

| Cited as | Now | Effect on the argument |
|---|---|---|
| `storage/backfill.rs` (whole file, 292 lines) | **deleted** | §2.3's four `Backfill*` rows are history; the verdicts were right and are now moot |
| `BackfillProgress { address, start, resume, anchor }` `storage/backfill.rs:28` | **deleted**; replaced by `WatchProgress::indexed_through: Option<BlockRef>` (`storage/watch.rs:25`) and `watches/set.rs:194` `Owed { watch, address, expected }` | `032`'s "What did not land" no longer applies — the contiguous model landed |
| `BackfilledBlock` `storage/backfill.rs:82` | **deleted** — `IndexedBatch.blocks` is `Vec<CanonicalBlock>` (`storage/commit.rs:65`) | the "accidental" verdict was correct |
| `BackfillBatch` `storage/backfill.rs:104` | **deleted** → `IndexedBatch { network, expected_checkpoint, checkpoint, blocks, progress }` (`storage/commit.rs:61`) | as predicted, including that the whole-chunk fold survives as behaviour |
| `BackfillOutcome` `storage/backfill.rs:274` | **deleted** | `written`, the field with zero production readers, is gone with it |
| `IndexStore::{checkpoint, commit, backfill}` `storage/block.rs:104` | `IndexStore::{checkpoint, commit}` `storage/commit.rs:298` | §1's "three methods where the table wants one" is now two, and `checkpoint` is retained deliberately — its doc at `commit.rs:300-307` states `032` recommendation 1 verbatim |
| `Purging`, `PurgedRows` `storage/block.rs:202,244` | `storage/commit.rs:380,423`; `purge_network` now also resets every `WatchProgress::indexed_through` of the network in the same transaction | this closes `032` §1's "Gap found" and collision 9 — the purge reaching progress was the one thing neither port could express |
| `Coverage` `watches/set.rs:30` | **deleted** | its two states (nobody has worked it out / worked out but not written) are no longer needed once progress is one position |
| `Progress`, `BlockWindow`, `ConfirmedLine` `pass.rs:28,36,47` | `Committed`, `BlockWindow`, `ConfirmedLine` `pass.rs:28,36,47` — `Progress` is now the public `storage/commit.rs:42` type | `pass.rs` is 481 lines, up from 436 |
| `sqlite/backfill.rs` 184 production | **unchanged** | the SQLite half of the collapse has not landed; §5.11's remaining work |

**The one defect this audit found is fixed, and fixed structurally rather than
by adding the check.** §5.11 warned that `BackfillBatch::filled`
(`storage/backfill.rs:180-186`) ran `CanonicalBlock::new`'s validation *minus*
its duplicate-transaction-identity check (`block.rs:172-181`), so a backfilled
block with two transactions of one identity was accepted where a committed one
was refused. `IndexedBatch::new` (`storage/commit.rs:87`) takes
`Vec<CanonicalBlock>`, and a `CanonicalBlock` can only be built by
`CanonicalBlock::new`. **The weaker path no longer exists to inherit from.**
`IndexedBatch::new` adds three checks of its own the old pair had nowhere for
(`storage/commit.rs:148-178`): one observation advanced twice in one batch, a
claim above the confirmed line, and a claim that does not advance the value it
expects.

---

## 1. Is `032`'s table the right spec?

**No — not because it is wrong, but because it is already satisfied.** Every
one of its eleven rows exists in the code today. It therefore names nothing to
delete and nothing to add, which makes it useless as a reduction target and
dangerous as a completeness test.

| `032` row | Realised by | Verified |
|---|---|---|
| `Watch` | `watch.rs:34` `Watch { id, address, asset, start }` | yes |
| `WatchProgress` | `storage/watch.rs:25` | yes. **At the start of this audit it was `{ watch, coverage: Option<BackfillProgress> }` — the four-field non-contiguous form `032` recorded as not landed. The lane landed `indexed_through: Option<BlockRef>` during the audit** (§0.4) |
| `Watches` | `watches.rs:30` `Watches` + `watches/set.rs:37` `Set` + `:30` `Coverage` | yes, as three types |
| `Block` | `block.rs:147` `CanonicalBlock` | yes, under another name |
| `Blocks` | `overlay.rs:59` `Frontier` | yes, under another name; `032` §"Step 4" records that the rename is deliberately not done |
| `Transaction` | `observation.rs:205/280/295`, three stages | yes, deliberately three (§3.5) |
| `Indexer` | `service.rs:41` `Service<S,I,R>` + three chain newtypes | yes; `032` records why the name cannot be taken inside the crate |
| `Index` | `composer.rs:29` `Composer<C>` | yes — the table's `Index` row ("assembles network indexers over one repository, routes observations and coordinates their bounded `sync` calls") is a verbatim description of `Composer` |
| `WatchStore` | `storage/watch.rs:45` | yes, three methods (`add`/`all`/`forget`) |
| `IndexStore` | was `storage/block.rs:104` (three methods), now `storage/commit.rs:298` (two) | yes. The table implies one method; the second, `checkpoint`, is retained deliberately and §1.1 argues it needs a row of its own |
| `Repository` | `storage.rs:55` blanket impl | yes |

**11 of 11 present.** Those eleven concepts are realised by about 15 types in
6 files out of 62. The table says nothing about the other 56.

### 1.1 The four rows the table needs before it can be used as a spec

Taking the table as an exhaustive description and deleting what it omits would
delete four categories that `032`'s own proof of mapping verified necessary.
A successor document should add these rows rather than leave a reader to
rediscover them:

| Missing row | What it would own | Lines | Why it must be named |
|---|---|---:|---|
| **The network's durable line** (`Checkpoint`) | the confirmed/pending boundary, the commit-time CAS base, the `Start::Fresh` origin, the `GET /v1/status` payload, the output-cursor anchor | `indexer.rs:11`, `storage/block.rs:113` | `032` §1 proves per-watch progress cannot substitute (the double-count argument) and its recommendation 1 says "add `indexed_through` *beside* it; do not replace it". The table has no row for it at all |
| **The admission handshake** | `NetworkAdmission`, `SyncPlan`, `CommitPermit`, `PublicationPermit` | `admission.rs`, 345 | `032` §6 verified all four survive per-watch progress unchanged in structure, and `DEFECT_PATTERNS.md:226-232` records an agent proposing their deletion and being wrong. Each of the four handshake steps has a test that goes green while the invariant disappears |
| **Honesty about incompleteness** | `Completeness`, `BackfillStatus`, `SyncStatus`, `NetworkHealth`, `SyncHealth`, `IndexError` | `balance.rs:45`, `sync.rs`, `health.rs`, `error.rs`, ~600 | `Completeness` is 45 external call sites across five crates; `SyncHealth::settled` gates the HTTP bind (`apps/server/src/main.rs`). None of it is a domain concept in the table's sense and all of it is load-bearing |
| **Consumer capabilities** | `TransactionStore`, `BalanceStore`, `OutputStore`, `Purging` and their query vocabulary | `storage/{history,balance,output}.rs` + `storage/block.rs:202`, 979 | The table's prose demotes them correctly; the table itself lists none, so a reader counting rows concludes the storage surface is two traits when it is six |

The **optional runner** (`Composer` 302 + `supervisor/**` 517 + `schedule.rs`
100 = 919 lines) is arguably covered by the `Index` row and arguably not; a
successor should say which.

> **Verdict on the spec: `032`'s table is the right description of the write
> path's domain vocabulary and is complete for it. It is not a reduction spec,
> and used as one it deletes the per-network line, the permits, the operational
> surface and the consumer ports — roughly 2,800 production lines, all four
> categories verified necessary by `032`'s own proof.** Amend it with the four
> rows above.

---

## 2. Per-item verdicts

Every `pub` and `pub(crate)` type, trait and module in `sdk/indexing/src`.
**Core** = named by `032`'s table or provably required by something that is.
**Earns it** = not in the table, checked, keeps its place. **Accidental** =
exists because of a constraint rather than the domain.

External-call-site counts are from a scripted sweep over all 268 `.rs` files
under `apps/ sdk/ packages/ examples/ tests/` excluding `sdk/indexing/src/`,
with every hit resolved through the importing file's own `use` list (the
ambiguous names — `Page`, `Start`, `Completeness`, `Balance`, `History`,
`Repository`, `Service`, `Subject`, `Effect` — were each checked against a
same-named local type; `apps/api` defines its own `Start`, `Completeness`,
`TransactionPage`, `HistoryQuery` and `Balance`, re-exported at
`apps/api/src/api/mod.rs:16-28`, and only the `indexing::`-qualified hits were
credited).

### 2.1 Domain vocabulary

| Item | file:line | Verdict | Note |
|---|---|---|---|
| `Watch` | `watch.rs:34` | **Core** | table row; 67 external sites |
| `Start` | `watch.rs:23` | **Core** | the birthday of a `Watch`; 84 external sites. `Fresh` on an empty index still resolves to position 0 with no policy (`032` §5) |
| `AddressFilter` | `watch.rs:10` | **Earns it** | *not* a projection of one `Watch`: `Set::filters` (`watches/set.rs:54`) emits one filter per **address**, from the earliest birthday any observation of it names. A `Vec<Watch>` cannot express that, which is the whole same-address-multi-asset capability |
| `Watching` | `watch.rs:65` | **Core** | required by the table's `Indexer` |
| `Watches` | `watches.rs:30` | **Core** | table row |
| `Set`, `Coverage` | `watches/set.rs:37,30` | **Core** | the table row's two keyspaces (rows by identity, coverage by address). `Coverage` has since been deleted and `Owed` (`watches/set.rs:194`) added — §0.4 |
| `CanonicalBlock` | `block.rs:147` | **Core** | table's `Block` |
| `InterpretedBlock` | `block.rs:98` | **Core** | what a `BlockInterpreter` returns; 37 external sites |
| `BlockCredit` | `block.rs:34` | **Earns it** | value a block moves with no transaction to name it. An Ethereum consensus withdrawal has no transaction, receipt or trace entry; without it a withdrawal-funded address reads permanently short and says nothing (`testing/backfill.rs:266-268`, `testing/subjects.rs:7-12`) |
| `Frontier`, `Sample` | `overlay.rs:59,33` | **Core** | table's `Blocks`, plus the sample a read takes. `Sample.revision` is what makes an output cursor refuse a page folded over different blocks (`reads.rs:190`) |
| `LiveChanges` | `overlay.rs:280` | **Earns it** | the fold of pending blocks onto one address's live outputs. Not the same fold as `PendingValue::fold` — one produces outputs, the other amounts |
| `CanonicalTransaction` | `observation.rs:205` | **Core** | table's `Transaction`, inclusion stage |
| `ObservationDraft`, `ObservationDraftStatus` | `observation.rs:295,378` | **Core** | draft stage (§3.5) |
| `ObservedTransaction`, `TransactionStatus` | `observation.rs:280,254` | **Core** | read stage (§3.5) |
| `CanonicalStatus` | `observation.rs:187` | **Core** | `Included{block}` / `Failed{block,reason}` |
| `ValueMovement`, `MovementId` | `observation.rs:23,15` | **Accidental in shape, core in content** | see §5.5: five variants sharing `{id, asset, amount}` cost 90 lines of accessors and a 45-line enum↔row round-trip, for an arity guarantee no consumer reads |
| `NetworkFee` | `observation.rs:155` | **Core** | always the native asset; `Ledger::apply` needs the payer |
| `OutputId`, `OutputKey`, `IndexedOutput`, `OutputChanges` | `output.rs:14,23,35,65` | **Core** | the table's `Block` carries output changes; Bitcoin coin selection is a chain-correctness consumer (`032` §9) |
| `Holder` | `holder.rs:12` | **Earns it** | 2 methods, **11 impls, 8 of them outside this crate** (bitcoin/ethereum/solana `Wallet`, `apps/api::Custody`, `wallets::factory`'s macro, three fixtures). `wallets::Wallet: Holder + Signer` (`sdk/wallets/src/wallet.rs:71`). Removing it breaks four crates |
| `Subject` | `holder.rs:27` | **Earns it** | a two-field pair with no logic, and 46 external sites across seven crates. It is the canonical keyless `Holder`; every use is a struct literal. A wrapper, but the one the trait needs |
| `Balance`, `PendingValue` | `balance.rs:24`, `history.rs:215` | **Earns it** | the two halves of a balance, each with its own anchor |
| `Completeness` | `balance.rs:45` | **Earns it, verified** | 45 external sites. `while_backfilling` is the single gate both the fold (`reads.rs:99`) and the history page (`reads.rs:65`) take. `032` gap 10 records the missing history gate as a shipped bug this closed |
| `FeeSettlement` | `balance.rs:102` | **Earns it** | `Movements` vs `Charged` is not guessable from row shape and the two differ by exactly the fees paid — never by twice one (`testing/balance.rs:68-70`) |
| `Ledger` | `balance.rs:153` | **Earns it** | the fold, in `BigUint` because `Decimal` has no subtraction |
| `Effect` | `balance.rs:116` | **Accidental** | **zero occurrences of the bare name anywhere outside `sdk/indexing/src`.** It is `pub` only because `pub fn ValueMovement::effect` returns it, and `effect` has exactly one caller in the workspace: `Ledger::apply` (`balance.rs:187`). Both should be `pub(crate)` |
| `Page`, `HistoryPosition` | `history.rs:20,53` | **Core** | the paging half of a history read |
| `HistoryQuery`, `CanonicalPage` | `history.rs:79,168` | **Earns it** | the `TransactionStore` request/response. External sites are `sdk/indexing`'s own tests only, because no other crate implements the optional port |
| `TransactionPage` | `history.rs:179` | **Core** | what `History::history` returns; 28 external sites |

### 2.2 The one network owner and its pass

| Item | file:line | Verdict | Note |
|---|---|---|---|
| `Service<S,I,R>` | `service.rs:41` | **Core** | the table's `Indexer`; 62 external sites |
| `delegate_indexer!` | `service.rs:~231` (155 lines) | **Earns it, with a caveat** | stands in for nine forwarding impls written three times. `030` measured it at 77 lines; it is now 155. It cannot be a type alias: bitcoin's `Indexer` needs an inherent `spendable` (`sdk/chains/bitcoin/src/indexer.rs`) and a foreign crate cannot write `impl Service<…>`. Its real cost is downstream of the trait surface, not of itself (§3.2) |
| `Progress`, `BlockWindow`, `ConfirmedLine` | `pass.rs:28,36,47` | **Core** | one pass's state. `BlockWindow` was `Lines`, renamed by `032` |
| `NativeBlocks<B>` | `fetch.rs:40` | **Core** | `032`'s "any native-block cache is private to the fetching path", built. Keyed by whole `BlockRef`, so an unwound block's bytes cannot come back under the hash that replaced it |
| `NetworkAdmission`, `SyncPlan`, `CommitPermit`, `PublicationPermit` | `admission.rs:53,198,257,299` | **Earns it, verified** | each of the four handshake steps has a named test (`admission.rs:488,511,534,562`). `DEFECT_PATTERNS.md:226-232` is a standing warning about deleting exactly these |
| `Listeners` | `listeners.rs:14` | **Earns it** | a list not a slot, synchronous so a hook cannot slow a pass |
| `BlockCommit`, `Listener` | `commit.rs:12,42` | **Earns it** | `Listener::committed` fires only on `Applied`, never `AlreadyApplied`, so a replayed tip does not re-notify |
| `Backfilling` | `backfill.rs:38` | **Earns it** | one method (`cancel`), and it must be a durable write rather than a `SyncControl` signal because `SyncControl` holds no store |
| `SyncPolicy`, `SyncConfig` | `policy.rs:26,110` | **Earns it** | two bounds on work. `SyncConfig` is a validated `{network, policy}` pair; its value is that `new` refuses a zero batch synchronously |
| `SourceError` | `error.rs:11` | **Earns it — checked, negative result** | it looks exactly like the `NetworkFailure` that `032` deleted: `{message, retryable}` against `IndexError`'s `{kind, message, retryable}`. It differs in the one way that mattered: `NetworkFailure` had no invariant stopping any `IndexError` becoming one, while `From<SourceError>` (`error.rs:101`) always yields `kind: Source`, so a chain adapter physically cannot classify its own failure as `InvalidBlock` or `Conflict`. 71 external sites |
| `IndexError`, `IndexErrorKind` | `error.rs:24,35` | **Core** | 226 and 138 external sites; the most-used types in the crate |

### 2.3 Ports

**Read this table as of the start of the audit.** The lane has since deleted the
four `Backfill*` rows and moved `IndexStore`, `Purging` and `PurgedRows` into
`storage/commit.rs`; §0.4 maps each one. The verdicts stand as the reasoning
that predicted the outcome.

| Item | file:line | Verdict | Note |
|---|---|---|---|
| `WatchStore`, `WatchProgress` | `storage/watch.rs:45,25` | **Core** | table rows |
| `IndexStore` | `storage/block.rs:104` | **Core**, three methods where the table wants one | `checkpoint` is the per-network line §1.1 argues must stay |
| `BlockAddition` | `storage/block.rs:27` | **Core** | the write precondition, separated from the facts by `032` step 4 |
| `BlockOutcome` | `storage/block.rs:261` | **Core** | `Applied{unknown_spends}` / `AlreadyApplied` |
| `Repository` | `storage.rs:55` | **Core** | table row; used only as a bound, 7 sites |
| `Purging`, `PurgedRows` | `storage/block.rs:202,244` | **Earns it, misfiled** | one production consumer each (`apps/server/src/rescan.rs`), and `PurgedRows`' five fields exist to be formatted into one log line (`rescan.rs:107-111`). It is a consumer capability like `BalanceStore`, and the other three each have their own file — it sits in `storage/block.rs` because nothing moved it |
| `TransactionStore` | `storage/history.rs:6` | **Core** (consumer) | a 12-line file for one trait. Justified only if the stated rule is applied everywhere, and it is not — §5.8 |
| `BalanceStore`, `BalanceQuery` | `storage/balance.rs:73,15` | **Core** (consumer) | `BalanceQuery::validate` is `HistoryQuery::validate` with one word changed — §5.9 |
| `OutputStore`, `OutputRequest`, `OutputPage`, `OutputCursor` | `storage/output.rs:66,20,56,10` | **Core** (consumer), **misplaced vocabulary** | `src/history.rs:5-6` states the rule ("read vocabulary lives beside the trait that names it, not inside the store port"), and these three break it while `reads.rs:140` implements `OutputStore` for `Service` — they are demonstrably shared index-read vocabulary |
| `BackfillProgress` | `storage/backfill.rs:28` | **Core**, pending the model change | `{address, start, resume, anchor}`. It is `WatchProgress.coverage`, i.e. the table's `indexed_through` in its not-yet-contiguous form |
| `BackfilledBlock` | `storage/backfill.rs:82` | **Accidental** | `CanonicalBlock` (`block.rs:147`) minus the `network` field that `BackfillBatch` hoists to batch level. **Zero occurrences of the name outside its own file** — the four store impls consume it structurally via `for block in batch.blocks()`. Dies with the write-path collapse |
| `BackfillBatch` | `storage/backfill.rs:104` | **Accidental in type, core in one method** | against `BlockAddition` it differs only in N-vs-1 blocks and a non-`Option` checkpoint. Its `changes()`/`settle()` whole-chunk output fold (`:234,:255`) survives as behaviour, because a chunk routinely spans an output's whole life |
| `BackfillOutcome` | `storage/backfill.rs:274` | **Accidental** | `unknown_spends` is field-for-field `BlockOutcome::Applied::unknown_spends` and is produced by the *same* function on both paths (`Tables::spend`, called at `sqlite/blocks.rs:137` and `sqlite/backfill.rs:94`). `written` is self-declared dead: "**Reported, never acted on**" (`:282`) — verified, **zero production readers**, every reader is a contract scenario |

### 2.4 Reads, health, runner

| Item | file:line | Verdict | Note |
|---|---|---|---|
| `Checkpoint`, `History`, `Balances` | `indexer.rs:11,23`, `balance.rs:276` | **Earns it, verified** | `032` §9 verified the split worthwhile: Solana's `Recovery` holds `Arc<dyn Checkpoint>` and `Arc<dyn History>` as two separate fields (`sdk/chains/solana/src/sender/recovery.rs:21,22`), and a `History` that does not return the checkpoint beside its rows makes transaction absence unprovable |
| `Indexing`, `Indexer` | `indexer.rs:48,84` | **Core** | minimum vs complete surface; 36 and 40 external sites |
| `Composer` | `composer.rs:29` | **Core** | the table's `Index` |
| `SyncUnit` | `composer.rs:38` | **Probably accidental** | zero occurrences outside `sdk/indexing/src`. `Composer::units()` is called only from this crate's own `supervisor` module (`supervisor.rs:75`, `supervisor/control.rs:25`, `supervisor/unit.rs:42,93`). But `032` states "a caller needing each network's outcome separately drives `Composer::units`" as a contract, so demoting it to `pub(crate)` retracts a documented affordance. **Leave, and record the contradiction** |
| `SyncStatus`, `SyncPhase`, `BackfillStatus` | `sync.rs:48,39,13` | **Core** | one pass's result |
| `NetworkState`, `NetworkHealth`, `SyncHealth` | `health.rs:16,29,76` | **Earns it, with a duplication named** | `SyncStatus` and `NetworkHealth` share five fields exactly (`network`, `checkpoint`, `pending_tip`, `observed_tip`, `backfill`), copied verbatim at `supervisor/unit.rs:183-195`, and `NetworkState` is `SyncPhase` plus two variants. It sits at 5-of-7 / 5-of-9 overlap, just under `rules."rust/duplicate-entity-base".min_shared_fields = 3`'s companion thresholds. The distinction is real (a pass result vs a cached standing condition, and `health.rs` compiles without the `supervisor` feature) but the five-field copy is not — §5.12, deferred |
| `Supervisor`, `SyncControl`, `SyncMonitor`, `NetworkEvent`, `Unit` | `supervisor.rs:31`, `supervisor/{control,monitor,unit}.rs` | **Earns it** | feature-gated, and `cargo check -p indexing --no-default-features` passes without it. 13 + 4 + 7 + 8 external sites, all `apps/server` and examples |
| `Schedule`, `Schedules` | `schedule.rs:15,29` | **Earns it** | deliberately outside the `supervisor` feature: a cadence is a `Duration` and a map, and a caller driving `sync` itself needs one |
| `NetworkRules` | `network.rs:12` | **Accidental, verified** | exactly one impl (`network.rs:33`, for `base::Network`). **Never a bound, never `dyn`, never a field or parameter type.** All six mentions in the workspace are the definition, the impl, the `pub use`, and three import lines in the chain interpreters that exist only to bring the methods into scope. `require_chain` has three production callers, `select_addresses` three. `indexing` itself calls neither. It buys method syntax and nothing else — but see §5.10, the fix may be forbidden by another rule |
| `IndexedBlock`, `ChainTip`, `BlockSource`, `BlockInterpreter` | `source.rs:11,25,48,74` | **Core** | what a chain implements; `032`'s "chain adapters own native RPC, parsing and finality" |

### 2.5 The SQLite backend and the in-memory store

| Item | file:line | Verdict | Note |
|---|---|---|---|
| `sqlite::Database` | `storage/sqlite.rs:94` | **Earns it** | file handle + pool; one production consumer (`apps/server/src/storage.rs:44,62`) |
| `sqlite::Repository` | `storage/sqlite.rs:132` | **Earns it** | takes no network and cannot fail; every table keyed `(chain, network, …)`. Five inherent methods and ten port methods, nine of which rename (§3.6) |
| `Tables` | `storage/sqlite/tables.rs:38` | **Accidental in scope** | one field, 67 methods, eight partial impls across eight files, nine of them static. It is three hats: the `(chain, network)` key, the row-level query surface of every port, and a stateless codec namespace (§0.3) |
| `Place` | `storage/sqlite/tables.rs:34` | **Accidental** | `pub(super) type Place = (i64, String)` meaning `(height, tx)`, **shadowed in a sibling** by `type Place = (String, i64)` meaning `(tx, index)` at `storage/sqlite/outputs.rs:27`. Both in scope in `sqlite/*`. `DEFECT_PATTERNS.md:118` §2.3, one name two concepts |
| `testing::Memory` | `testing/store.rs:62` | **Earns it, verified — a second implementation, not a double** | it implements six real ports with real errors, real atomicity (mutate a clone, install on success, `store.rs:88`), real cursors and the real `unmatched`-spend record. Its purpose is stated: "a trait that quietly required one backend's key framing or its conditional batch would fail here rather than a year later" (`store.rs:9-11`). Two crates depend on it in code — this crate's own tests and `sdk/wallets`' unit tests |
| `WatchContract` … `PurgeContract` (6) | `testing/{watch,history,balance,backfill,spends,purge}.rs` | **Earns it** | 45 scenarios, several of them proofs of fund-loss bugs (§3.3) |

---

## 3. The five things this was commissioned to settle

### 3.1 The 500-line cap — it has produced four files, and it is on the wrong axis

**Four files, not three, state the cap as their reason for existing.** All four
quoted verbatim:

- `testing/chunk.rs:4-5` — "Separate from `backfill.rs` **only because that
  file is at the repository's file-length cap**"
- `testing/coverage.rs:3-4` — "Separate from `store.rs` **only because that
  file is at the repository's file-length cap**"
- `testing/subjects.rs:3-4` — "Separate from `store.rs` **only because that
  file is at the repository's file-length cap**"
- `testing/addresses.rs:4-6` — "Separate from `store.rs` **for the same reason
  `coverage.rs` is — that file is at the repository's length cap**"

`testing/addresses.rs` was the one not previously found. It is also the worst
of the four on a second count: **its name predicts addresses and it contains
the `WatchStore` impl for `Memory`** (`addresses.rs:17`) — `DEFECT_PATTERNS.md`
§2.1. All four names (`chunk`, `coverage`, `subjects`, `addresses`) are named
for a subject rather than as fragments because `linter.toml:79`
(`reject_numbered_fragments`) and `:87-89` (banned `helper`, `shared`, `util`)
forbid admitting what they are. **The naming rules are what hide the split the
length rule caused.**

`testing/store.rs:1-4` states the whole split in its own module doc: "the
checkpoint, the forward commit, history and outputs here, the balance fold in
`subjects.rs`, the backfill in `coverage.rs` and the watch list in
`addresses.rs`." **One type, four files, 856 production lines.**

One of the four claims is **factually stale**: `testing/chunk.rs:4-5` says
`backfill.rs` "is at the cap". `testing/backfill.rs` is 345 of 500 — 69%, with
155 lines of headroom.

#### The merge arithmetic

| Merge | Sibling | Helper | Total | vs 500 |
|---|---:|---:|---:|---|
| `chunk.rs` → `backfill.rs` | 345 | 265 | 610 | +110 ✗ |
| `subjects.rs` → `store.rs` | 437 | 66 | **503** | **+3** ✗ |
| `addresses.rs` → `store.rs` | 437 | 168 | 605 | +105 ✗ |
| `coverage.rs` → `store.rs` | 437 | 185 | 622 | +122 ✗ |
| all three → `store.rs` | 437 | 419 | **856** | +356 ✗ |

Every merge fails on the enforced metric, and `subjects.rs` fails by **three
lines** — against a duplication the same module contains (`store.rs:157-170`
and `coverage.rs:174-182` are the same ten-line spend-application loop). A rule
whose binding margin is three lines against a known duplicate is not measuring
anything.

#### Which files qualify for an exception, and what the exception is

The mechanism already exists and is already used eight times in this
repository: `// linter:disable <rule> -- <reason>`
(`apps/api/src/api/mod.rs:50,74`, `apps/api/src/api/asset.rs:262`,
`apps/server/src/config.rs:31`, `packages/json-rpc/src/endpoint.rs:310`,
`packages/http-server/src/runtime.rs:29`,
`sdk/chains/solana/src/sender/registration.rs:9`,
`sdk/chains/solana/src/sender/stub.rs:215`). **A `linter.toml` glob exception is
therefore the wrong answer** — a per-file config target is invisible from the
file, while a directive states its reason where the next reader is.

**Not verified:** every existing use of `linter:disable` is on a span-scoped
rule. Whether it applies to a whole-file rule like `rust/file-length`, and
whether the directive must sit at line 1, is unknown. The cheap test is one
directive plus one `check` run; do that before planning around it.

Two files qualify, on the test "is this one idea that a reader would open as
one thing":

| File after merge | Lines | Why it is one idea |
|---|---:|---|
| `testing/store.rs` (absorbing `subjects`, `coverage`, `addresses`) | ~835 | `Memory` — one type, six port impls, one set of tables. Its own module doc already describes the four parts as one store. Splitting a store by which port it answers is splitting by caller, not by idea |
| `testing/backfill.rs` (absorbing `chunk`) | ~595 | `BackfillContract` — eleven scenarios and the one five-block fixture every one of them writes first |

**The deeper finding, which no exception fixes.** `AGENTS.md:515-516` says
"Production and tests must still be organized cohesively; numbered fragments
and `include!` do not create boundaries." A 500-line cap on contract scenarios
caps **invariants per file**, which is the wrong axis: `testing/spends.rs` is
458 lines for eight scenarios, one of which is the proof that stops a
permanently-spendable phantom UTXO (§3.3). The next fund-loss bug that needs a
ninth scenario there will be met by a rule, and the rule will be satisfied by
another apologetic file. Either `src/testing/**` joins the layout rule's test
globs (`linter.toml:60-67`), or `rust/file-length` grows a second target block
for it. Both are `DEFECT_PATTERNS.md` §3.4 — work done to satisfy a checker
rather than a caller.

#### The same rule's opposite failure

Three real duplications sit **under** the linter's thresholds and are
consequently invisible to it:

| Pair | Shared fields | Rule that should fire |
|---|---:|---|
| `CanonicalBlock` (`block.rs:147`) / `BackfilledBlock` (`storage/backfill.rs:82`) | 4 of 5 vs 4 of 4 | `rust/duplicate-entity-base` (`min_shared_fields = 3`) — silent, probably because `CanonicalBlock`'s fields are private |
| `SyncStatus` (`sync.rs:48`) / `NetworkHealth` (`health.rs:29`) | 5 | `rust/duplicate-entity-base` — 5 of 7 and 5 of 9 is under `rust/wire-domain-model-duplication`'s `min_overlap_percent = 75` |
| `HistoryQuery` (`history.rs:79`) / `BalanceQuery` (`storage/balance.rs:15`) | 2 + an identical `validate` | under `min_shared_fields = 3` |

### 3.2 The root's 28 files — the reading-order problem is real, and `engine/` is not the fix

The complaint holds. `sdk/indexing/src` root mixes two kinds of file with no
signal which you are opening:

**Vocabulary a reader starts from** (~2,460 production lines): `indexer.rs`,
`watch.rs`, `holder.rs`, `source.rs`, `observation.rs`, `block.rs`,
`output.rs`, `history.rs`, `balance.rs`, `error.rs`, `sync.rs`, `health.rs`,
`commit.rs`, `policy.rs`, `schedule.rs`, `network.rs`, `lib.rs`.

**Machinery implementing it** (~2,870 production lines): `service.rs`,
`pass.rs`, `reads.rs`, `catchup.rs`, `backfill.rs`, `fetch.rs`, `overlay.rs`,
`admission.rs`, `watches.rs`, `listeners.rs`, plus `watches/set.rs`.

**Do not reintroduce `engine/`, and do not rename it.** `DEFECT_PATTERNS.md`
§2.2 records why it died: "11 files, 2,800 lines, all of them serving one sync
pass. The module doc had to explain *'How a pass actually runs'* because the
name could not." Any new directory over the machinery half has the same
problem and the same test to fail: can the name say what kind of thing is
inside, without a sentence?

Two answers that do not need a directory, and one that needs proof:

1. **The `lib.rs` re-export block is already the index, and it is unordered.**
   `lib.rs:43-71` lists 28 `pub use` lines alphabetically by module. Grouping
   them under four comments — *what a chain implements*, *what a store
   implements*, *what an application calls*, *what the pass owns* — gives a
   reader the map at the one place they arrive, for zero lines and zero moves.
   Nothing in `linter.toml` forbids it.
2. **The machinery half is not 11 files of machinery; it is one owner.**
   `service.rs`, `pass.rs`, `reads.rs`, `catchup.rs`, `backfill.rs`, `fetch.rs`
   are all `impl Service<S,I,R>` — 1,747 production lines of one type's methods
   across six files, which is what a 500-line cap does to a 1,747-line owner.
   The honest statement for a reader is in `service.rs`'s module doc, and it is
   already there (`service.rs:3-7`): "the state is here, the procedure that
   advances it is beside this file… One owner, three files about it." It now
   undercounts: it is six.
3. **The trait surface is what makes the root expensive, not the file count.**
   Nine methods across seven traits (`Checkpoint` 1, `History` 1, `Balances` 1,
   `Watching` 3, `Backfilling` 1, `Indexing` 3) are forwarded **twice** — once
   by `Composer` routing on network (`composer.rs:154-302`, ~149 lines) and
   once by `delegate_indexer!` for each chain newtype (`service.rs:~231-366`,
   ~155 lines). **304 production lines of pure delegation, 5.4% of the root.**
   Every method added to any of the seven traits costs ~22 more. Note that the
   seven-trait split is itself partly the linter's:
   `rules."rust/trait-method-count".max_methods = 3` makes a single ten-method
   `Indexer` illegal. Three of the seven (`Checkpoint`, `History`, `Balances`)
   were verified worthwhile independently (`032` §9, Solana recovery); the
   remaining split is the rule's.

`catchup.rs` at 106 lines was checked specifically. It is two methods
(`catch_up`, `initialize_checkpoint`), both the forward phase, and
`pass.rs` + `catchup.rs` = 542, so the cap forbids merging it anyway.
More importantly it earns its place on symmetry: `backfill.rs` is the other
phase at 337 lines, and `pass.rs:9-14` names the pair. **Not a cap artefact in
the sense that matters — it would stay separate under any cap.**

### 3.3 `testing/` — judged on scenarios, it is the best-earned code in the tree

3,471 production lines, 45 public scenarios across six contracts, one second
store implementation. Judged on whether the scenarios duplicate each other:

**These are proofs, and they name what they caught.** Each quote is verbatim:

- **The permanently-spendable phantom UTXO.** `testing/spends.rs:172-181`:
  "nothing will ever present that spend again — **an inserted output would stay
  selectable for good, and coin selection would offer it**", and at the struct
  level `spends.rs:18-22`: "**A store that forgot the miss would fill the
  creation in and leave an output nothing can ever spend.**" Its counterparts
  are `testing/store.rs:164-169` and `testing/coverage.rs:148-152`. The
  scenario runs its chunk **twice** (`spends.rs:206`, `for redone in [false,
  true]`) because a chunk whose outcome was lost is redone.
- **Fund loss on a withdrawal-funded address.** `testing/backfill.rs:266-268`:
  "Without it a withdrawal-funded address watched from its own past would come
  back **permanently short, and nothing would say so**."
- **Mutation-verified store shape.** `testing/history.rs:136-144`: "A store
  whose cursor were an offset, a row count or a snapshot handle **would pass
  every other scenario here and fail this one**." `testing/spends.rs:216-222`:
  "a store that suppressed every historical create of an output *any* spend had
  ever named **would pass that one and break this one**."
- **Restart after an uncertain write.** `testing/watch.rs:107-110`: "a repeat
  that reset progress **would make every uncertain registration re-read the
  chain**."
- **Fee double-charging.** `testing/balance.rs:68-70`: "the two rules differ by
  exactly the fees the holding paid, **never by twice one of them**."
- **A scenario load-bearing for the *absence* of production code.**
  `testing/watch.rs:173-177`: blank-address refusal at both entries is what
  lets the pass carry no check of its own — this is the scenario that
  authorised deleting `SyncConfig::validate_filters`.

**These are duplication, and it is fixture code rather than scenarios:**

| Duplication | Sites | Lines |
|---|---|---:|
| `fn block(height) -> BlockRef`, identical body | `testing/history.rs:392`, `testing/balance.rs:392`, `testing/purge.rs:269`, `testing/chunk.rs:244`, `testing/spends.rs:408`, **and it escaped into `storage/sqlite/backfill.rs:229`** | ~72 |
| four byte-identical members shared by `HistoryContract` and `BalanceContract` (`prepare`, `blocks`, `block`, the three address helpers) — `diff` empty | `history.rs:259,305,392,406` ≡ `balance.rs:227,265,392,406` | ~55 |
| `draft` and `output` fixtures, identical after parameter plumbing | `spends.rs:394,422` ≡ `purge.rs:240,254` | ~30 |
| `history()` / `outputs()` read helpers, same shape, differing only in `limit` | `chunk.rs:179,197`, `spends.rs:374,359`, `purge.rs:188,206` | ~40 |
| "record coverage with an empty chunk", identical down to the panic string | `watch.rs:305`, `purge.rs:182` | ~15 |
| the spend-application loop inside `Memory` | `store.rs:157-170`, `coverage.rs:174-182` | ~10 |

**Three scenarios are genuinely redundant** and can go after a mutation check:
`balance.rs:163 an_unindexed_holding_is_zero` (the second half of
`refuses_a_holding_it_cannot_own`, `:192-213`, asserts the same triple for a
foreign-network address); the tail of `spends.rs:144
a_reported_miss_commits_the_rest_of_its_block` (re-asserts `:45` at block 2,
adding only "an earlier output is untouched"); the second half of
`backfill.rs:281-288` (the redo invariant `:103` already pins).

**These look duplicate and are not — keep all of them:**

- `history.rs:203 pages_equal_the_selected_history` does **not** subsume
  `:57`/`:77`/`:95`/`:114`. Its expected value is built by `selected()`
  (`history.rs:249`), which calls the *production* `HistoryQuery::select`
  (`src/history.rs:134`), so it cannot fail on a `select` defect. The four
  literal scenarios are the only things pinning `select`'s semantics.
- `backfill.rs:169` vs `:238` assert the same two lines through
  `IndexStore::backfill` and `IndexStore::commit`. That the two paths now share
  one rule is the *point* of the pair (`backfill.rs:234-237`), and it stops
  being a proof the moment only one path is checked.
- `spends.rs:45` vs `:70` differ by one bit: `OutputKey` is address-qualified,
  so a key that misses because the owner differs is a distinct case.
- `history.rs:114` vs `balance.rs:86` assert one invariant through two
  independent read paths.
- The four network-isolation assertions (`watch.rs:255`, `backfill.rs:291`,
  `balance.rs:192`, `purge.rs:111`) cover four different row families.

**One scenario is not a contract and should be made one.** `watch.rs:255
answers_only_its_own_network` has exactly one caller
(`tests/repository_contract.rs:493`, i.e. `Memory` only) and is **absent from
SQLite's `contract!` list** (`storage/sqlite/watches.rs:387-397`, which names
the other nine). SQLite asserts the same thing in a private test instead
(`storage/sqlite/watches.rs:406`). By this repository's own definition —
`storage/sqlite/watches.rs:357-359`, "a port that passes here is the same port
by the only definition this repository has" — it is not a contract. Add it to
the list and delete the private duplicate.

**One claimed affordance is unexercised.** `testing.rs:16-18` says "a chain
crate or an engine test can also use it wherever it would otherwise reach for a
database and a temporary directory." No chain crate does: `Memory` has zero
occurrences across `apps/`, `sdk/chains/**`, `packages/` and `examples/`. Only
`sdk/indexing`'s own tests and `sdk/wallets`' unit tests
(`sdk/wallets/Cargo.toml:18`) enable the feature.

**Three stale references to deleted things:**
`tests/service_contract.rs:1751-1752` names
`indexing::testing::RegistryContract::{…, refuses_a_blank_address,
refuses_another_network}` — `RegistryContract` no longer exists and
`refuses_another_network` is not a scenario in the module.
`docs/design/030:446` shows `HistoryContract::new(MyStore::empty()).run()` —
there is no `run()`.

**One misattached doc comment**, an extraction artefact of the `chunk.rs`
split: `testing/chunk.rs:22-26` puts `filled`'s doc line on `replay` and then
references a parameter `replay` does not have, leaving `filled`
(`chunk.rs:41`) undocumented.

> **Verdict on `testing/`: the scenarios are not duplicative; the fixtures
> are.** About 220 lines are copy-paste fixture, three scenarios out of 45 are
> redundant, and the rest are the definition of "two stores are the same
> store". The subtree's size is a function of how many invariants it pins, and
> two of those invariants are fund loss.

### 3.4 `reads.rs` / `balance.rs` / `history.rs` / `output.rs` — the shape *is* shared

Checked directly. **This is a negative result: the four reads are not four
copies.**

The shape is `sample the overlay → read the store → narrow the sample by the
store's own checkpoint → fold`, and its load-bearing half is **one function**:
`Service::above` (`reads.rs:238`), called by all three reads at `reads.rs:62`
(history), `:96` (balance) and `:148` (outputs). Its doc comment
(`reads.rs:217-237`) states why each rule alone is a defect, and `032` records
that moving the sample after the store read still fails two named tests.

The two folds are genuinely different folds: `PendingValue::fold`
(`overlay.rs:247`) produces amounts, `LiveChanges::fold` (`overlay.rs:289`)
produces outputs and spent keys.

The four files divide by **role**, not by read: `reads.rs` holds the four trait
impls on `Service`; `balance.rs`, `history.rs` and `output.rs` hold vocabulary
and validation. What duplication exists is small and specific:

| Duplication | Sites | Lines |
|---|---|---:|
| the `while_backfilling` gate, verbatim | `reads.rs:64-66`, `reads.rs:98-100` | 6 |
| `HistoryQuery::validate` / `BalanceQuery::validate` — identical bodies, one word different in the message | `history.rs:120-129`, `storage/balance.rs:36-48` | ~25 |
| `PendingValue` declared in `history.rs:215`, its `fold` implemented in `overlay.rs:232-268` | two files, one type | 0 |

One misplacement worth naming: `src/history.rs:5-6` states the rule that read
vocabulary lives beside the trait that names it rather than inside the store
port, and `storage/output.rs` breaks it while `reads.rs:140` implements
`OutputStore` for `Service`. That is §5.8.

### 3.5 `observation.rs` — the three-stage argument still holds, with one seam

`032:110-118` argued the three transaction stages stay three types because
their identically-named fields do not mean identical things. **Verified after
the recent changes**, field by field:

| | `ObservationDraft` `:295` | `CanonicalTransaction` `:205` | `ObservedTransaction` `:280` |
|---|---|---|---|
| `transaction_id` | must match the syncing network, checked at `:314` | network-qualified, validated | same |
| `status` | `ObservationDraftStatus` `:378` — `Included \| Failed{reason}`, **no block** | `CanonicalStatus` `:187` — `Included{block} \| Failed{block,reason}` | `TransactionStatus` `:254` — `Pending{block,confirmations} \| Confirmed{…} \| Failed{block,reason}` |
| `movements` | unvalidated, **must be empty when failed** — enforced at `:343-351` | "Every stable value movement, native and token" (`:211`) | "The movements of the queried asset only" (`:286`), produced by `HistoryQuery::select` retaining one asset (`history.rs:143-144`) |
| `fee` | unvalidated | validated by `NetworkFee::validate` (`:165`) | same value, carried |

The three status enums are three distinct state spaces, so folding into one
`Transaction<Status>` would share three field declarations and erase the
movements distinction. No caller is generic over the stage — checked. **The
argument holds.**

**One seam it does not cover.** `CanonicalStatus::Failed { block, reason }` and
`TransactionStatus::Failed { block, reason }` are field-for-field identical,
and the conversion at `reads.rs:324-326` is a pure re-wrap. Three lines. Not
worth a change, but it is the one place the "no two stages mean the same thing"
claim is literally false, and a reader who finds it will doubt the rest of the
argument. Say so in the doc comment rather than leave it to be found.

### 3.6 The rename tell, in both stores

`DEFECT_PATTERNS.md:64-66`: "when a trait method delegates to an inherent
method whose only difference is the name, ask *why the name had to change*."
`032` used exactly this to delete `Index::{load, watch, unwatch, cancel}`.

**SQLite: nine of ten port methods rename.** Verified, all ten:

| Port method | Delegates to | Renamed? |
|---|---|---|
| `IndexStore::checkpoint` `sqlite/blocks.rs:51` | `Tables::checkpoint` `tables.rs:83` | no |
| `IndexStore::commit` `blocks.rs:63` | `Tables::apply` `blocks.rs:114` | yes |
| `IndexStore::backfill` `blocks.rs:75` | `Tables::fill` `sqlite/backfill.rs:69` | yes |
| `Purging::purge_network` `blocks.rs:96` | `Tables::rescan` `blocks.rs:237` → `Tables::purge` `tables.rs:146` | yes, twice |
| `WatchStore::add` `sqlite/watches.rs:39` | `Tables::register` `watches.rs:147` | yes |
| `WatchStore::all` `watches.rs:56` | `Tables::observations` `watches.rs:127` | yes |
| `WatchStore::forget` `watches.rs:70` | `Tables::withdraw` `watches.rs:214` | yes |
| `TransactionStore::list` `sqlite/history.rs:36` | `Tables::page` `history.rs:54` | yes |
| `OutputStore::list` `sqlite/outputs.rs:30` | `Tables::outputs` `outputs.rs:44` | yes |
| `BalanceStore::balance` `sqlite/balance.rs:28` | `Tables::folded` `balance.rs:43` | yes |

The names had to change because `Tables` wears three hats (§0.3) and two of
them collide with the port vocabulary. **The fix removes no lines** — each
wrapper does real work (validate, hop to a blocking thread, open a
transaction) — so this is a naming finding, not a reduction. Give the `Tables`
method the port's name; `Tables::purge` / `Tables::rescan` /
`Purging::purge_network` are three names for one operation at three altitudes.

**`Watches`: ten forwarders, and they are load-bearing — a negative result.**
`Watches` (`watches.rs:30`) is `{network, admission, RwLock<Set>, policy,
loaded}`, and ten of its methods are one line each:

| `Watches` | body | `Set` |
|---|---|---|
| `filters` `:90` | `Ok(self.read()?.filters())` | `set.rs:54` |
| `resolve` `:96` | `self.write()?.resolve(…); Ok(())` | `set.rs:110` |
| `coverage` `:104` | `Ok(self.read()?.coverage())` | `set.rs:139` |
| `record` `:113` | `self.write()?.record(advanced); Ok(())` | `set.rs:250` |
| `backfilling` `:120` | `Ok(self.read()?.backfilling(address))` | `set.rs:172` |
| `rows` `:131` | `Ok(self.read()?.watches())` | `set.rs:72` — **rename tell** |
| `attach` `:192`, `owing` `:306`, `identified` `:311`, `detach` `:317` | same shape | `set.rs:272,151,77,288` |

Three of the doc comments point at the `Set` method they forward to
(`watches.rs:97`, `:105`, `:121`), which is the author saying the wrapper adds
nothing. It looks exactly like the `Index` wrappers `032` deleted.

**It is not the same thing.** `watches.rs:6-11` states the invariant: the hot
set stays in memory because `NetworkAdmission::plan` captures it "through a
*synchronous* closure and holds no guard across an await, while
`WatchStore::all` is async." The forwarders are the only thing making "no `Set`
guard escapes into an async context" **structural** rather than a review rule.
Exposing the guard would let a future caller hold it across an await, which is
the deadlock the design is written to prevent. The reduction is therefore to
compress the forwarders while keeping the guard inside — §5.7 — not to delete
them.

---

## 4. Things that look accidental and are load-bearing

Collected so the next reader does not re-investigate. Each was checked.

1. **`NetworkAdmission` and the three permits** (345 lines). `032` §6 verified
   each of the four handshake steps and `DEFECT_PATTERNS.md:226-232` records an
   agent proposing their deletion on the §3.6 test and being wrong: machinery
   coordinating two concurrent *actors* is not machinery coordinating two
   *owners*.
2. **The ten `Watches` forwarders.** §3.6 above. They are the structural half
   of "no lock guard crosses an await".
3. **`SourceError`** (`error.rs:11`). Shaped exactly like the deleted
   `NetworkFailure`, but `From<SourceError>` always yields `kind: Source`
   (`error.rs:101`), so a chain adapter cannot misclassify its own failure.
   `NetworkFailure` had no such invariant. 71 external sites.
4. **`AddressFilter`** (`watch.rs:10`). Not a projection of one `Watch`: it is
   one address's *union* of observations at the earliest birthday any of them
   names (`watches/set.rs:54`). `tests/minimum_contract.rs:552` is a
   compile-assertion that the type stays nameable.
5. **`catchup.rs` at 106 lines.** The forward phase of a symmetric pair, and
   `pass.rs + catchup.rs = 542` forbids the merge anyway.
6. **`bitcoin::Indexer`'s second field.** `pub struct Indexer<C, R>(Service<…>,
   R)` (`sdk/chains/bitcoin/src/indexer.rs:87`) looks like
   `DEFECT_PATTERNS.md` §1.1's courier — a repository cloned at construction
   and handed nowhere, forcing an `R: Clone` bound. **It is read**, at
   `indexer.rs:138`: `Indexer::confirmed()` hands back the durable store alone,
   as opposed to `spendable()`'s overlay-folded view. Not a courier.

   **This entry was wrong, and the field is now deleted.** The read exists, but
   it is one hop of a longer courier chain: `confirmed()`'s only caller made it
   `WalletFactory`'s `settled: Arc<UtxoReader>`, copied onto every `Wallet`,
   and nothing ever read it for an output or a balance — only
   `Dependency::of(&self.settled)` for pointer identity in the origin
   fingerprint. `wallets::Wallet` has no balance method; every balance is
   `indexing::Balances` over the same rows. So the whole chain — the second
   field, `confirmed()`, the `confirmed` argument, both `settled` fields and
   `Fingerprint::settled` — went, and `Indexer` is a plain newtype with no
   `R: Clone` bound. **The lesson:** §1.1 asks whether a value is read *for its
   purpose*, not whether the field is touched at all; being compared for
   pointer identity is not a purpose.
7. **`testing/`'s 3,471 lines.** §3.3. Two of its scenarios are the only thing
   standing between the tree and a fund-loss bug.
8. **`Subject`** (`holder.rs:27`). A two-field struct with no behaviour, and 46
   external sites across seven crates. It is the canonical keyless `Holder`.
9. **`Schedule` outside the `supervisor` feature.** Looks like a feature-gate
   leak; is the opposite — `032` records that moving it out is what lets a
   third-party graph of `indexing` plus the chain crates pull in no `rusqlite`
   and no Tokio task pool.
10. **The `delegate_indexer!` macro** (155 lines). Cannot become a type alias:
    bitcoin needs inherent methods on the newtype, and a foreign crate cannot
    write `impl Service<…>`.
11. **`storage/history.rs` at 12 lines.** Defensible *if* the stated rule is
    applied everywhere. §5.8 applies it rather than deleting the file.

---

## 5. The plan, ordered by lines removed per unit of risk

Each step lands independently with gates green. **Budget** is production lines;
negative numbers remove. Steps 1–4 and 8–9 touch no correctness invariant.
Step 11 is the in-flight lane's and nothing here depends on it.

| # | Step | Budget | Files | Risk |
|---|---|---:|---:|---|
| 1 | Dead surface and stale docs | −5 | 0 | none |
| 2 | Un-split the two cap-driven splits | −36 | −4 | none¹ |
| 3 | `testing/` fixture deduplication | −185 | 0 | none |
| 4 | Retire three redundant scenarios; promote one to a contract | −75 | 0 | low |
| 5 | Flatten `ValueMovement` | −110 | 0 | **medium — invariant** |
| 6 | SQLite mechanical deduplication | −57 | 0 | low |
| 7 | Compress the `Watches` forwarders | −45 | 0 | low — **concurrency invariant** |
| 8 | Consumer-port file reorganisation | −42 | −3 | none |
| 9 | One asset/address chain check | −22 | 0 | low |
| 10 | `NetworkRules` → two free functions | −18 | 0 | low, probably blocked² |
| 11a | ~~`git rm` the orphaned `storage/block.rs`~~ | −322 ✓ | −1 ✓ | done |
| 11b | Merge the SQLite write halves (the lane's remaining work) | −110 | −1 | **medium — one invariant** |
| | **total still open** | **−705** | **−8** | |
| | **total actually delivered** | **−171 code / −88 production** | **−3** | §7.1 |

¹ conditional on `linter:disable` working for `rust/file-length` — unverified.
² probably forbidden by `rules."rust/free-function".mode = "receiver"`.

**−705 of the current 13,373 is 5.3%**, on top of the −322 that already landed.
Of the open work, **−465 is safe** (1, 2, 3, 8 plus the low-risk 4, 6, 9),
**−130 is low-risk** (7, 10), and **−110 is medium** (11b), with 5 at −110
medium — call it **−240 medium**.

The largest single line in the original table was a `git rm`, and it landed
first. That is the honest summary of the exercise: the biggest safe reduction
available in `sdk/indexing/src` was deleting a file nothing compiled, and
everything after it costs review.

### 5.1 Dead surface and stale docs — safe, −5

> **Executed: −4.** Two of the four items were already fixed by the lane.


- `Effect` (`balance.rs:116`) and `ValueMovement::effect` (`balance.rs:134`) →
  `pub(crate)`. Zero occurrences of `Effect` outside `sdk/indexing/src`; one
  caller of `effect`, at `balance.rs:187`.
- Delete `tests/service_contract.rs:1751-1752`'s reference to
  `testing::RegistryContract` (the type does not exist) and to
  `refuses_another_network` (not a scenario).
- Fix `testing/chunk.rs:22-26`'s misattached doc comment; document `filled`
  (`chunk.rs:41`).
- Correct `testing/chunk.rs:4-5` — `testing/backfill.rs` is 345 of 500, not
  "at the cap".
- Record, without acting: `SyncUnit` (`composer.rs:38`) has zero external
  occurrences, but `032` states `Composer::units` as a caller-facing contract.
  Demoting it retracts a documented affordance.

**Gate:** `cargo check --workspace --all-features`, `plugin_linter check`.

### 5.2 Un-split `Memory` and `BackfillContract` — safe, −36, −4 files

> **Executed partly: −13, −1 file. The exception this step depends on does not
> exist in either form** — the directive does not apply to a whole-file rule
> and the config alternative is forbidden by `AGENTS.md`. §7.2 is what landed
> and why the merge arithmetic below had already gone stale.


Merge `testing/{subjects,coverage,addresses}.rs` into `testing/store.rs`
(~835 lines) and `testing/chunk.rs` into `testing/backfill.rs` (~595). Delete
the duplicated ten-line spend loop (`store.rs:157-170` ≡
`coverage.rs:174-182`). Four module headers become one.

**Do this first:** add one `// linter:disable rust/file-length -- <reason>` to
any file and run `plugin_linter check`. If the directive does not apply to a
whole-file rule, this step becomes a `linter.toml` second target block for
`src/testing/**` instead — worse, because a config exception is invisible from
the file, but it still lands.

**Gate:** `plugin_linter check` returns zero findings; `cargo test -p indexing
--all-features`.

### 5.3 `testing/` fixture deduplication — safe, −185

> **Executed: −81, in `testing/fixture.rs`.** Five copies of `fn block`, not
> six, and the last row of the table below names a duplication that does not
> exist in this tree. §7.3.


Every assertion preserved; only fixture construction moves.

| Change | Budget |
|---|---:|
| one `fn block(height)` for the six copies (`testing/{history,balance,purge,chunk,spends}.rs` + `storage/sqlite/backfill.rs:229`) | −58 |
| one shared fixture for `HistoryContract`/`BalanceContract`'s four byte-identical members | −55 |
| one `draft`/`output` pair for `SpendContract`/`PurgeContract` | −30 |
| one `history()`/`outputs()` read helper for the three copies | −30 |
| one "record coverage with an empty chunk" for the two copies | −12 |

Naming: the shared fixture cannot be called `helper`, `shared` or `common`
(`linter.toml:87-89`). `testing/fixture.rs` is one word and says what it holds.

**Gate:** `cargo test -p indexing --all-features` — the same 45 scenarios pass,
and `git diff --stat` shows no change inside any `assert`.

### 5.4 Retire three redundant scenarios, promote one — low risk, −75

> **Do not execute the deletions. All three claims were re-derived and none
> survived; the first was disproved by the mutation check this step makes
> mandatory.** The promotion landed. §7.4 — which also records a separate
> mutation-verified scenario found to be masked, and fixed.


Delete `balance.rs:163 an_unindexed_holding_is_zero` (subsumed by
`:192-213`), the redundant half of `spends.rs:144` (keep only its `live ==
[funding, later]` claim), and `backfill.rs:281-288` (subsumed by `:103`).

Add `answers_only_its_own_network` to SQLite's `contract!` list
(`storage/sqlite/watches.rs:387-397`) and delete the private
`registrations_survive_reopening_and_stay_in_their_network`
(`storage/sqlite/watches.rs:406`) — keeping only its reopen half, which the
contract does not cover.

**Risk is low but not zero, and the discipline is mandatory:** for each
deletion, delete the scenario, then mutate the invariant it named and confirm
another scenario fails. A deletion whose invariant survives unmutated is a
deletion of coverage, not of duplication. Three of the 45 scenarios are the
target; the other 42 stay, and §3.3 lists the five pairs that look duplicate
and are not.

### 5.5 Flatten `ValueMovement` — medium risk, −110

> **Executed: −47 code, +31 doc. The diagnosis below held in full.** What
> landed is stronger than "one validating constructor": five infallible
> constructors keep the arity guarantee at the type level for every producer,
> and one checked `stored` door serves the only caller that has columns rather
> than a call site. §7.5.


`ValueMovement` (`observation.rs:23`) is five variants each carrying `{id,
asset, amount}` plus zero to two addresses. The cost:

- six accessors, each a five-arm match over the shared fields:
  `id` `:84`, `asset` `:95`, `amount` `:106`, `kind` `:117`, `from` `:129`,
  `to` `:140` — **90 lines**.
- an enum↔row round-trip in the store, because **the stored shape is already
  flat**: `storage/sqlite/history.rs:280-312` rebuilds the enum from
  `(kind_tag, sender, recipient)` and `:443-451` destructures it straight back
  into the same `(Option, Option)` pair — **45 lines**.

The tell is that `base::MovementKind` already exists as the flat discriminant
and is already what the table stores (`ValueMovement::kind` at
`observation.rs:117` maps to it, `Tables::tag` at
`storage/sqlite/history.rs:321` stores it). The enum-with-payload is the second
representation of a vocabulary that already has a first.

Replace with `{ id, asset, amount, kind: MovementKind, from: Option<Address>,
to: Option<Address> }` plus one validating constructor.

**The invariant this moves, and it must move rather than vanish.** The enum
guarantees at the type level that a `Transfer`, `Mint` or `Burn` has its
endpoint and an `Input`/`Output` may not. The store's decoder relies on it
today: `storage/sqlite/history.rs:285-310` uses `endpoint(sender)?` (fallible,
required) for `Transfer`/`Mint`/`Burn` and `owner(sender)` (optional) for
`Input`/`Output`. After flattening, that becomes a checked constructor, and a
row that violates it must still be `Store`-class corruption rather than a
silently absent address.

**Stored bytes do not change.** The `movement` table already holds `kind`,
`sender` and `recipient` as separate columns (`storage/sqlite/schema.rs:93`),
so no migration is needed. Verify that claim against the schema before
starting.

Affected outside this crate: the three interpreters construct variants
(`sdk/chains/{bitcoin,ethereum,solana}/src/interpreter.rs`, 5+3+2 sites) and
`apps/api/src/api/history.rs` matches on four. Roughly neutral in lines,
non-trivial in review.

**Gate:** `cargo test --workspace --all-features`; the `HistoryContract` and
`BalanceContract` scenarios against both stores unmodified; a round-trip test
that a row with a `Transfer` kind and a null sender is refused.

### 5.6 SQLite mechanical deduplication — low risk, −57

> **Executed: +10. The −57 does not exist.** Two of the five items cite sites
> that are gone, and the rest replace a one-line idiom with a one-line call. It
> landed as a naming fix, which is what it was. §7.1, §7.7.


No behaviour change; four copy-paste sites become combinators.

| Change | Sites | Budget |
|---|---|---:|
| one `Database::snapshot` for the read-transaction preamble, including its four-times-repeated error string `"failed to begin a sqlite read"` | `sqlite/balance.rs:48`, `sqlite/outputs.rs:49`, `sqlite/history.rs:59`, `sqlite/watches.rs:128` | −20 |
| one `Tables::decimal(row, n)` for `Self::amount(&Self::column::<String>(row, n)?)?` | `sqlite/outputs.rs:149`, `sqlite/credits.rs:156`, `sqlite/history.rs:259,273` | −12 |
| one `Tables::position(row, n)` / `height(row, n)` for `BlockPosition(Self::restored(Self::column(row, n)?)?)` | `sqlite/backfill.rs:43,44`, `sqlite/outputs.rs:151`, `sqlite/tables.rs:220,227,228` | −15 |
| route `sqlite/watches.rs:277,294`'s raw `i64::try_from` / `u64::try_from` through `Tables::stored` / `Tables::restored` (`tables.rs:177,182`), which exist for exactly this and produce different error messages — one of the two current spellings is wrong | 2 | −10 |
| rename the shadowing `type Place` in `sqlite/outputs.rs:27` (`(tx, index)`) so it stops colliding with `tables.rs:34`'s `(height, tx)` | 1 | 0 |

Deliberately **not** in this step: the 26 `(chain, network)` bind sites
(`sqlite/{credits,backfill,outputs,history,tables,watches}.rs`) and the 18
`WHERE chain = ?1 AND network = ?2` clauses. A statement builder would save
about one line each and would put a layer between `Tables` and SQL, which is
the opposite of what §0.3 needs.

Also deliberately not in this step: the nine renames of §3.6. They remove no
lines and they touch every port method, so they belong with step 11's rewrite
of those methods, not before it.

**Gate:** `cargo test -p indexing --all-features`; `tests/repository_contract.rs`
unmodified.

### 5.7 Compress the `Watches` forwarders — low risk, −45, concurrency invariant

> **Executed: −12, and the invariant is stronger** — the guard accessors are
> deleted outright, so a `Set` guard is unobtainable rather than merely unused.
> Nine forwarders, not ten. The −45 required publishing `Set` crate-wide, which
> trades this invariant for another. §7.6.


Replace the ten one-line forwarders (`watches.rs:90,96,104,113,120,131,192,
306,311,317`, about 60 lines with their doc comments) with two combinators:

```rust
fn with<T>(&self, read: impl FnOnce(&Set) -> T) -> Result<T, IndexError>
fn with_mut<T>(&self, write: impl FnOnce(&mut Set) -> T) -> Result<T, IndexError>
```

**The invariant must survive, and the signature is what preserves it.** Both
closures are synchronous (`FnOnce`, not an async fn), so no `Set` guard can be
held across an await — which is the property `watches.rs:6-11` is written
about and the only reason the ten forwarders exist. A signature taking a
future, or one returning the guard, loses it silently.

Not in scope: `Watches::load` (`:66`), `add` (`:153`), `remove` (`:202`),
`cancel` (`:236`) and `plan` (`:126`) do real work and stay as they are.

**Gate:** `cargo test -p indexing --all-features`, and specifically
`tests/watch_contract.rs` plus `admission.rs`'s four handshake tests
(`:488,511,534,562`). Mutation check: make one combinator `async` and confirm
it fails to compile against the `plan` closure — that is the proof the
invariant is still structural.

### 5.8 Consumer-port file reorganisation — safe, −42, −3 files

> **Executed: +2 lines, −3 files.** The structural claims held; the arithmetic
> assumed ~90 lines of load-bearing doc prose would be deleted with the merge.
> `storage/reads.rs` was the name used and trips nothing. §7.1.


Apply the rule `src/history.rs:5-6` already states, consistently.

- Move `OutputCursor`, `OutputRequest`, `OutputPage` from `storage/output.rs`
  to `output.rs`, beside `IndexedOutput` (`output.rs:35`) and the
  `OutputStore` impl that `reads.rs:140` gives `Service`.
- Merge the three consumer-port traits into one `storage/reads.rs`:
  `TransactionStore` (`storage/history.rs:6`), `BalanceStore`
  (`storage/balance.rs:73`), `OutputStore` (`storage/output.rs:66`) — 157
  lines of three files become about 65 of one, which is the grouping
  `storage.rs:10-19` already describes in prose.
- Move `Purging` and `PurgedRows` (`storage/block.rs:202,244`) into
  `storage/reads.rs` too: it is a consumer capability, and `storage/block.rs`
  is the one place that mixes a required port with an optional one — its own
  doc says so (`storage/block.rs:198`, "Deliberately **not** part of
  `Repository`").
- Merge `commit.rs` (45) and `listeners.rs` (44): `BlockCommit` + `Listener` +
  `Listeners` are one idea (the post-commit hook) at 89 lines against a
  500-line cap, which is `DEFECT_PATTERNS.md` §3.2's test.

Net: 28 root files → 27, 7 `storage/` files → 5.

**Gate:** `plugin_linter check` (watch `redundant-parent-name` and
`rust/module-name` on the new `storage/reads.rs` — `reads.rs` already exists at
the root, and whether two files of that name in one crate trip
`shared-affix` is unverified); `cargo test --workspace --all-features`.

### 5.9 One asset/address chain check — low risk, −22

> **Executed: −11, as a provided method on `Holder`.** Provided methods count
> toward `rust/trait-method-count`, so `Holder` is now exactly at the cap of
> three with no headroom. §6.


`HistoryQuery::validate` (`history.rs:120-129`) and `BalanceQuery::validate`
(`storage/balance.rs:36-48`) are the same body with one word different in the
message. Put the check where the pair lives — one function over `(&Address,
&AssetId)`, or a provided method on `Holder` (`holder.rs:12`), which both
query types already implement (`history.rs:90`, `storage/balance.rs:52`).

**The message becomes one instead of two, and `IndexError::message` reaches
the wire** (`apps/api/src/api/error.rs:218`). Checked: no test anywhere asserts
either string, so nothing breaks — but the new message must still name the
asset and the address, because that is what makes the 400 actionable.

Note the third `validate` in the family, `OutputRequest::validate`
(`storage/output.rs:33`), is a **different** check (address-belongs-to-network,
not asset-chain-equals-address-chain). Leave it.

### 5.10 `NetworkRules` → two free functions — low risk, −18, probably blocked

> **Executed: −20. It was not blocked.** `rules."rust/free-function".mode =
> "receiver"` was probed against exactly this shape and returned zero findings,
> so the reasoning below — that the rule forbidding the fix is why the trait
> exists — is wrong. §6.


`network.rs:12` is a two-method trait with exactly one impl (`network.rs:33`),
never used as a bound, never `dyn`, never a field or parameter type. Its six
mentions in the workspace are the definition, the impl, the `pub use`
(`lib.rs:55`) and three import lines that exist only to bring the methods into
scope. `030` §0.1 named it: the trait exists "only so two functions can be
written with a dot," because `IndexError` is in `indexing` and `Network` is in
`base`, and the orphan rule forbids an inherent impl.

Deleting the trait means `indexing::require_chain(&network, &chain)` and
`indexing::select_addresses(&network, &addresses)`, changing three import lines
in the chain interpreters. It is a breaking change to the public surface for 18
lines.

**Probably blocked by the linter.** `linter.toml:191-193` configures
`rules."rust/free-function"` with `mode = "receiver"`, and there is no
precedent in the repository for a `pub fn` whose first parameter is a
receiver-shaped reference — a grep over `sdk/*/src/*.rs` and
`packages/*/src/*.rs` for `^pub fn \w+\(\s*&` returns nothing. So the rule that
would forbid the fix is likely the reason the trait exists, which makes this a
second instance of `DEFECT_PATTERNS.md` §3.4 rather than a reduction.

**Verify before attempting:** write the two free functions in a scratch branch
and run `plugin_linter check`. If the rule fires, record that and leave the
trait, with a doc line saying why it is a trait. **Do this last, or not at
all** — it is the worst ratio on the list.

### 5.11 Collapse the two write paths — **landed during this audit**

This step was written as a plan and is now a record. The lane landed the index
half: `IndexStore` is `{ checkpoint, commit }` over an `IndexedBatch`
(`storage/commit.rs:61,298`), `Progress { watch, expected, indexed_through }`
(`:42`) is `032`'s sketch field for field, `storage/backfill.rs` is deleted, and
`WatchProgress::indexed_through` is one hash-verified `BlockRef`. §0.4 lists
every type that went. The predicted budget was **−408**; the measured effect on
the tree was **+32** (§0.3a), because the old 292-line file became a 466-line
one carrying the validation that used to be split across two constructors.

**The evidence that justified it, kept as the record of why:**

- **Eight helpers inside the SQLite backend are each called exactly twice,
  once from the forward path and once from the historical one**: `rows`,
  `insert`, `refill`, `refill_credits`, `insert_credit`, `fill_outputs`,
  `spend`, `entries`.
- **Three twin methods differ by one call**: `Tables::write`
  (`sqlite/blocks.rs:176`) vs `Tables::refill` (`sqlite/backfill.rs:109`) —
  `conflict(…)` vs `self.verify(…)`; `Tables::credit` (`blocks.rs:196`) vs
  `Tables::refill_credits` (`sqlite/credits.rs:58`); `Tables::create`
  (`sqlite/outputs.rs:163`) vs `Tables::fill_outputs` (`outputs.rs:190`). Both
  bodies of the first pair loop over the same `Self::rows(transactions)`
  (`blocks.rs:181` / `backfill.rs:115`) and call the same `self.insert`.
- **`BackfilledBlock` is `CanonicalBlock` minus one field**, and its
  constructor is `CanonicalBlock::new` minus one check (below).
- **`BackfillOutcome::unknown_spends` is `BlockOutcome::Applied::unknown_spends`**,
  produced by the same `Tables::spend` on both paths (`sqlite/blocks.rs:137`,
  `sqlite/backfill.rs:94`), and `BackfillOutcome::written` has zero production
  readers.

**What is left, and it is the larger half by lines:**

| Where | Budget | Risk |
|---|---:|---|
| ~~`git rm sdk/indexing/src/storage/block.rs`~~ — **done by the lane during this audit**; it was 428 physical / 322 production and orphaned | −322 ✓ | none |
| `storage/sqlite/backfill.rs` 184 production — the eight twice-called helpers and the three `conflict`-vs-`verify` twin methods merge into their forward counterparts, now that there is one port method to serve | −110 | medium |
| the nine `Tables` renames of §3.6, done as part of that rewrite rather than before it | 0 | none |

**The two correctness invariants the collapse had to preserve, and how each was
settled — kept because the next person to touch this path needs both:**

1. **The unified rule had to be the historical path's insert-if-absent /
   verify-if-present, not the forward path's refuse.** `032` §7 proves the other
   direction: an uncertain write that is retried — which the design explicitly
   requires — would fail terminally on the first already-present row and
   quarantine the network over a block the store already holds. The SQLite half
   still has both spellings (`Tables::write` `sqlite/blocks.rs:176` refuses,
   `Tables::refill` `sqlite/backfill.rs:109` verifies), so **this invariant is
   the thing the remaining merge can still get wrong.**
2. **The duplicate-transaction-identity check.** Settled structurally, not by
   adding code: see §0.4. `IndexedBatch` takes `Vec<CanonicalBlock>`, so the
   weaker constructor no longer exists.

The whole-chunk output fold survives as behaviour of any multi-block batch: a
fill routinely spans an output's whole life, and a store applying a batch block
by block would create an output at block 3 that block 5 already spent
(`testing/backfill.rs:117-121` as it was; the scenario moved with the lane).

**Gate for the remaining half:** every `BackfillContract` and `SpendContract`
scenario against both `Memory` and SQLite **unmodified**, plus
`tests/{backfill,service}_contract.rs` and `tests/minimum_contract.rs`.
Mutation check: restore the refuse-on-present rule and confirm the replayed-block
scenario fails.

**Ordering against the rest of this plan.** Steps 3, 4 and 6 touch files this
work rewrites — `testing/backfill.rs`, `testing/chunk.rs`,
`storage/sqlite/backfill.rs` — and `testing/` already moved by 79 lines, so
**rebase those three onto the lane rather than racing it**. Steps 1, 2, 5, 7, 8
and 9 do not overlap and can start now; step 1 gains the `git rm` above.

### 5.12 Deferred: `SyncStatus` / `NetworkHealth`

> **Executed as this section asks: still deferred, overlap now recorded in both
> doc comments** (`health.rs`, `sync.rs`). +17 lines of reason.


`NetworkHealth` (`health.rs:29`) could hold a `SyncStatus` rather than copy its
five fields (`supervisor/unit.rs:183-195`), saving about 30 lines. It is
deferred, not rejected, for two reasons: `SyncStatus.phase` would then sit
redundantly beside `NetworkHealth.state` (which is `SyncPhase` plus
`Retrying`/`Quarantined`), and `apps/api/src/api/sync.rs:94` projects
`NetworkHealth`'s fields onto the `/v1/status` wire type, so the change is
wire-visible for a 0.2% saving. Record the overlap in both doc comments
instead, so the next reader does not read it as an oversight.

---

## 6. What was not checked — answered

The two questions §5 depended on, and the rest.

- **Does `// linter:disable` apply to `rust/file-length`? No.** Tested with a
  525-line probe file: the rule fires, the directive at line 1 does not suppress
  it, and the directive itself then raises a second finding, *"directive did not
  suppress any finding"*. **Whole-file rules take no directive.** The only
  remaining mechanism is a `linter.toml` target block, which `AGENTS.md`
  forbids in as many words — *"Never add blanket suppressions, weaken limits,
  broaden exclusions, or change policy merely to empty the report"*. So §5.2's
  exception does not exist in either form; see §7 for what was done instead.
- **Does `rules."rust/free-function".mode = "receiver"` forbid the
  `NetworkRules` replacement? No.** Tested with a `pub fn probe(network:
  &Network, chain: &ChainName)` in `network.rs`: zero findings. §5.10's
  "probably blocked" was wrong, and the step landed.
- **Do two files named `reads.rs` in one crate trip `shared-affix` or
  `rust/module-name`? No.** `storage/reads.rs` was tried first and the linter
  returned zero findings. The crate already shipped `src/commit.rs` beside
  `src/storage/commit.rs`, which was the same evidence unread.
- **Do provided trait methods count toward `rust/trait-method-count`? Yes.**
  Probed with a fourth method on `Holder`: *"trait `Holder` declares 4
  functions; maximum is 3"*, with the provided `validate` among them. `Holder`
  now sits **exactly at the cap with no headroom**, which is a constraint on
  §5.9's successor rather than a finding.
- **`sdk/indexing/tests/` (now ~5,700 lines) was still not audited for scenario
  duplication.** `tests/service_contract.rs` remains the largest single file in
  the crate. One defect *was* found in it by accident — see §7.4.
- **The in-file test blocks were counted but not read**, except where a
  conversion forced it. `reads.rs` (782 test lines for 365 production),
  `storage/sqlite/blocks.rs` and `admission.rs` remain the largest unexamined
  body of code in the crate.
- **`apps/api/src/api/signers.rs`** was reported as a 500-line-cap artefact and
  is still neither confirmed nor refuted.
- **A finding outside this audit's scope, recorded because it was found while
  executing it: `docs/design/030 §8 (ex-032).md` does not exist.** Not in the
  working tree, not staged, and `git log --all -- 'docs/design/030 §8 (ex-032)*'` is empty.
  **29 files cite it as an authority**, this document included — `docs/INDEXING.md`,
  `docs/SYSTEM_REQUIREMENTS.md`, `sdk/indexing/README.md`, eighteen
  `sdk/indexing/src` files, three of its integration tests and all three chain
  `config.rs`. Every claim in §1 attributed to "`032`'s table" is therefore
  unverifiable from the tree. The same is true of `021`, `024`, `019` and `012`,
  cited in source doc comments; `docs/design/` holds only `030` and this file.
  Either those documents were completed and removed without rerouting their
  citations, or the citations were always aspirational. **Nothing was changed
  about it** — it is a repository-wide docs problem, not this plan's.

---

## 7. Execution record

Executed in one pass on 2026-09-27 against the working tree, with three
sub-lanes over disjoint file sets. Gates at the end: **1,131 passed / 0 failed /
9 ignored** (baseline 1,129, +2 new tests), clippy `--workspace --all-targets
--all-features -D warnings` **0**, `RUSTDOCFLAGS=-D warnings cargo doc
--workspace` **0**, `scripts/check-chains.sh` **0**, `linter check` **0 findings
/ 8 suppressions**, `docs/openapi.json` sha256 **dd1a2163** (unchanged),
`cargo check -p indexing --no-default-features` clean.

### 7.1 What each step actually cost

Production lines, measured; "budget" is §5's estimate.

| # | Step | Budget | Actual | Outcome |
|---|---|---:|---:|---|
| 1 | Dead surface and stale docs | −5 | −4 | landed; two of its four items were already fixed by the lane |
| 2 | Un-split the cap-driven splits | −36, −4 files | −13, −1 file | **partly blocked** — §7.2 |
| 3 | `testing/` fixture deduplication | −185 | −81 | landed; §7.3 |
| 4 | Retire three redundant scenarios | −75 | **0** | **claim disproved** — §7.4 |
| 5 | Flatten `ValueMovement` | −110 | −47 code, +31 doc | landed, invariant strengthened — §7.5 |
| 6 | SQLite mechanical deduplication | −57 | **+10** | landed as a naming fix; budget was arithmetic that did not exist |
| 7 | Compress the `Watches` forwarders | −45 | −12 | landed, invariant strengthened — §7.6 |
| 8 | Consumer-port reorganisation | −42, −3 files | +2, −3 files | landed; the files went, the lines did not |
| 9 | One asset/address chain check | −22 | −11 | landed on `Holder` as a provided method |
| 10 | `NetworkRules` → free functions | −18, blocked? | −20 | landed; **not blocked** |
| 11b | Merge the SQLite write halves | −110 | — | **already landed** by the lane before execution began; `storage/sqlite/backfill.rs` is gone and `storage/sqlite/progress.rs` is new |
| 12 | §5.12's deferred overlap | 0 | +17 doc | recorded in both doc comments, as §5.12 asked |

**−705 was the promise; −171 lines of code and −88 production lines is the
result, with +92 lines of recorded reason against it.** Every budget that
assumed a duplicated *spelling* costs duplicated *lines* was wrong by roughly
the same factor: replacing a one-line idiom repeated at three sites with a
one-line call at three sites removes the idiom, not the lines. The budgets that
held were the ones counting whole duplicated *bodies* (§5.3, §5.5).

### 7.2 The 500-line cap: settled, and not the way §5.2 proposed

The directive does not work file-scoped (§6) and the config alternative is
forbidden by `AGENTS.md`. **So the cap was not excepted. It was obeyed, and it
still distorts this subtree.** What landed:

- `testing/subjects.rs` → `testing/store.rs`. This is the merge §5.2 wanted and
  the one file that still named the cap as its only reason; the arithmetic that
  made it fail *"by three lines"* had gone stale — `store.rs` had shrunk from
  437 to 314 when the lane extracted `commits.rs`, so the merge lands at **369
  of 500** with headroom. The ten-line duplicated spend loop §5.2 also targeted
  was already gone with the same extraction.
- `testing/addresses.rs` → **`testing/observations.rs`**. The misnaming §3.1
  called "the worst of the four" is fixed: the file holds the `WatchStore` impl
  and its rows are observations, which is now what it is called.
- `testing/{chunk,commits}.rs` **stay separate, and their module docs now say
  why honestly.** `chunk.rs` + `backfill.rs` is ~730 lines for twenty-one
  invariants; the merge is illegal and no exception is available. `chunk.rs`'s
  doc no longer claims a stale fact and states the §3.1 argument instead.
- Two members (`watch`, `checkpoint`) moved from `backfill.rs` to `chunk.rs`,
  because §5.3's receiver rewrite pushed `backfill.rs` to 506 and over the cap.
  They are fixture setup and `chunk.rs` is the fixture file, so the move is
  cohesive — **but it is also the rule reshaping the code again, which is §3.1's
  whole point.**

§3.1's deeper finding therefore stands unfixed and unfixable from inside a file:
**a 500-line cap on a contract file caps invariants per file.** The next
fund-loss bug needing a ninth `spends.rs` scenario will still be met by a rule.
The only remaining routes are a `linter.toml` target block for `src/testing/**`
(forbidden as policy today) or adding `**/testing/**` to the layout rule's test
globs. Both need a human decision, not an agent's.

### 7.3 `testing/` fixture deduplication landed, at less than half the budget

`testing/fixture.rs` is new: one `Fixture<'a, R>` holding `{repository,
network}`, which all six contracts now hold instead of those two fields. It
carries the synthetic chain (`block`, `facts`, `chain`, `batch`, `commit`), the
naming (`address`, `transaction`, `native`, `token`), the two shared fixture
bodies (`paid`, `output`) and the two shared reads (`history`, `outputs`).

- `fn block(height)` existed **five** times identically, not six: the copy §3.3
  said "escaped into production at `storage/sqlite/backfill.rs:229`" died with
  that file. The three remaining copies outside `testing/` are all inside
  `#[cfg(test)]` blocks and are not production.
- `HistoryContract`/`BalanceContract`'s shared members were confirmed
  byte-identical and are now one each; `prepare` is one line.
- `PurgeContract` gains the clearest improvement: it held two `Network`s and
  passed them to static helpers, and now holds **two `Fixture`s** — `purged` and
  `elsewhere` — so `Self::history(self.repository, &self.network)` became
  `Self::history(&self.purged)`. 270 → 196 lines.
- `SpendContract`'s own `draft` and `output` turned out to be byte-identical to
  `PurgeContract`'s after the plumbing, exactly as §3.3 predicted, and both are
  now `Fixture::paid` / `Fixture::output`.

Why −81 rather than −185: the shared fixture's own module doc and eleven
signatures cost 204 lines, which the budget did not count. In lines of *code*
the subtree went 2,663 → 2,566.

`testing/watch.rs` was left alone. §5.3's last item — *"record coverage with an
empty chunk", identical down to the panic string, at `watch.rs:305` and
`purge.rs:182`* — **does not exist in this tree**; neither file has such a
helper. That citation predates the lane.

### 7.4 §5.4's redundancy claims did not survive the discipline §5.4 demanded

All three claimed-redundant scenarios were re-derived. **None was deleted**, and
the first was disproved by the mutation check §5.4 made mandatory.

- **`balance.rs an_unindexed_holding_is_zero` is not subsumed.** §3.3 said its
  triple is re-asserted by `refuses_a_holding_it_cannot_own`'s second half. It
  is not: that half never calls `prepare()` and asserts `observed_at == None`
  for an address of *another* network, while this scenario asserts `observed_at
  == Some(block(8))` for an empty holding of *this* network after the checkpoint
  moved. Those are opposite claims — "zero because nothing happened" against
  "zero because nothing has been looked at". **Mutation run:** make
  `Memory::folded` return no checkpoint when the fold saw no rows.
  `an_unindexed_holding_is_zero` **fails and is the only test that does** —
  51 pass. It is the sole guard of that invariant.
- **`spends.rs a_reported_miss_commits_the_rest_of_its_block`**: §3.3's saving
  is ~6 lines of a 26-line scenario, and its `history == [block-1, block-2]`
  claim (that a block with an unmatched spend still writes its history rows at
  block 2, above an existing output) has no other home. Not worth the risk;
  left.
- **`backfill.rs:281-288`**: the citation does not map. The nearest current code
  is inside `refuses_a_batch_whole_when_one_block_disagrees` and asserts the
  atomicity of a *refused* batch, which is a different invariant from
  `redoes_a_batch_without_duplicating_it`'s successful redo. Left.

The promotion half of §5.4 **did** land: `answers_only_its_own_network` is now
in SQLite's `contract!` list (`storage/sqlite/watches.rs`), and the private
`registrations_survive_reopening_and_stay_in_their_network` is now
`registrations_survive_reopening_the_file`, keeping only the two properties the
port cannot state — that a row survives reopening the file, and that an identity
is unique across every network in it.

**A separate defect was found while verifying the coordinator's second
mutation-verified scenario, and fixed.**
`tests/minimum_contract.rs::an_index_of_one_network_reads_only_that_network_out_of_a_shared_store`
**passed with `WatchStore::all`'s network filter deleted.** `Watches` mirrors
the store in memory and reads it exactly once, so two indexes that registered
their own observations never consult `all` again — the store's filter was never
exercised. The scenario now drops and rebuilds both indexes before reading the
watch lists, which forces `Watches::load`, and the mutation fails again. The
first mutation (drop every `indexed_through` from `all`) failed correctly
throughout. **A test that passes when the store lies is worse than no test, and
this one had been that for as long as the mirror has existed.**

### 7.5 `ValueMovement` is flat, and the invariant got stronger rather than weaker

§5.5's diagnosis was right on every count that mattered. The enum was a second
representation of `(kind, from, to)`: the store already holds those as three
columns, **the three chain interpreters already start from that triple and
convert up**, and **no consumer anywhere destructures a variant** — all of
`apps/api`, `overlay.rs`, `history.rs`, `balance.rs` and the SQLite writer read
only the six accessors.

What landed is not §5.5's "one validating constructor", which would have made
the arity a runtime check for everybody:

- `ValueMovement { pub id, pub asset, pub amount, kind, from, to }`, with the
  triple private.
- **Five infallible constructors** — `transfer(…, from: Address, to: Address)`,
  `input(…, owner: Option<Address>)`, `output`, `mint(…, to: Address)`,
  `burn(…, from: Address)`. A transfer still cannot be built without both
  addresses, a mint without its recipient, or a burn without the address it
  shrank. **The type-level guarantee the enum made is kept at the type level for
  every producer**, which is what §5.5 warned must not vanish.
- **One fallible door, `stored(…)`**, for the only caller that has three
  independent columns rather than a call site: the SQLite decoder. It refuses
  every pairing the five constructors cannot express, as
  `IndexErrorKind::Store`, non-retryable — the classification §5.5 required.
- A new test, `a_stored_movement_whose_endpoints_disagree_with_its_kind_is_corruption`,
  pins all eight refused pairings and round-trips all seven accepted ones.

`Tables::movement`'s 33-line reconstruction is now a six-argument read, and
`Tables::endpoints`' five-arm match is gone. **Stored bytes are unchanged** —
verified: the `movement` table's `kind`/`sender`/`recipient` columns were
untouched and `schema.rs` is not modified. Net: −47 lines of code across seven
crates, +31 lines of doc recording why the representation is one.

### 7.6 The two properties §4 said not to lose

- **The `Watches` forwarders.** There were **nine**, not ten, and three of
  §3.6's named methods (`resolve`, `coverage`, `record`) no longer exist under
  those names. All nine now route through `with`/`with_mut` taking a synchronous
  `FnOnce`, and — better than §5.7 asked — `Watches::read()`/`write()`, which
  handed out the guards, are **deleted**, so a `Set` guard is now unobtainable
  anywhere in the file rather than merely unused. **Mutation run:** making one
  combinator `async` fails to compile against `NetworkAdmission::plan`'s
  closure bound, which is the proof the property is still structural. Five
  forwarders were kept as named methods, because deleting them means publishing
  `Set` crate-wide, which would trade this invariant for the "durable write
  before hot-set mutation" one. −12 lines, not −45; the −45 required that trade.
- **`NetworkAdmission` and the three permits, `SourceError`, `AddressFilter`,
  `BlockCredit`, `Subject`, `Holder`, the `delegate_indexer!` macro,
  `catchup.rs`, `bitcoin::Indexer`'s second field, and every `testing/`
  scenario** were left exactly as they are. Nothing in §4 was touched.
- **`WatchProgress::caught_up` is still one comparison** (`storage/watch.rs`)
  with four callers, all in `watches/set.rs`, plus the external consumer in
  `examples/custom_store/sink.rs`. The consumer-port reorganisation moved the
  file it lives in and did not spread the decision out.

### 7.7 One behaviour change was proposed, reviewed and reverted

The SQLite lane found that `Registration::of` and `Registration::into_watch`
spelled the same `i64`/`u64` narrowing two ways with two different error kinds,
and §5.6 was right that one of the two is wrong. It routed both through
`Tables::stored`/`restored`, which made a watch birthday above `i64::MAX`
`IndexErrorKind::Store` instead of `InvalidRequest` — a 500 instead of a 400 at
`apps/api/src/api/error.rs`.

**Reverted, deliberately.** The reasoning for `Store` was that `testing::Memory`
accepts the same request and the contracts call the two one store — but the two
stores disagree either way, so no error kind repairs it, and a caller can act on
a 400 where it can do nothing with a 500. The read side keeps the shared
spelling. **The real finding is that the contracts say nothing about the
storable range of `Watch::start`**, which is why the two stores differ at all;
it is now recorded in `Registration::of`'s doc comment and belongs in
`docs/findings.md`, not in an error kind.

### 7.8 What this document's own claims are worth, after execution

Held: §0 (the diagnosis — prose, contracts and one SQL backend outweigh the
engine three to one, and the tree does not give lines back when its structure
improves), §0.3 (`Tables` is the one genuinely bad number), §3.1's *deeper*
finding, §3.2, §3.3's verdict on scenarios versus fixtures, §3.4, §3.5, §3.6's
rename tell, and all eleven of §4's negative results. §5.5's diagnosis held in
full.

Did not hold: **every line-count budget in §5 except §5.3's and §5.5's shape**,
§5.4's three redundancy claims, §5.6's arithmetic, §5.8's arithmetic, §5.10's
"probably blocked", §5.2's merge arithmetic (stale rather than wrong), and
§3.3's "six copies" and "record coverage with an empty chunk" citations.

**The pattern in the failures is one pattern**, and it is the same one §0
diagnosed: a duplicated *spelling* was repeatedly counted as duplicated *lines*.
Four steps were sized by grepping for a repeated idiom and multiplying. That is
why §5 promised −705 and delivered −171, and it is the thing a successor audit
should not do again — **count bodies, not sites.**

### 7.9 Recommendation on this file

**Delete it from `docs/design/`, but not before moving three things out.** By the
convention this repository uses, `docs/design/` holds open design work and this
plan is spent: eleven steps executed, both open questions answered, nothing left
to do that an agent should do unasked. What is left in it that a reader will
want is not a plan:

1. **§4's eleven negative results and §3.6's rename tell** belong in
   `docs/DEFECT_PATTERNS.md`, which is the repository's standing recognition
   guide and already carries the `NetworkAdmission` false positive as a
   cautionary note. §4 items 2, 3, 6 and 9 are four more of exactly that.
2. **§3.1's deeper finding** — that a 500-line cap on a contract file caps
   invariants per file — is an **open** decision needing a human, and belongs in
   `docs/findings.md`'s Open section, with §7.2's record of why neither
   available mechanism works.
3. **§6's last bullet** — 29 files citing a `docs/design/030 §8 (ex-032)` that does not
   exist, and the same for `012`, `019`, `021` and `024` — is an open
   repository-wide finding and belongs in `docs/findings.md`.

Everything else in these 1,158 lines is either a measurement of a tree that has
since moved four times, a per-item verdict that would have to be re-derived
before it could be trusted, or a budget §7.1 has already corrected. Keeping it
in `docs/design/` would mean the next reader opens a plan and finds a history;
keeping it anywhere would mean maintaining a third copy of an argument
`DEFECT_PATTERNS.md` and `findings.md` will hold better. **The three extractions
above are the durable part. This file is the scaffolding.**
