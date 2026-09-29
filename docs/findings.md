# Findings

Open findings with `file:line`, and the record of what closed. This file is
tracking. It holds no design.

**The indexing design is not here.** It is
[`docs/design/030 §8 (ex-032).md`](fails/032-indexing-model.md) — the concepts
table, the two diagrams, the `WatchStore`/`IndexStore` contract, the proof of
mapping, the collisions and the transition plan — and it stays there while the
lane implementing it records progress in it. This file used to carry a verbatim
second copy of all of it under an "Agreed indexing design" heading. Two copies
of one design diverge and one of them gets read; the copy is gone. What the copy
held and `032` does not is kept below, under "Carried out of the deleted design
copy". The pointer stands because that is where the design belongs; the file
itself is not in the tree at this instant, which is the first entry under Open.

Reconciled against the working tree at `refactor/chain-unification` on
2026-09-27, with concurrent lanes editing `sdk/indexing/src` throughout the
reconciliation. Every closure below was re-read from the tree rather than taken
from the earlier record. Where the tree disagreed with the record, the tree won
and the disagreement is named.

**Citations inside `sdk/indexing` name a file and a shape, not a line**, because
that crate moved while this was written: `storage/{balance,history,output}.rs`
folded into `storage/reads.rs`, and `lib.rs`, `storage.rs`, `watches.rs` and
`watches/set.rs` each shifted by tens of lines within the hour. Citations
outside it are line-exact and were re-verified.

## Renames and deletions absorbed here

The stale-citation map, recorded once. Every path here is under
`sdk/indexing/src`, and `docs/design/033-indexing-core.md` §0.4 carries a longer
version of it that predates the SQLite half landing.

- **Deleted, with no occurrence of the name left under `sdk/`, `apps/` or
  `packages/`:** `Coverage { progress, stored }`,
  `BackfillProgress { address, start, resume, anchor }`, the per-address
  coverage map, the `anchor`, `BlockAddition`, `BackfillBatch`,
  `BackfilledBlock`, `BackfillOutcome`, `IndexStore::backfill`, `Synchronizer`,
  and the files `storage/backfill.rs`, `storage/block.rs`, `storage/registry.rs`
  and `storage/sqlite/backfill.rs`.
- **Progress** is one contiguous, hash-verified position per observation:
  `WatchProgress { watch, indexed_through: Option<BlockRef> }` in
  `storage/watch.rs`, with `next_required` and `caught_up` on it. The claim a
  batch carries is `Progress { watch, expected, indexed_through }` in
  `storage/commit.rs`. `Owed`, as §0.4 names it, is `Backlog` in
  `watches/set.rs`.
- **Renamed or moved:** `Composer` is `Index` (`index.rs`); the reads facade
  that used to be called `Index` in `index.rs` is `Service` plus the trait impls
  in `reads.rs`, so two unrelated findings that both cited `index.rs` now cite
  two different files. `BlockWindow`, `ConfirmedLine` and `Pass::window` are in
  `pass.rs`. `storage/sqlite/progress.rs` replaced the deleted SQLite backfill
  half. `Purging`, `PurgedRows` and the three consumer read ports are in
  `storage/reads.rs`.

## Open

- **`docs/design/030 §8 (ex-032).md` does not exist.** Not in the working
  tree, not staged, and not in any commit reachable from any ref —
  `git log --all -- 'docs/design/030 §8 (ex-032)*'` is empty and `docs/design/` holds
  `030-the-proposal.md` and `033-indexing-core.md` and nothing else. Forty-five
  references across twenty-seven files cite it as the authority for a decision:
  `docs/INDEXING.md` (eight), `docs/SYSTEM_REQUIREMENTS.md`,
  `sdk/indexing/README.md`, the module or item docs of eighteen
  `sdk/indexing/src` files, three of its integration tests, and each of
  `sdk/chains/{bitcoin,ethereum,solana}/src/config.rs`. This file's own header
  is one of them. `docs/design/033-indexing-core.md` audits `032` section by
  section, so its §0.4 stale-citation table and its `032`-row tables are the
  closest surviving description of what `032` said. Nothing below depends on
  `032` being restored; everything that cites it does.

