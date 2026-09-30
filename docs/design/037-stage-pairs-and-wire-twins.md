# 037: Stage pairs and wire twins — the smaller repeats

> "search for similar cases identify similar names and similar structures
> and tell me if cases such as blocks repeats"

Status: **proposal.** A four-way structural audit — `sdk/chains/{bitcoin,
ethereum, solana}` and `sdk/base` + `sdk/wallets` + `apps` — asked one
question: does the `036` case (same shape, different binding) repeat
elsewhere. Answer: yes, once at full scale, as a pattern in smaller forms,
and as one systematic twin layer. This document is the inventory and the
fix plan. `035` and `036` stand; this document extends them.

---

## 0. Method

Four parallel audits, same four categories per crate:

- Maturity pairs: same field set, one binding differs. Generic candidates.
- Stage-pair subsets: same shape plus extra facts. Embedding candidates.
- Impostor names: named X, not X.
- Name look-alikes: confusing near-identical names.

Cross-crate findings are grouped by pattern, not by crate. Each item: the
evidence, the exact difference, the fix verdict.

---

## 1. Exact-scale analogs — generic candidates

The `036` pattern precisely: same fields, one binding, no invariant on the
raw side.

### 1.1 Ethereum `Limits` vs `FeeLimits` — the cleanest repeat

- `config::Limits` (`sdk/chains/ethereum/src/config.rs:388`): six ceilings,
  raw `u128`s, serde-document values.
- `native::transaction::FeeLimits` (`fee.rs:11`): same six fields, fees
  typed `Wei`, invariants enforced in `FeeLimits::new`.
- The config doc admits it: "the same six numbers `FeeLimits` holds, before
  they are checked against each other."
- Even the field names drifted: `gas_margin_basis_points` vs
  `gas_limit_margin_basis_points`.
- **Fix:** one generic `FeeLimits<Amount>`; `Limits = FeeLimits<u128>`,
  `native::FeeLimits = FeeLimits<Wei>`. Conversion stays one-way via
  `Limits::build()`. No invariant lives on the raw side, so nothing is lost.

### 1.2 Solana `LifetimeWire` vs `Lifetime`

- `LifetimeWire { blockhash: String, last_valid_block_height: u64 }`
  (`native/transaction/preflight.rs:41`).
- `Lifetime { blockhash: Hash, last_valid_block_height: u64 }`
  (`lifetime.rs:6`).
- Identical fields; binding of `blockhash` only. The wire twin exists solely
  to decode, then converts at `preflight.rs:62`.
- **Fix:** collapse via serde-with or a `Deserialize` on the domain type.
  Part of a consistent crate-wide wire→domain split (`AccountWire`/
  `Account`, `StatusWire`/`SignatureStatus`, `TokenWire`/`TokenBalance`) —
  apply the same treatment crate-wide or leave the pattern; do not fix one
  twin and keep the other five.

### 1.3 Bitcoin `Input` vs `Spend`

- `Input { outpoint, sequence, previous: Option<Output> }`
  (`native/block/transaction.rs:25`).
- `Spend { utxo: Utxo, sequence }` where `Utxo = { outpoint, output }`
  (`native/transaction/unsigned.rs:12`).
- Isomorphic for every non-coinbase input. Same three facts; nesting and the
  coinbase `Option` differ.
- **Fix:** one shape. Either `Spend` gains the coinbase case or `Input`
  flattens to `Utxo`. Decide at implementation; both preserve the coinbase
  distinction.

---

## 2. Stage-pair subsets — embedding candidates

Same shape plus extra facts. Not generics: the fix is embedding, so the
shared head stops being re-stated.

- **Ethereum `BuildContext` ⊂ `UnsignedTransaction`**
  (`builder.rs:11`, `unsigned.rs:6`). Same six fields, same types, same
  order; `UnsignedTransaction::new` copies them all (`unsigned.rs:47`).
  **Fix:** `UnsignedTransaction { context: BuildContext, to, value, input }`.
- **Ethereum `Sent` ⊂ `Transaction`** (`head.rs:28`,
  `transaction.rs:10`). Parse-stage record vs receipt-enriched transaction;
  `Transaction::new(sent, receipt, trace)` copies the shared five.
  **Fix:** embed `Sent`.
- **Ethereum `FeeInspection`** (`signed.rs:111`). A third restatement of the
  fee field subset, decoded from the envelope. The derived courier field
  `maximum_total_fee = gas_limit * max_fee_per_gas` is computable from the
  other two. **Fix:** drop the courier field; compute it.
- **Bitcoin `Settlement` ⊇ `Admission`** (`reservation.rs:34,42`). Same two
  fields in the same order plus submission facts. **Fix:** embed `Admission`.
- **Bitcoin `Record` ≈ `PreparedTransaction`** (`reservation.rs:51,64`).
  Same two-field shape; transaction made optional plus a status.
  **Fix:** `Record { prepared: Option<PreparedTransaction>, status }`.
