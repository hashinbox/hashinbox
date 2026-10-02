# 039: Maximal line reuse — one shape, many uses

> "I want to keep features while I want as little as code as possible while I
> want every line to be used as many times as possible, while keeping sound
> architecture."

Status: **proposal.** A five-way structural scan of the current tree —
`base`, `indexing`, `wallets` + `apps/server`, `chains/bitcoin` +
`chains/ethereum`, `chains/solana`, `packages` — asking one question: where
does one shape or one rule already exist in several copies?

The scan also produced a second list: findings that go too far. A
three-parameter wallet generic, chain error enums moved to `base`, a generic
signed-transaction model, macro-generated newtypes — all rejected on the
same rule: **two copies at a real boundary are not duplication**, and a
generic whose type parameters exist only to re-create distinctions the
names already carry is ceremony, not reuse. The surviving findings total
**roughly 500 lines**, ordered below by area and, inside each area, by
soundness. The keep-list (§2) is as important as the findings.

---

## 0. The method

- **Shape signature.** Every struct/enum reduces to its field types, names
  stripped: `(Option<BlockRef>, Vec<Progress>)`, `(Network, BlockRef,
  Vec<T>)`. One line per type.
- **Cluster.** Identical signatures cluster. Near-identical = differs by
  exactly one field or one type argument.
- **Same concept or distinct concept.** Same → unify; mechanism follows
  from what differs: type argument → generic parameter; struct-plus-facts →
  embedding; same body over different types → one generic function; same
  rule over different receivers → free function beside the governed type.
  Distinct → keep, and record the guarantee merging would lose.
- **Reject by default.** Three similar fields are not a shared entity. A
  shared primitive argument is not a concept. Two copies at a real
  boundary are the boundary's price, not a defect.

## 1. Findings

### 1.1 `sdk/indexing` + `sdk/base` — ~150 lines

Ordered safe-first.

1. **The movement tag vocabulary is encoded/decoded in three places**
   (`sqlite/movements.rs:39`, `effects.rs:172`, `activity.rs:100`). Two
   independent decoders of one column; `activity.rs` already spells
   Fee=5/Withdrawal=6 inline. **Fix:** `tag()`/`from_tag()` on the existing
   `Direction`/`ActivityKind` enums; all three call sites. ~25 lines.
   Correctness value over line value.
2. **`one_network` is byte-identical in both watch stores**
   (`sqlite/watches.rs:125` vs `memory/observations.rs:180`). **Fix:** one
   `pub(crate) fn` in `storage/`. ~16 lines. The batch-atomicity rule is
   port-level, not backend-level.
3. **Seven `evidence_*` columns spelled in four SQL statements**
   (`watches.rs:276`, `evidence.rs:38,91,109`). The `COLUMNS` const pattern
   already exists; add `EVIDENCE_COLUMNS`. ~20 lines.
4. **Memory backend rebuilds its balance key triple at six sites**
   (`memory/mod.rs:341`, `observations.rs:162,204`, `balances.rs:33`,
   `evidence.rs:14,35`). **Fix:** one `balance_key(holder)`. ~20 lines and
   the key shape gets a name.
5. **`Fold::apply` internally knows "did this touch the holding" but doesn't
   say so** (`balances.rs:123` re-implements the filter). **Fix:** return
   `bool`. ~12 lines; kills a double source of truth on fee settlement.
6. **`Subject` and `BalanceQuery` are the same struct with two names**
   (`holder.rs:75` vs `storage/reads.rs:139`). **Fix:** drop `BalanceQuery`;
   `BalanceStore::balance` takes `Subject`. ~20 lines + one concept gone.
7. **`HistoryQuery` and `ActivityQuery` are one struct with different cursor
   types** (`history.rs:191` vs `activity.rs:70`). **Fix:** `PagedQuery<C>`
   with two type aliases, or one shared `validate_paged`. ~35 or ~15 lines.
8. **Activity/evidence capabilities exist twice as inherent methods**
   (`sqlite/activity.rs:28` + `memory/activity.rs:15`, and the verify/record
   pair). Consumers already invent shim traits to talk about both. **Fix:**
   promote to ports beside `BalanceStore`. Line-neutral, uniform capability
   model.

### 1.2 `sdk/wallets` + `apps/server` — ~55 lines

1. **`Histories` / `Folds` are the same per-network router**
   (`composition.rs:261,283` — identical find-or-unconfigured bodies).
   **Fix:** shared `owner(network)` lookup (preferred) or one private
   `ByNetwork<C>`. ~18 lines.