- **The 500-line production cap has no exception mechanism, and it is reshaping
  `sdk/indexing/src/testing/`.** A contract file's length is its *invariants per
  file*, which is the wrong axis: `testing/spends.rs` is eight scenarios, one of
  which is the only thing stopping a permanently-spendable phantom UTXO, and
  `testing/backfill.rs` plus `testing/chunk.rs` is one `BackfillContract` split
  across two files at ~730 lines for twenty-one guarantees. Both mechanisms for
  excepting it were tested and neither is available: **`// linter:disable
  rust/file-length` does not suppress a whole-file rule** (the rule still fires
  and the directive raises a second finding, *"directive did not suppress any
  finding"* — probed with a 525-line file), and a `linter.toml` target block is
  forbidden by `AGENTS.md`'s *"Never add blanket suppressions, weaken limits,
  broaden exclusions, or change policy merely to empty the report."* The two
  remaining routes both need a human decision: a `src/testing/**` target block
  for `rules."rust/file-length"` as deliberate policy, or adding
  `**/testing/**` to the layout rule's test globs so scenario files stop
  counting as production. Until then the rule keeps moving members between the
  two files — `docs/design/033-indexing-core.md` §7.2 records one such move made
  to get back under 500. **Closed by `034` §4.1c**, which took the second of the
  two routes named above without needing a policy change: the scenarios moved to
  `sdk/indexing/tests/contracts/`, which the layout rule already counts as test
  code, so `rust/file-length` stopped counting them as production. The cap is
  unchanged and no suppression was added.

- **The store contracts say nothing about the storable range of
  `indexing::Watch::start`, and the two stores disagree because of it.**
  `memory::Repository` accepts any `u64` birthday; the SQLite backend cannot hold
  one above `i64::MAX` and refuses it as `IndexErrorKind::InvalidRequest`
  (`sdk/indexing/src/storage/sqlite/watches.rs`, `Registration::of`). No error
  kind repairs that — one store serves the request and the other does not — so
  it is a gap in `WatchContract` (`sdk/indexing/tests/contracts/watch.rs`), which
  is this repository's only definition of two stores being the same store. No chain produces a block
  position above 9.2x10^18, so nothing is broken today; what is missing is a
  scenario that states which answer is right. Recorded in `Registration::of`'s
  doc comment.

## Partly closed

- `sdk/indexing/src/observation.rs` — **the duplicated network is gone; the
  three stages were kept, with an argument.** `CanonicalTransaction`,
  `ObservedTransaction` and `ObservationDraft` carry no `network` field:
  `TransactionRef` is `{ network, id }`, so each of the three already names its
  network through its own identity, and `ObservationDraft::canonical` keeps the
  one check that matters (`transaction_id.belongs_to(network)`). The three stay
  three deliberately: no caller is generic over the stage, and their identically
  named fields do not mean identical things — a draft's `movements` are
  unvalidated and must be empty when it failed, a canonical transaction's are
  every movement it made, and an observed transaction's are only the queried
  asset's. Partly closed, not closed: the duplication half was answered by
  change, the three-stage half by argument.

## Closed

Each one re-verified in the tree, not inherited from the earlier record. Where a
closure is stronger than it was because the shape that made the defect
expressible has since been deleted, it says so.

- `overlay.rs` — **`Uncommitted` deleted**, with its `()` impl. No occurrence of
  the name survives anywhere in `sdk/indexing`. The owner is `Service`, whose
  `pending: Arc<Frontier>` calls `held()`/`sampled()` directly
  (`sdk/indexing/src/service.rs`), and a service with no pass holds an empty
  `Frontier` instead of a unit. The earlier record named `Index` as the holder;
  that role is `Service` now.
- `overlay.rs` — **`Frontier` no longer caches native blocks.** It lost its
  `<B>` generic and its `raw` field and holds
  `HeldBlock { facts: CanonicalBlock, selection: u64 }` — interpreted facts plus
  the address-filter revision they were produced against. The cache is
  `NativeBlocks<B>` in `sdk/indexing/src/fetch.rs`, private to the fetching
  path, still 16 entries (`RETAINED_BLOCKS`) keyed by the whole `BlockRef`.
- `index.rs` — **the two independent snapshots are one ordering**, and the code
  moved to `sdk/indexing/src/reads.rs`. `Service::above(held, checkpoint)` is
  called after the overlay is sampled and narrowed by the checkpoint the store's
  own page carried, at all three read sites: `history`, `balance` and the paged
  output read. The ordering argument is stated in the doc comment on `above`,
  and the interleaved-commit tests in `reads.rs` drive a real `Frontier` rather
  than a local double.
- `health.rs` — **`NetworkFailure` deleted.** No occurrence survives.
  `NetworkHealth::failure` and `BackfillStatus::failure` hold
  `Option<IndexError>` directly, which deletes both conversions. The HTTP
  surface did not move: `IndexFailure` still reads `kind`, `retryable` and
  `message`.
- `history.rs` — **`HistoryCursor` deleted from `indexing`.** No occurrence
  survives there; `Page::after`, `HistoryQuery::after`, `CanonicalPage::next`
  and `TransactionPage::next` carry `Option<HistoryPosition>`. `apps/api` keeps
  its own unrelated wire type of that name (`apps/api/src/api/cursor.rs:15`),
  which encoded only `{chain, network, transaction, height}` and is unchanged,
  so no encoded cursor moved — and the name means one thing per crate.
- `synchronizer.rs` — **`Lines` renamed `BlockWindow`**, and **closed harder:**
  `Synchronizer` itself no longer exists, so neither does the file the finding
  cited. `BlockWindow` and `ConfirmedLine` are in `sdk/indexing/src/pass.rs`,
  produced by `Pass::window`. Its own doc states what the rename bought: the
  confirmed block a pass may commit up to and the observed block it may read up
  to, with everything between held and never committed — a window, not a
  collection. The earlier record cited `docs/CHAIN_LAYOUT.md` section 5 for that
  naming rule; that document does not contain the word, so the citation is
  dropped rather than repaired.
- `storage/block.rs` — **`BlockAddition` split, and then deleted; closed
  harder.** `CanonicalBlock` (`sdk/indexing/src/block.rs`) is one block's
  validated facts and nothing else —
  `{ network, block, transactions, outputs, credits }`, built only by
  `CanonicalBlock::new`, where every content check lives: unique transaction
  identities, non-overlapping and uniquely identified output changes,
  network-bound movements and fees, `BlockCredit::validate_all`. The checkpoint
  precondition `BlockAddition` paired with it is now `IndexedBatch`'s own
  `expected_checkpoint`, formed at the write in
  `sdk/indexing/src/storage/commit.rs`. **The earlier record's closing sentence
  is retracted:** it called `BackfilledBlock`/`BackfillBatch` "the symmetric
  pair on the historical path", and they were not symmetric —
  `docs/design/033-indexing-core.md` §5.11 found `BackfillBatch::filled` running
  `CanonicalBlock::new`'s validation *minus* its duplicate-transaction-identity
  check, so a backfilled block with two transactions of one identity was
  accepted where a committed one was refused. That defect is fixed structurally:
  because `IndexedBatch::new` takes `Vec<CanonicalBlock>` and a `CanonicalBlock`
  can only come from `CanonicalBlock::new`, the weaker constructor no longer
  exists to inherit from. `IndexedBatch::new` adds three checks the old pair had
  nowhere for — one observation advanced twice in one batch, a claim above the
  confirmed line, and a claim below the value it expects.
- `policy.rs` — **`SyncConfig::validate_filters` removed**, with both per-pass
  call sites. The only surviving occurrence of the name is a comment at
  `sdk/indexing/tests/service_contract.rs:1741` recording where its invariants
  went. Those invariants are enforced where addresses enter the collection:
  network membership and non-blankness at each of the two entries —
  `Watches::validate` for `Watching::watch`, and the store contract for
  `Watches::load`, whose blank rule is pinned by `refuses_a_blank_address` in
  `sdk/indexing/tests/contracts/watch.rs`, run against both stores from
  `sdk/indexing/tests/repository_contract.rs`, beside the existing
  `refuses_another_network`. **Uniqueness is not structural and is not claimed
  to be:** `Set::rows` is keyed by watch id, `Set::filters` deduplicates by
  address explicitly (taking the lowest block any caught-up observation of that
  address still needs), and uniqueness is enforced per `(address, asset)` at the
  entry — `Set::holder` behind `Watches::existing`, which refuses with "this
  address is already watched for this asset".
- `watches.rs` — **`Watches::covered` is gone**, and **closed harder.** It is
  now `Watches::loaded`, one `AtomicBool` meaning "the durable rows have been
  read into the set", which is what its caller asks it. The earlier record in
  this file said it was renamed `coverage_loaded`; that name never existed. The
  per-address half of the question is gone with `Coverage`: a contiguous
  `indexed_through` cannot be in the state "worked out but not written", so
  there is nothing left for a second field to mean. Whether a watched address's
  history is covered is `WatchProgress::caught_up` per row, and
  `Watches::backfilling` — deliberately address-scoped, because stored rows are
  shared by every asset at one address — is what a read consults.
- `watches/set.rs`'s `adopt` — **closed harder still.** The earlier record
  closed this against `Set::attach(watch, anchor)`; that shape is itself gone.
  `Set::restored` is gone, `adopt` is gone, and `Set::attach(watch)` takes no
  anchor: it inserts `WatchProgress::unread(watch)`, inherits nothing and resets
  nothing, so a new observation owes its own range from its own birthday and
  every other observation keeps the claim it earned. There is no parked coverage
  to be claimed and no anchor to inherit, so the class of defect the finding
  named is no longer expressible. `Watches::add` still attaches synchronously
  under the publication permit whose checkpoint the birthday comes from, after
  the durable write; `Watches::load` still takes each watch beside its own
  progress from one `WatchStore::all` snapshot, merge-only for the watches and
  authoritative for the progress; `Set::detach` still drops a row and its
  progress together.
- `storage/redb/balance.rs` — **gone with the backend.** No `redb` module is
  declared in `sdk/indexing/src/storage.rs` and no `redb` directory survives;
  the only occurrence of the string in the workspace is a rejected-configuration
  fixture at `apps/server/src/storage.rs:90`. `storage/sqlite/balance.rs` reads
  the checkpoint and every movement page inside one deferred transaction with
  the checkpoint read as its first statement, so the fold and the checkpoint it
  reports are one snapshot; the history and output reads do the same, which is
  what `reads.rs` depends on.
- `storage/redb/backend/probe.rs` — **gone with the engine it instrumented**,
  and it has no SQLite counterpart.
- `sdk/base/src/transaction/claim.rs:27` — **`Claim` is now
  `Claim(BTreeSet<String>)`**, a report and never a lock. The names are opaque
  to the core and compared only for equality, so a chain may start claiming
  something new without this crate learning about it; the one rule — two sends
  collide when they take one thing in common — is `collides_with`
  (`claim.rs:43`), and what differs per chain is only what the chain puts in the
  set. No chain-specific reservation rule remains in the generic core.
- `packages/json-rpc/src/endpoint.rs`, `packages/json-rpc/src/http.rs` —
  **`Config` deleted; query strings work.** No `Config` type survives anywhere
  in the crate. `Endpoint` remains (`endpoint.rs:110`) and the provider's URL
  reaches the node unaltered, path and query included. `Endpoint::validate`
  (`endpoint.rs:163`) rejects only what is unusable — a non-`http`/`https` URL,
  a missing host, a fragment, a colliding or invalid header name, a malformed
  `${NAME}` reference, a zero bound — and no message echoes a URL or a value.
- `sdk/chains/solana/src/wallet.rs` — **receive-only assets expose no send.**
  `Wallet` and `Author` are split, and `WalletFactory::authorship`
  (`wallet.rs:312`) returns `Some` only for `AssetKind::Native`, so an SPL
  factory declares no `wallets::Authorship` at all. Pinned by
  `a_token_asset_is_refused_before_a_builder_exists`.
- `sdk/chains/solana/src/builder.rs` — **a native SOL draft for an SPL wallet is
  refused before a builder exists.** `TransferBuilder` is native by construction
  (`builder.rs:25`), a resumed snapshot is bound to wallet, network *and* asset
  (`builder.rs:123-131`), and the test above pins the refusal at
  `builder.rs:432`, with the defect it removed described at
  `builder.rs:421-427`.
- `sdk/wallets/src/composition.rs`, `sdk/wallets/Cargo.toml`,
  `sdk/chains/*/Cargo.toml` — **the bundled store and the supervisor are no
  longer forced on library consumers.** This was the file's one open finding and
  all three impositions it named are gone. **The Cargo defaults:** `sdk/wallets`
  (`Cargo.toml:12`) and all three chain crates
  (`sdk/chains/bitcoin/Cargo.toml:16`, `ethereum/Cargo.toml:20`,
  `solana/Cargo.toml:13`) take `indexing` with `default-features = false`, so
  nothing depending on `wallets` or on a chain crate compiles bundled rusqlite
  or tokio through them. The only member taking the defaults is the binary
  `apps/server` (`Cargo.toml:34`); `apps/api` (`Cargo.toml:15`) keeps its own
  `default-features = false`. **The supervisor:** `Schedule` and `Schedules`
  moved out from behind `#[cfg(feature = "supervisor")]` —
  `sdk/indexing/src/lib.rs` exports `schedule::{Schedule, Schedules}`
  unconditionally and gates only `mod supervisor` — so `Composition.schedules`
  names no gated type. **The store bound:** `Connection<A, R>`
  (`composition.rs:58`) carries none, and `Composition::build` asks only
  `R: Clone` (`composition.rs:116`); the minimum contract is
  `pub trait Repository: WatchStore + IndexStore` in
  `sdk/indexing/src/storage.rs`, blanket-implemented over `?Sized` so
  `Arc<dyn Repository>` works and `Clone` is not required.
- `sdk/wallets` no longer exports `Repository`, **and the chain crates caught
  up.** All three now
  `use wallets::{ComposedChain, Connection, SubmissionRegistrar}` and bound on
  `indexing::Repository` directly
  (`sdk/chains/{bitcoin,ethereum,solana}/src/config.rs:17`, with the
  `R: indexing::Repository` bound below it). The mid-move breakage the earlier
  record noted is resolved.
- `sdk/indexing/src/storage/registry.rs` — **deleted.** The orphaned
  `pub trait Registry` and `RegisteredAddress` are gone with the file, and no
  occurrence of either name survives in the workspace. `RegistryContract` is
  `WatchContract` in `sdk/indexing/tests/contracts/watch.rs`.
- **The incompleteness gate exists on both reads.** `reads.rs` calls
  `watches.backfilling(of.address(), line.as_ref())` and weakens the page's
  `Completeness` through `while_backfilling` in `history` and again in
  `balance`, against the same checkpoint the rows came with. The earlier record
  carried its absence in `history` as a live pre-existing defect.
- **The two-port split has landed.** `sdk/indexing/src/storage.rs` declares
  `WatchStore` and `IndexStore` as the required pair, `Repository` as their
  blanket-implemented union, and history, balance, outputs and purge as separate
  consumer capabilities in `storage/reads.rs` — with the one-consistent-read
  guarantee stated in the contract, on `WatchStore::all` and on `IndexStore`.
- **`purge_network` reaches progress.** It resets every
  `WatchProgress::indexed_through` of the network in the same transaction that
  removes the network's rows, which closed a gap neither of the two old write
  ports could express. It has no counterpart in the earlier record because there
  was no shape to record it against.

## Checked and not a finding

- `sdk/indexing/src/storage.rs` — the earlier entry said SQLite is gated behind
  `cfg(all(test, feature = "sqlite"))` and so exposes no experimental public
  backend. **That is not what the tree says**, and the conclusion changed with
  it: `pub mod sqlite` is gated on `#[cfg(feature = "sqlite")]` alone and
  `sqlite` is on by default. It is not experimental — it is the only store the
  crate ships, stated as such in `sdk/indexing/Cargo.toml`'s own feature
  comment, and being public is the point. Retired as answered rather than as
  inapplicable.

## Carried out of the deleted design copy

Two things the removed copy of the design held that
`docs/design/030 §8 (ex-032).md` does not. Both are corrections to `032`'s own
text, and `032` belongs to the implementing lane, so they are recorded here
rather than applied there — and while `032` is missing from the tree they have
no target at all, so this is the only place they exist.

- **`032`'s transition step 3 is stale in the step itself.** It reads "preserve
  frozen redb encodings, fixtures, legacy key export and unrelated application
  data rather than casually changing disk formats". The corrected text, which
  the removed copy carried: there is no frozen encoding to preserve any more —
  the store is one SQLite file with ordered schema steps (`MIGRATIONS`, counted
  by `PRAGMA user_version`, in `sdk/indexing/src/storage/sqlite/schema.rs`), and
  `docs/SYSTEM_REQUIREMENTS.md:26-33` revokes the durable-layout exception
  explicitly and in those words, together with the redb backend. `032`'s banner
  and its own collision 7 both say the redb-fixture advice is moot; step 3 was
  never rewritten to match. Verified: no `redb` module is declared in
  `sdk/indexing/src/storage.rs`, and the surviving definition of "two stores are
  the same store" is `sdk/indexing/tests/contracts/`, run against `sqlite` and
  `memory` as a pair per scenario.
- **"both durable format fixtures" is stale for the same reason**, in `032`'s
  required-proofs paragraph. There is one *durable* store and one set of
  contracts, in `sdk/indexing/tests/contracts/`.

## Contradictions with the tree, for the lanes

Found while reconciling. Each is something a document asserts that the code does
not, kept so that a restored `032` is not restored stale.

- **`032`'s §4 reads `Set::rows` as `BTreeMap<Address, Row>`**, in its verdict,
  its "Step 4, aside" table row and its collision 8. It is
  `BTreeMap<String, WatchProgress>`, keyed by watch id, and there is no second
  map beside it. Deduplication by address is explicit, in `Set::filters` and
  `Set::behind`.
- **The same-address multi-asset watch exists.** `Watches::existing` conflicts
  only on "this address is already watched for this asset", and the rows of one
  address advance together because one inspection of the address serves every
  asset observed at it. `032`'s §4 states the opposite as its verdict — that the
  hot set refuses it and drops the second row on restore — and names
  `apps/api/tests/route_contract.rs` as pinning the refusal. That capability
  landed; the fixture at that path builds a `Watch` per `(address, asset)` and
  refuses nothing of the kind.
- **`032`'s status banner reads "agreed design, not implemented".** Several of
  its load-bearing pieces are implemented, including its proof §1's missing
  one-consistent-read guarantee and the contiguous per-observation progress its
  "What did not land" section recorded as outstanding. Left to the implementing
  lane to restate, once the document is back.