- **Solana `SignedBatch` ≈ `Vec<PreparedTransaction>`**
  (`outbox.rs:63`, `reservation.rs:51`). `broadcast_held` unpacks and
  re-wraps — pure shape repacking. **Fix:** batch holds the vector; derive
  the floor or store it once (see 2.1).
- **`min_slot` tripled in Solana** — `SignedTransaction.min_slot`
  (`signed.rs:25`), `PreparedTransaction.min_slot` (`reservation.rs:51`),
  `SignedBatch.min_slot` (`outbox.rs:63`). The outer two shadow the inner
  one with subtly different semantics: the inner is the build slot, the
  outer are post-simulation floors. This is a drift hazard, not just noise.
  **Fix:** one name for the build slot (`min_slot`), one for the simulated
  floor (`floor_slot`), never both spelled the same.

---

## 3. The cross-chain reservation family

All three chains independently define the same stage vocabulary with
near-same shapes:

- `Admission` / `Settlement` — all three (`sender/reservation.rs` in each).
- `PreparedTransaction` — all three.
- `Record` / `Reservations` / `State` / `Status` — bitcoin, ethereum.

Per-chain implementations are mandated by `docs/CHAIN_LAYOUT.md` and the
rule against universal transaction models. **That boundary stands.** What
does not stand: within each crate, the stage shapes re-declare their own
subsets (§2). Fix those per chain. Do not lift a shared send-pipeline
abstraction into `wallets` or `base` — three implementations of a real
chain boundary is not duplication in the `036` sense.

---

## 4. The wire twin layer

`apps/api` has moved into `apps/server/src/api/` (noted: the working tree
changed under this audit). Its DTOs re-state nearly every SDK struct
stringly-typed, each with a hand-written `From`:

- `Network`, `Address` (`contract.rs:206,262`).
- `Block` + `ParentBlock` vs `base::BlockRef` + `BlockParent`
  (`contract.rs:161,176`).
- `Movement`, `Fee`, `Transaction`, `TransactionPage` (`history.rs:51,90,
  229,265`).
- `Balance` (`wallet.rs:221`), `Completeness` (`contract.rs:122`),
  `HistoryCursor` vs `HistoryPosition` (`cursor.rs:15`).
- `NetworkCheckpoint`, `Backfill`, `IndexFailure` vs `NetworkHealth` and its
  fields (`sync.rs:175,143,80`).

HTTP owning its encoding is the rule (`AGENTS.md`: HTTP owns extraction,
authentication, limits, encoding). Mapping-only twins are its legitimate
cost. Two items are still fixable:

- **The history status triple** (`history.rs:163-197`): `PendingStatus` and
  `ConfirmedStatus` are field-for-field identical; `FailedStatus` differs by
  exactly one field. Same shape as indexing's status situation. **Fix:** one
  `Status<B>`-style shape, discriminated. This is the strongest wire-layer
  candidate.
- **The error triple**: `base::transaction::Error` → `wallets::WalletError`
  → `api::Error`/`ErrorBody` all carry `{kind, message,
  ambiguous_transaction_id}`. The `From` impls are pure kind remapping.
  **Fix:** none for the shape (each layer legitimately remaps); note only
  that a third restatement is the ceiling. Watch for a fourth.

Also noted, lower priority: `{asset, start}` request head redefined three
times (`CreateWallet`/`ImportWallet`/`WatchAddress`); `ResumeNetwork` and
`CancelBackfill` are byte-identical single-field bodies;
`PaymentsParams` duplicates `Payments` field-for-field — a self-admitted
courier bag; snapshot DTOs in all three chains rename `base` fields
(`destination`, `scope` — the latter an explicit legacy alias the
pre-release policy says to drop).

---

## 5. Deliberate — do not unify

- **Solana `Admission` / `Settlement`** (`reservation.rs:35,43`): different
  `Drop` impls releasing different reservation sets. Typestate on purpose.
- **Wire→domain twins in Solana** (`AccountWire`/`Account` et al.):
  consistent private serde pattern; boundary, not duplication.
- **Indexing `WatchProgress` / `Progress` / `Backlog`**: lifecycle moments,
  recorded in `035` §4.5.
- **Per-crate `Error{kind, message, retryable}`**: workspace convention.
- **`BlockHeight` / `BlockPosition`** (`base/block.rs:5,32`): identical
  newtype mechanics, deliberate distinct meanings (produced count vs native
  coordinate). The pair already shares `checked_successor` shape via
  convention; do not merge the types.

---

## 6. Recurring impostors — one naming mistake, three chains

The same impostor pattern repeats in all three chains:

- `Batch` — holds `{config, outbox}`. No batch data. It is the per-factory
  `Sender`/`Authorship` adapter. (All three: `sender.rs`.)
- `Item` — content-free name for one send request. (All three:
  `outbox.rs`.)
- `Node` — a config+verified-client composition handle, not a node. (All
  three: `config.rs`.)

