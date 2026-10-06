# 048: Block and transaction — two structs, no stages

> "I don't see any reason why to duplicate structures for this."

Status: **proposal.** One transaction struct, one block wrapper. Validation is
a function on them, not a type boundary.

---

## 0. The rule

Structures split when the split carries meaning. The interpreter/indexing
split carries none: same fields, same consumers, one `validate()` between
"off the wire" and "into storage." The checks that matter — duplicate ids,
wrong network, negative amounts — are value checks. They don't need types.

Out of scope by decision: dishonest nodes. This system plays by consensus
rules (depth, finalized, rooted) and does not defend against a lying node.
See §4 for what was rejected and why.

## Case 1 — the transaction

**File: `sdk/indexing/src/observation.rs`**

DELETE:

```rust
pub struct Transaction {                 // shared core
    pub transaction_id: TransactionRef,
    pub movements: Vec<ValueMovement>,
    pub fee: Option<NetworkFee>,
}

pub struct ObservationDraft {
    pub transaction: Transaction,
    pub transaction_order: u64,
    pub status: ObservationDraftStatus,
}

pub struct CanonicalTransaction {
    pub transaction: Transaction,
    pub transaction_order: u64,
    pub status: CanonicalStatus,
}

pub enum ObservationDraftStatus { Included, Failed { reason: Option<String> } }
pub enum CanonicalStatus { Included { block }, Failed { block, reason } }
```

ADD:

```rust
/// One chain transaction as indexing holds it: identity, movements, fee,
/// its native place in its block, and inclusion or failure with the block.
pub struct Transaction {
    pub id: TransactionRef,
    pub movements: Vec<ValueMovement>,
    pub fee: Option<NetworkFee>,
    pub order: u64,
    pub status: Status,
}

pub enum Status {
    Included { block: BlockRef },
    Failed { block: BlockRef, reason: Option<String> },
}

impl Transaction {
    /// The ledger's rules, checked before storage: unique id and order,
    /// non-negative amounts, every address, asset, and id of this network.
    pub fn validate(&self, network: &Network) -> Result<(), IndexError>;
}
```

The interpreter emits `Transaction` with the block filled — it knows the
block; it interpreted exactly that one. `validate` additionally checks
`status.block == the block being scanned`, which replaces the old
"draft has no block" machinery with one equality check.

The pass flow becomes: scan → interpret → `BlockWithOutputs` → `validate()`
→ commit. Nothing exists that storage cannot hold.

**File: `sdk/indexing/src/storage/sqlite/models/transaction.rs`**

`Execution` (the third status copy) mirrors `Status` as its row tag — same
discriminant, no third enum.

## Case 2 — the block wrapper

**File: `sdk/indexing/src/block.rs`**

DELETE:

```rust
pub struct InterpretedBlock {
    pub block: Block<ObservationDraft>,
    pub outputs: OutputChanges,
    pub credits: Vec<BlockCredit>,
}

pub struct CanonicalBlock {
    block: Block<CanonicalTransaction>,
    outputs: OutputChanges,
    credits: Vec<BlockCredit>,
}
```

ADD:

```rust
/// One block's indexing contents: the transactions found in it, the
/// live-output changes it made, and the value it moved with no transaction.
pub struct BlockWithOutputs {
    pub block: Block<Transaction>,
    pub outputs: OutputChanges,
    pub credits: Vec<BlockCredit>,
}
```

One instantiation, so no type parameter. Uses of `InterpretedBlock` and
`CanonicalBlock` both become `BlockWithOutputs`; `CanonicalBlock::new`'s body
moves into `BlockWithOutputs::validate(&self, network) -> Result<()>`, which
runs each transaction's `validate` plus the block-level invariants (unique
ids, unique orders, valid credits, reference unchanged).

## Removal math

Five struct/enum definitions (`Transaction` core, `ObservationDraft`,
`CanonicalTransaction`, `ObservationDraftStatus`, `CanonicalStatus`) become
two (`Transaction`, `Status`). Two block structs become one. Deleting with
them: the field-copy in `canonical()`, the core-hop (`draft.transaction.x`),
the doubled doc paragraphs, and the third sqlite status copy. No aliases, no
parameters, no `Canonical*` name anywhere.

## Work order

1. `Transaction` + `Status` + `validate`; sqlite `Execution` follows.
2. `BlockWithOutputs`; its `validate` absorbs `CanonicalBlock::new`.
3. Gates; contract suites (sqlite + memory) green; MCP check.

## Verification

The eight `AGENTS.md` gates per step, plus: commit idempotency (a block
presented twice changes nothing) and network isolation tests still green —
the duplicate-block and wrong-network guarantees the checks exist for.

## 4. Parked — rejected, with the one-line reason

- **Type-staged validation (`Transaction<S>`, draft-vs-canonical):**
  rejected. The only thing it enforced beyond `validate()` was
  "unvalidated can't be stored" — one call site, now a convention.
- **Dishonest-node defense:** out of scope. Consensus rules (depth,
  finalized, rooted) are the trust assumption; a lying node is not this
  project's adversary.
- **Everything else from the audits (pages, `Page`, `Plan`, `Nodes`,
  `Error<K>`, chain `Error`, `Signed`, server cases):** unreviewed; each
  stands or falls on its own when these two land.
