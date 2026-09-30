# 036: One block shape — the common `Block<T>`

> "all blocks has one way or the other basis in block ref and transactions
> possibly network ... its important to find same structure/data in this"

Status: **proposal.** One structural decision: a single block shape in
`base`, carried through every stage of the pipeline, with the stage carried
by the child type — not by a new struct per stage. Related documents:
`035` (boundary cut, density, naming) stands; where a rename in `035`
touches a type this document reshapes, this document wins (§6).

---

## 0. The finding

`InterpretedBlock` and `CanonicalBlock` are the same struct.

| Field | `InterpretedBlock` | `CanonicalBlock` |
|---|---|---|
| `block` | `BlockRef` | `BlockRef` — identical |
| `outputs` | `OutputChanges` | `OutputChanges` — carried through untouched |
| `credits` | `Vec<BlockCredit>` | `Vec<BlockCredit>` — carried through untouched |
| transactions | `Vec<ObservationDraft>` | `Vec<CanonicalTransaction>` |
| network | — | `Network`, added at validation |

Four fields. Three identical. The fourth differs only in the child's type.
And the child types differ in exactly one field: the status.

- `ObservationDraft.status`: `ObservationDraftStatus` — `Included` /
  `Failed { reason }`. No block.
- `CanonicalTransaction.status`: `CanonicalStatus` — `Included { block }` /
  `Failed { block, reason }`. Block bound.
- Every other child field is the same: `transaction_id`,
  `transaction_order`, `movements`, `fee`.

So the whole difference between the two block structs is one question: **does
the transaction's status know its block yet?** A draft is a canonical
transaction before the status is bound. Validation is the binding plus the
checks. `CanonicalBlock::new` already passes `outputs` and `credits` through
unchanged — the proof that the shape never varied.

Two hand-rolled structs exist where one shape plus a child-type change would
do. This document is that change.

---

## 1. The design

### 1.1 One shape in `base`

```rust
pub struct Block<T: Identified> {
    network: Network,
    reference: BlockRef,
    transactions: Transactions<T>,
}
```

- `network` joins `reference` and `transactions`.
- This is base idiom, not invention. `Address` carries `Network`.
  `TransactionRef` carries `network`. `RawBlock` in every chain crate pairs
  evidence with the network that gives its addresses text. `Block` is the
  only identity-bearing type that does not carry its network today.
- `Transactions<T>` stays the one child container: block order,
  duplicate-id refusal at construction, id lookup. Chains use it now;
  indexing gets the same container at both of its stages.

### 1.2 The stage map