Fix by renaming in each crate with the same word, so the census reads
consistently: `Sender` (or `FactorySender`), `Request` (or
`SendRequest`), `Connected` (or `ChainHandle`). Per-crate choice; the
requirement is only that the three stop independently inventing the same
wrong names.

Single-crate impostors worth renaming:

- Solana: `Progress` (a commit-listener hook, `recovery.rs:27`), `Pass` (a
  read budget, `rpc/blocks.rs:137`), `Witness` vs `BalanceWitness` (the
  asker vs the durable handle, `witness.rs:54`, `wallet.rs:251`).
- Ethereum: `Sent` (a partial transaction record, not an event,
  `head.rs:28`), `Grant` (an admission outcome, `reservation.rs:42`),
  `Order` (a log-index checker state, `log.rs:27`), `Builder` vs
  `TransferBuilder` (internal sign-pair vs user-facing builder).
- Bitcoin: `Status` ×2 — node-identity struct (`rpc/node.rs:14`) vs
  reservation-lifecycle enum (`reservation.rs:71`); `Read` (verb-named
  helper, `utxos.rs:37`); `Utxos.utxos` stutter.

Name look-alike families to document, not rename: `Transfer` ×3–4 across
every layer; `Transaction` ×4 in Solana; `Network` ×3 (`native` slug vs
`base` chain+name vs `indexing`); `Balance` ×2 (trace `Balance` vs
`indexing::Balance`); `Supervisor` ×2 in one binary (`submissions` vs
`indexing`).

---

## 7. Base deviation

`AssetId { chain: ChainName, asset }` (`sdk/base/src/asset.rs:5`) is the one
base identity that does not carry network. Every other identity —
`Address`, `transaction::Ref`, `Network`, `Asset` — is network-scoped. The
exception looks deliberate (an asset like `native` spans a family's
networks) but is undocumented. **Fix:** document the exception on the type,
or close it. Do not leave it accidental.

---

## 8. Relationship to `035` and `036`

- `036` owns the one-block-shape decision. This document does not revisit
  it.
- `035` owns indexing's boundary, density, and renames. Items there
  (`SyncStatus`/`NetworkHealth`, the progress trio, `BlockCredit`) are not
  repeated here.
- New here: the chain-crate stage pairs, the cross-chain reservation
  naming, the wire twin layer, and the base `AssetId` exception.

---

## 9. Work order

One step per change set. Every step leaves the workspace green.

1. **Ethereum `FeeLimits<Amount>`.** Collapse `Limits`/`FeeLimits`. The
   audit's cleanest case; do it first.
2. **Solana wire twins.** Collapse `LifetimeWire`/`Lifetime` and decide
   crate-wide for the `*Wire` family: same treatment for all six, or none.
3. **Bitcoin `Input`/`Spend`.** One input shape with the coinbase case.
4. **Embedding pass, per chain.** Ethereum: `UnsignedTransaction` embeds
   `BuildContext`; `Transaction` embeds `Sent`; `FeeInspection` drops the
   courier field. Bitcoin: `Settlement` embeds `Admission`; `Record` holds
   `Option<PreparedTransaction>`. Solana: `SignedBatch` holds the vector;
   `min_slot`/`floor_slot` renamed apart.
5. **Wire status triple.** One status shape in `apps/server/src/api/history.rs`.
6. **Impostor renames.** `Batch`/`Item`/`Node` in all three chains with one
   consistent word each; the single-crate list of §6.
7. **Snapshot DTOs.** Drop the `scope` legacy alias and the rename-mirror
   fields where no contract pins them; keep what the snapshot contract pins.
8. **`AssetId` exception.** Document or close (`§7`).
9. **Docs.** `docs/CHAIN_LAYOUT.md` asymmetry rows where a rename lands;
   crate READMEs' API maps.

## 10. Out of scope

- **No shared send-pipeline abstraction.** Three chains, three
  implementations, one mandated layout. The reservation family's
  cross-crate similarity is the chain boundary's cost, not a defect.
- **No new `base` types.** `036` already widened `Block<T>`; nothing in
  this audit clears the §0 bar from `035` for a new base abstraction.
- **No merging of per-crate `Error` types.** The workspace convention
  stands.
- **No renaming of the wire layer's mapping-only twins.** HTTP owns its
  encoding; only the status triple (a shape defect, not an encoding
  concern) is in scope.

## 11. Verification

Per step: the eight gates from `AGENTS.md`, plus the Linter MCP `check` on
the repository root.

Behavioral proof where a fix touches a rule:

- `FeeLimits::new` still refuses inconsistent ceilings after the collapse;
  config parsing errors unchanged in kind.
- The coinbase case still parses; non-coinbase inputs unchanged in
  sequence and outpoint.
- Solana floor semantics: the simulated floor still advances past the build
  slot in `broadcast_held`; a test pins the two apart.
- The wire status JSON is unchanged for all three statuses (schema contract
  in `docs/openapi.json`).
