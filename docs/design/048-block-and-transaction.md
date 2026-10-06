# 048: Block and transaction — two structs, no stages
Status: proposal. One transaction struct, one block wrapper. Validation is a function on them, not a type boundary.
Rule: structures split only when the split carries meaning. Out of scope: dishonest nodes.
Case 1 (sdk/indexing/src/observation.rs): DELETE `Transaction` core {transaction_id, movements, fee}, `ObservationDraft`, `CanonicalTransaction`, `ObservationDraftStatus`, `CanonicalStatus`. ADD
```rust
pub struct Transaction { pub id: TransactionRef, pub movements: Vec<ValueMovement>, pub fee: Option<NetworkFee>, pub order: u64, pub status: Status }
pub enum Status { Included { block: BlockRef }, Failed { block: BlockRef, reason: Option<String> } }
impl Transaction { pub fn validate(&self, network: &Network) -> Result<(), IndexError>; }
```
The interpreter emits Transaction with the block filled; validate also checks status.block == the block being scanned. Flow: scan → interpret → BlockWithOutputs → validate() → commit. sqlite models/transaction.rs `Execution` mirrors `Status` (no third enum).
Case 2 (sdk/indexing/src/block.rs): DELETE `InterpretedBlock`, `CanonicalBlock`. ADD
```rust
pub struct BlockWithOutputs { pub block: Block<Transaction>, pub outputs: OutputChanges, pub credits: Vec<BlockCredit> }
```
CanonicalBlock::new's body moves into `BlockWithOutputs::validate(&self, network) -> Result<()>` (each tx validate + unique ids, unique orders, valid credits, reference unchanged). No aliases, no type params, no `Canonical*` name anywhere.
Work order: 1) Transaction+Status+validate, sqlite Execution follows. 2) BlockWithOutputs absorbing CanonicalBlock::new. 3) gates.
Verification: gates + commit idempotency (block presented twice changes nothing) and network-isolation tests green.
Rejected: type-staged validation; dishonest-node defense.