2. **`Movement::new` / `Fee::new` repeat the atomic→display conversion**
   (`api/history.rs:68,106`; a fourth copy of the `units.of` lookup at
   `mod.rs:309`). **Fix:** one `denominated` free function; `Balance::new`
   uses it too. ~10 lines.
3. **`Error` embeds exactly `ErrorBody`** (`api/error.rs:59` vs `:14`;
   `IntoResponse` reassembles field-by-field). **Fix:** `Error { failure,
   body: ErrorBody }`. ~10 lines; kills field-order coupling.
4. **`"all"` amount parsing duplicated** (`api/transaction.rs:53` re-implements
   `wallets::sender::Amount`'s `FromStr`). **Fix:** use the SDK parser. ~5
   lines; one spelling of `"all"`.
5. **The wallet-publish pipeline is written three times**
   (`api/mod.rs:182,212`, `config.rs:194` — persist → watch → install, in
   that order). The ordering is a safety property living in three places.
   **Fix:** one `Payments::publish`. ~12–15 lines. Strongest of the six.
6. The 201-response tail ×2 (`registered`/`observed`). Folds into 5. ~4 lines.

### 1.3 `chains/bitcoin` + `chains/ethereum` — ~200 lines

The scan's large early candidates — a generic `wallets::AttachedWallet<Key,
Cfg, Deps>` for ~150 lines of verbatim wallet glue, a generic
`FactorySender`, chain error enums in `base`, a generic
`base::SignedTransaction<Id>`, a shared `IndexerParams`, a macro for
identity.rs — are **rejected** and recorded in §2. Each builds a machine to
save lines at a boundary the architecture says stays per-chain. What
survives is small, helpers-only, routed through crates both chains already
depend on:

1. **`BlockSource::blocks` range loop** (`source.rs:68,46` — identical
   modulo one error string). **Fix:** `indexing::walk_blocks(start, end,
   limit, fetch)`. ~25 lines.
2. **Interpreter scaffolding** (`interpreter.rs` both — `BlockInterpreter
   {network}`, identical `new`/`network()`, identical fee mapping).
   **Fix:** provided methods / a `NetworkScoped` helper in `indexing`. ~30
   lines.
3. **Verified `Client<C>` scaffolding** (`native/rpc/client.rs` both —
   `State{transport, network}`, manual `Clone`, `unverified`). **Fix:** a
   small wrapper in `json-rpc`. ~25 lines.
4. **Held-transaction ring** (`sender/reservation.rs` both — `HELD=64`,
   `hold`/`take` verbatim). **Fix:** small `wallets::Held<T>`. ~20 lines.
5. **Snapshot `NetworkBody` + network JSON + `invalid()`** (`builder.rs`
   both). **Fix:** one snapshot helper. ~20 lines.
6. **Ethereum internal: `UnsignedTransaction::new` vs
   `SignedTransaction::inspect_eip1559_fees` repeat the same three field
   rules with identical messages** (`unsigned.rs:19` vs `signed.rs:60`).
   **Fix:** one `validate_eip1559_fields`. ~15 lines; drift hazard.
7. **`senders()` (ethereum outbox) vs `payers()` (bitcoin outbox)** — one
   generic `group_by_sender`. ~15 lines.
8. **Duplicated wallet contract tests** (`secret_text_is_hexadecimal…`
   verbatim in both crates, and more). **Fix:** move to a `wallets`
   contract suite run per chain. ~50 lines. Tests move; the wallet glue
   they test stays per-chain.

### 1.4 `chains/solana` — ~40 lines

1. **Canonical Base58 "parse + re-render" at seven sites**
   (`id.rs:37`, `network.rs:87`, `block/mod.rs:179`, `preflight.rs:66`,
   `transaction.rs:284`, `address.rs:51`, `token.rs:155`). One generic
   `fn canonical<T: FromStr + ToString>`. ~20–25 lines; each site becomes
   one line. Strongest finding in the crate.
2. **Attributed net-sum loops ×3** (`movement.rs:99,118`, `token.rs:82`).
   One private generic `net_attributed`. ~12–14 lines.
3. **`Outbox::with_registrar` hand-clones 7 fields** (`outbox.rs:204`).
   `#[derive(Clone)]` + struct-update. ~8 lines.

### 1.5 `packages` — ~50 lines

1. **`json-rpc`: the `Work`/`Repetition` seam and the retry/failover loop**
   exist 4 times. One generic loop. ~50 lines. Everything else in the
   packages is protocol boundary or coincidence — no findings.

## 2. The keep-list — similar shapes that must stay separate

The scan's rejected candidates live here too, each with its reason.

**Rejected over-reach from this scan:**

- **Wallet/factory glue and `FactorySender`** (~200 verbatim lines across
  bitcoin/ethereum): the provider/sender adapter is chain-boundary
  machinery, the same category as the reservation family (`037` §3) — three
  per-chain implementations of a real boundary is not duplication in the
  reuse sense. A `wallets::AttachedWallet<Key, Cfg, Deps>` generic would
  carry three type parameters and blanket impls to re-create distinctions
  the four names already make for free. Not worth it.
- **Per-crate `Error`/`ErrorKind`**: chain failure vocabulary. `base` stays
  chain-free; moving kinds there is a leak, and a macro for constructors is
  worse than two readable files.
- **Native `SignedTransaction {id, envelope}`**: an opaque-envelope generic
  in `base` is a step toward a universal transaction model — explicitly
  out of scope.
- **`IndexerParams`**: chain-specific parameter sets; ~15 lines, low value.
- **`identity.rs` plumbing and `base` newtype macros**: macro-generated
  foundational types cost readability at the layer every crate reads.
- **Generic `Page<T, Cursor>`**: renames public fields to save ~25 lines.

**Standing keeps (from earlier scans, unchanged):**

- **Base identity types** (`Network`, `Address`, `AssetId`,
  `TransactionRef`): different namespace invariants; one generic would let
  a transaction id pass where an address is expected.
- **`BlockHeight` vs `BlockPosition`**: unit distinction.
- **`InterpretedBlock` / `CanonicalBlock` and the two statuses**: the
  validation boundary.
- **`Progress` / `OwedRange` / `WatchProgress` / `ForwardRange`**: four
  deliberate projections.
- **`Spend` vs `Input` (bitcoin)**: signing commits to a resolved output.
- **`Admission` / `Settlement` (all chains)**: typestate, different Drops.
- **Wire→domain twins and mapping-only `From`s**: the HTTP/SDK boundary.
- **Memory vs sqlite commit mechanics**: unified at the port-contract
  level — the sound place.
- **Backend-owned cursor encodings, `Funds` vs `Payer`, per-chain
  `BuildContext`, `Movement` vs `Transfer`, per-crate `TransactionId`
  grammars**: different ontologies or protocol texts.
- **`PendingValue` vs `MovementEffect`**: un-netted reporting vs sign
  projection.
- **Solana transfer family and `Reading<T>` non-generic members**: request
  vs observed vs index-space; slot semantics differ.

## 3. Rule

A finding that contradicts §2 is a finding against this document, not an
invitation. The next scan applies §0 and starts from this list.

## 4. Work order

One step per change set; every step leaves the workspace green. Safe
backend-internal first, public-API churn in one pre-release group, chain
helpers last.

1. **Indexing backend-internal** (§1.1 items 1–5): tag vocabulary,
   `one_network`, evidence columns, `balance_key`, `Fold::apply` bool. All
   drift-removal; no public API.
2. **Indexing public API group, pre-release** (§1.1 items 6–8): `Subject`
   for `BalanceQuery`, `PagedQuery<C>`, activity/evidence as ports.
   Contract suites re-run against both backends.
3. **Wallets/server** (§1.2 items 1–6), led by the publish pipeline.
4. **`json-rpc` retry loop** (§1.5).
5. **Solana** (§1.4 items 1–3).
6. **Chain helpers** (§1.3 items 1–7) and the wallet contract-test suite
   (item 8). Small, helpers-only; nothing here builds a cross-chain model.
7. **Docs.** `ARCHITECTURE.md` and crate READMEs where a type moved; the
   keep-list copied into indexing/chain READMEs as "do not merge" guidance.

## 5. Out of scope

- **No chain-to-chain dependencies.** Every surviving unification routes
  through `base`, `wallets`, `indexing`, or `json-rpc`.
- **No generic wallet, transaction, or error models.** Rejected in §2;
  stating it twice so no later scan resurrects it.
- **No new user-facing features.** The feature set is frozen by `038`.
- **No merging of keep-list entries.**
- **No test-fixture deduplication across crates** beyond the wallet suite.

## 6. Verification

Per step: the eight gates from `AGENTS.md`, plus the Linter MCP `check`.

Behavioral proof where a unification touches a rule:

- The tag-vocabulary change keeps every stored row readable: a fixture
  written before the change reads back with the same kinds.
- `one_network` / `balance_key` / port promotion: both contract suites pass
  unmodified against sqlite and memory.
- `Subject`-for-`BalanceQuery` and `PagedQuery`: the public examples and
  `custom_store` compile unmodified or are updated in the same change set.
- `Payments::publish`: the crash-order test (registry committed, watch
  refused) still passes; the three call sites now share it.
- Chain helpers: each chain's existing source/interpreter/sender tests pass
  per chain with no cross-chain import in any manifest.