| Stage | Shape | Home |
|---|---|---|
| Chain-native | `Block<Transaction>` (the chain's own `Transaction`) | `base::Block<T>`, as today plus `network` |
| Interpreted | `Block<ObservationDraft>` + `outputs` + `credits` | indexing wrapper, unvalidated DTO |
| Canonical | `Block<CanonicalTransaction>` + `outputs` + `credits` | indexing wrapper, sealed |
| Read model | `ObservedTransaction` in a `TransactionPage` | stays out of the block family (§3.3) |

The indexing wrapper, once:

```rust
pub struct Facts<T> {              // name decided at implementation
    block: Block<T>,
    outputs: OutputChanges,
    credits: Vec<BlockCredit>,
}
```

- `outputs` and `credits` cannot live in `base`: they are indexing
  vocabulary (`OutputChanges`, `IndexedOutput`, `BlockCredit`). Base knows
  no indexing. They stay as the wrapper's two fields.
- `InterpretedBlock` becomes the bare wrapper over `Facts<ObservationDraft>`
  — or the generic itself, since it needs no protection.
- `CanonicalBlock` becomes a thin sealed type over
  `Facts<CanonicalTransaction>`: private fields, one validating constructor.

### 1.3 What the conversion becomes

`CanonicalBlock::new(network, interpreted)` today re-states the whole
struct. After the change it is:

- Bind the block into every child's status: `ObservationDraftStatus` →
  `CanonicalStatus`.
- Refuse duplicate `transaction_order` — the one check `Transactions<T>`
  does not own (§2.2).
- Validate movements, fees, ids (id refusal now happens earlier, at
  `Transactions::new`, when the interpreter's output is assembled).
- Pass `reference`, `network`, `outputs`, `credits` through untouched.

The pipeline reads: `Block<native>` → `Block<draft>` + extras →
`Block<canonical>` + extras → held / batched. Wrappers after the canonical
stage (`HeldBlock`, `IndexedBatch`, `BlockCommit`) keep their roles and do
not change shape.

---

## 2. Why the boundary stays a type, not a flag

### 2.1 The status pair is the stage

Merging `ObservationDraftStatus` and `CanonicalStatus` into one enum with
`block: Option<BlockRef>` would make "included but no block" a state the
compiler lets anyone build. The two-status design exists so that:

- a draft is nameable without a block binding — it has not been validated;
- a canonical transaction's status always names its block — the type says
  the binding happened.

The child type carries the stage. The block wrapper does not need a second
copy of that information.

### 2.2 The duplicate-order check stays put

`Transactions<T>` refuses duplicate ids. Canonical children additionally
need duplicate `transaction_order` refused, because pages order by
`(height, transaction_order, transaction id)`. That is page-order business,
not child-container business. It stays in the canonical constructor.

### 2.3 The read model stays out

`ObservedTransaction` adds read-time status and confirmations. A
`TransactionPage` spans many blocks and carries no single `BlockRef`.
Forcing either into `Block<T>` would fake a block identity the read does not
have. Pages stay pages.

---

## 3. Soundness catches

Three, all load-bearing. Each names what breaks if ignored.

- **The seal must survive.** `CanonicalBlock`'s private fields and single
  constructor are what make invalid facts unnameable. If canonical becomes a
  bare `Block<CanonicalTransaction>`, anyone can build one unchecked and the
  validation boundary is gone. The thin sealed wrapper (§1.2) is not
  ceremony; it is the invariant.
- **Id refusal moves one stage earlier, not away.** `Transactions::new`
  refuses duplicate ids when the interpreter's output is assembled, where
  today `CanonicalBlock::new` refuses them. Same rule, earlier door. The
  error stays `InvalidBlock`; the message stays specific.
- **`network` is filled, not assumed.** Chains gain a constructor argument.
  Nothing infers a network. A block of the wrong network is refused where it
  always was — `ObservationDraft::canonical` already binds and checks it.

---

## 4. What changes, file by file

- `sdk/base/src/block.rs`. `Block<T>` gains `network: Network`.
  `Block::new(network, reference, transactions)`. No other shape change.
- `sdk/chains/*/native/block/`. Each chain's `type Block = base::Block<Transaction>`
  gains the network argument at parse. Mechanical; each chain already holds
  its network beside the raw block.
- `sdk/indexing/src/block.rs`. `InterpretedBlock` and `CanonicalBlock`
  become the two faces of one wrapper (§1.2). The conversion shrinks to a
  child-type map plus the checks that are genuinely validation.
- `sdk/indexing/src/overlay.rs`. `HeldBlock.facts` keeps its role; it now
  holds the sealed canonical wrapper. No shape change.
- `sdk/indexing/src/storage/commit.rs`. `IndexedBatch.blocks` unchanged in
  role.
- `sdk/indexing/src/listeners.rs`. `BlockCommit` keeps its diff shape.
- Everything that iterates a block's children (`scan.rs`, the stores, the
  readers) iterates `Transactions<T>` instead of `Vec<_>`. Mechanical.

## 5. Density outcome

- One base shape where today `base::Block` and indexing's two structs run
  parallel.
- One indexing wrapper where today two structs duplicate a four-field
  shape.
- One child container (`Transactions<T>`) where today `Vec` is validated by
  hand at canonical time.
- The pipeline stages stop being struct definitions. They become one
  generic instantiated three times.
- Nothing is deleted from the *semantics*: draft, validation, seal, extras.
  What is deleted is the second copy of the shape.

## 6. Relationship to `035`

- `035` §3's rename rows for these types are superseded here:
  `CanonicalBlock` and `InterpretedBlock` stop being parallel structs, so
  they stop being rename candidates. They become the wrapper's two
  instantiations.
- `IndexedBlock` → `NativeBlock` stands. The trait's role — the seam to the
  chain-native block — is unchanged. With `network` inside `Block<T>`, the
  trait's single method `block_ref()` stays sufficient; the engine reads
  the network from the block when it needs it.
- `035` §4 (density) and §5 (work order) stand. This document slots into
  that order as its own step (§7).
- `BlockCredit` naming: still listed in `035` as a census impostor. This
  document does not rename it; the wrapper keeps the type as-is.

---

## 7. Work order

One step per change set. Every step leaves the workspace green.

1. **`base::Block<T>` gains `network`.** Constructor change. Chain crates
   update their parse call sites. Engine untouched — it reads blocks through
   `IndexedBlock::block_ref()` and never constructs one.
2. **`Transactions<T>` refusal adopted at interpretation.** Assemble the
   interpreter's output into `Transactions<ObservationDraft>`; duplicate ids
   now fail where the output is built. The canonical stage's duplicate-id
   check is removed — the container owns that rule.
3. **The indexing wrapper.** `InterpretedBlock` → bare
   `Facts<ObservationDraft>` (or the generic). `CanonicalBlock` → the thin
   sealed type over `Facts<CanonicalTransaction>`. The conversion becomes
   bind + validate + pass-through.
4. **Callers.** `scan.rs`, `overlay.rs`, the stores, the readers: iterate
   `Transactions<T>`. No logic change.
5. **Docs.** `sdk/base/README.md` and `sdk/indexing/README.md` API maps.
   `docs/INDEXING.md` shape description. `ARCHITECTURE.md` if it names the
   old pair.

## 8. Out of scope

- **No change to `BlockRef`.** Position, height, hash, parent, timestamp
  stay. The two-coordinate (position vs height) design is protocol reality.
- **No merging of the read model.** `ObservedTransaction` and
  `TransactionPage` stay out of the family (§2.3).
- **No re-modeling of `outputs` / `credits`.** They keep their types and
  their rule: indexing vocabulary, wrapper fields.
- **No `HeldBlock` / `IndexedBatch` / `BlockCommit` redesign.** Wrappers and
  events, not block shapes.
- **Nothing else moves to `base`.** The §0 bar from `035` stands: one
  package's glue does not get promoted. This change widens an existing base
  type; it adds no new abstraction.

## 9. Verification

Per step: the eight gates from `AGENTS.md` (fmt, check, test, clippy,
doc-tests, docs with `-D warnings`, `check-chains.sh`, `git diff --check`),
plus the Linter MCP `check` on the repository root.

Behavioral proof for steps 2–3 specifically:

- A block with two transactions under one id is refused, at the interpreter
  boundary, as `InvalidBlock` — existing tests move, not weaken.
- A block with two transactions at one native place is still refused by the
  canonical constructor, distinct message.
- A draft whose reference moved between fetch and validation is still
  refused (`validate_reference` survives the move).
- `outputs` and `credits` pass through conversion byte-identical: one test
  asserting equality of the extras before and after `CanonicalBlock::new`.
- Each chain's parsed block carries its network: one assertion per chain's
  existing block-parse tests.
