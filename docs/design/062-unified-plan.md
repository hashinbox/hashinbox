# 062 — Unified plan: commit, queued sends, central storage

Status: accepted

Supersedes 058 and 061: their unbuilt parts, revised (sends in the queue,
durable job state, two outcomes, storage in the server).

## Context: implemented, not yet committed

- 057: running totals on `ledger` rows; `wallet_index.balance` gone.
- `posting` renamed to `ledger` (`storage/ledger.rs`).
- Solana recovery cut (`sender/presence.rs`, `sender/recovery.rs` deleted).
- `sqlite/schema.rs` removed; `migrate` runs `sdk.sql`, `server.sql`, `queue.sql`.
- `packages/tasks` base (058 steps 1–3): model, `Store`, drivers, runner, suite.
- Guards inlined (`location.rs`, `subject.rs`, server `models/signers.rs` gone).

## Ownership

| Owner | Owns | Must never touch |
| --- | --- | --- |
| `packages/tasks` | Generic queue: `NewTask`, `Store` trait, lanes, leases, retries, per-job JSON state, `Runner`/`Executor`/`Handle` | SDK or app crates, chains, wallets, SQL implementations |
| `sdk/base` | Chain-neutral values (`Address`, `Amount`, `SignedBytes`, ids), signer contract | Queue, indexing, storage |
| `sdk/chains/*` | `TransactionBuilder` (fetches its own chain inputs, builds, invokes the injected signer) and `Broadcaster` (exact bytes to every node); indexer adapters | Queue, wallet rows, storage implementations, other chains |
| `sdk/wallets` | `wallet` rows contract (`WalletRepository`), keys, `KeyFormat`, signers, factories | Queue, sending, indexing, SQL |
| `sdk/indexing` | `Change`/`Changes`, commit rules, watches, driver, store traits (`WatchStore`, `IndexStore`, `OutputStore`), pure helpers (`LedgerEntry`, output helpers) | SQL, memory stores, queue, `wallet` table |
| `sdk/payments` | Config parsing, chain registration, composition of factories/indexers (decided 4) | Storage implementations, queue workers |
| `apps/server/src/models` | One file per table: the model, its collection and its SQL | HTTP, SDK traits |
| `apps/server/src/storage` | SDK trait implementations built on the models; pool and `migrate` | HTTP, chain-specific logic |
| `apps/server/src/workers` | One file per queue: its job input/state, executor and lane rule (`send.rs` today), composed by `main.rs` | HTTP, direct SQL |
| `apps/server/src/api` | HTTP resources: enqueue sends, report status | SQL, chain names |

## Commit model (from 061, unchanged)

```rust
pub enum Change { Block(Block), Transaction(Transaction), Reward(Reward), Output(Output) }
pub enum TransactionState {
    Expected { sender: Address, signed: SignedBytes },   // saved before broadcast
    Mined { block: BlockRef, order: u32, status: ExecutionStatus },
    Failed { reason: String },                           // never mined
}
```

`Transaction` carries network, id, state, transfers and an optional fee.
`Reward` is a transactionless credit, such as a mining reward or an Ethereum
withdrawal. `Output` is `Created` or `Spent`. `Change` is a closed enum: stores
`match` on it, so a new variant fails to compile in every store until handled.

`Changes::new` validates the batch:

- It is non-empty and covers one network.
- Blocks are strictly ascending and hash-chained.
- Each block's `Block` comes first, followed by its facts in chain order.
- Every block-bound fact references a block in the batch.
- There are no duplicate block, transaction or outpoint.
- A batch may hold only Expected/Failed rows.

The commit runs in one store transaction:

1. **Chain onto the checkpoint.** Blocks at or below the checkpoint are
   hash-verified, or inserted if absent. The first block above must have
   `parent == checkpoint`, else `Disconnected` (retryable).
2. **Insert or verify every fact.** An existing row must be identical, else
   the commit fails with `Store`. Allowed transitions are expected→mined,
   expected→failed and failed→mined. Facts are written only for watched pairs.
3. **Derive watch advance, last.** Every watch whose progress equals
   `first.parent` advances to the last block. So does every watch with no
   progress whose birthday falls in the range. Progress never moves backwards.
4. **Identical retry** is a no-op by rules 1–3.

Bitcoin's fee is data on the transaction, not a ledger row: its movements
already contain it. Ethereum and Solana fees stay as payer debits.

Indexing is the only writer of `"transaction"`. The send worker writes
Expected and Failed rows through `IndexStore::commit`, implemented in
`apps/server/src/storage`.

Deleted with this: `IndexedBatch`, `Progress`, `expected_checkpoint`, claim
building (`line`/`claims`/`justified`/`supports`/`unjustified`,
`forward`/`proved`), store CAS, "unknown watch → Conflict", the reload on
Conflict in `run.rs`, `BlockWithOutputs` as a commit payload, `BlockCredit`,
`FeeSettlement` and `BlockInterpreter::settlement`.

## Send job

The caller enqueues the intent and nothing else:

```rust
struct SendIntent { wallet: WalletId, transfers: Vec<Transfer> } // recipient, asset, amount; ordered
NewTask { lane: sender_address, kind: "send", input: json(intent), .. }
```

Each sender has one lane, and a lane runs one job at a time. That serial lane
is the ordering. There is no wallet lock, no `PendingFacts`, no `pending()`
read, no reservations and no reconciler.

The reconciler is unnecessary because the job exists before any transaction
row. A crash at any point leaves a durable job, and its state says how far it
got. No transaction row can exist without a job to finish it.

The worker (`apps/server/src/workers/send.rs`) runs this flow:

1. **Resume.** If `state` holds signed bytes and txid, go to step 3 with
   them. Never rebuild.
2. **Build and sign.** The sender need not be watched at the SDK level; the
   server API forces/requires it before enqueue (decided 3). Load the wallet and its signer (`sdk/wallets`). Call
   the chain's `TransactionBuilder`, which fetches its own inputs:
   - **Ethereum:** the nonce is the node's `eth_getTransactionCount(pending)`.
     This is safe because the lane is serial and the previous job ends only
     after node acceptance (or failure), so its nonce is already in the node's
     pending count.
   - **Bitcoin:** coins are `output` rows with `to = sender` and
     `spent_by IS NULL` (decided 7); the builder derives the locking script
     from the sender's address.
   - **Solana:** a fresh blockhash plus its `last_valid_block_height`.
3. **State first, then record.** Save signed bytes + txid (and the Solana
   expiry height) into job `state` via `save_state`. Then commit
   `Transaction{Expected}` (`raw`, `sender`, `block_id` NULL) through
   indexing; an identical retry is a no-op. A retry always finds the bytes in
   state, so no Expected row exists without its job: no orphans. On Bitcoin
   the same commit sets `spent_by = txid` (`spent_block` NULL) on the chosen
   coins, so the lane's next job skips them before mining.
4. **Broadcast.** The chain's `Broadcaster` sends the exact bytes to every
   configured node.
   - **Accepted:** the job succeeds.
   - **Refused:** commit `Transaction{Failed}` (clears its coins'
     `spent_by`), and the job fails.
   - **Transient:** retry later, with the same bytes from state.
   - **Solana:** rebroadcast until the blockhash expires. On expiry the job
     fails with "outcome unknown". The row stays Expected, because the
     transaction may still land and the indexer remains the authority.
5. **Indexer.** Seeing it in a block, commits `Mined`, upgrading the row
   and filling `spent_block` on its coins.
   The job's only objective is sending (build, save, broadcast); it never
   tracks mining. Dropped transactions are not handled: unmined Expected rows
   just stay (decided 2).

An ordered batch is one job. It signs every transaction up front, saves them
in one `Changes`, and broadcasts them in order. Each accepted txid is recorded
in the job state, which preserves Ethereum's accepted prefix.

## Output store (universal)

`output` is the movement record for every chain: one row per
`(txid, index)` with `from`, `to`, `asset`, `amount`, `block`, and on UTXO
chains `spent_by` (txid) and `spent_block`. Querying an address as `from` or
`to` returns everything it sent or received on any chain.

- **Ethereum/Solana:** each transfer (native or token) is one row
  from → to. `spent_by` is unused.
- **Bitcoin:** rows pair inputs to outputs. One sender or one output pairs
  exactly. Many-to-many uses the in-order rule explorers use: inputs fill
  outputs in order, splitting across boundaries. The fee is attributed to
  the last input (sender side): its leftover after all outputs is the fee,
  kept as transaction data, not a row. Input addresses and amounts come from
  the existing prevout resolution.
- **Coins:** unspent coins for a send are rows with `to = sender` and
  `spent_by IS NULL` (UTXO chains only).
- **Writes:** indexing commit only, confirmed blocks, watched addresses,
  from birthday on. The send job's Expected/Failed commits only set/clear
  `spent_by`; Mined fills `spent_block`.
- **`evidence` dropped:** column, `IndexedOutput` field and Bitcoin
  interpreter write. The builder derives the script from the address.

Overlap with `ledger`: `ledger` keeps running totals per (address, asset)
and stays the balance table; `output` is the movement record. Recommended:
ledger rows are derived from output rows in the same commit. Open (8).

AGENTS.md lines that conflict, with proposed rewording:

- "Preserve every transaction movement, including Bitcoin inputs/outputs;
  never flatten chain facts into fictional transfers" → "Preserve every
  movement; Bitcoin input→output pairing follows the stated in-order
  convention."
- "Do not invent synthetic transactions or account UTXOs" → "Do not invent
  synthetic transactions; account-chain `output` rows are transfers, not
  UTXOs."

## Queue changes (`packages/tasks`)

- **One table, no `q_result`.** A job stays in `q` for life: `status` (pending | running | succeeded | failed), `state` (JSON) and `reason`. Finished rows are the record; no archive table for now.
- **Per-job state.** `q.state TEXT NOT NULL DEFAULT '{}'` (JSON). `Delivery` carries it, and the executor returns an
  updated state with every ack. `Store::save_state(id, owner, json)` lets the
  executor persist mid-run (the txid before broadcasting), checked against the
  lease owner.
- **Two terminal outcomes.** `Outcome { Succeeded, Failed }`. Failed carries a
  reason: `refused`, `expired`, `attempts-exhausted`, `cancelled`,
  `unknown-kind` or `outcome-unknown` (Solana). `Ack` is `Done`,
  `Fail(reason)` or `Retry(reason)`; the other four outcomes are deleted.
- **Input is JSON.** `payload` becomes `input TEXT`.
- **No idempotency key.** Its reconciler is gone; the caller gets the job id.
- Lease/owner CAS, lane heads, deadlines and backoff stay.

## Storage centralization

All storage traits stay exactly where they are today: indexing's
`WatchStore`/`IndexStore`/`OutputStore`, tasks' `Store` and wallets'
`WalletRepository`. Only the SQLite implementations move, into
`apps/server/src/{models,storage}`, so all of the app's storage lives in one
place. The memory implementations are deleted.

Target layout:

```
apps/server/src/
  models/      block.rs transaction.rs transaction_address.rs ledger.rs output.rs
               checkpoint.rs wallet_index.rs wallet.rs task.rs task_result.rs
               rpc_endpoints.rs asset.rs network.rs ...   (model + collection + SQL)
  storage/     mod.rs (pool, migrate, Config)
               index.rs    WatchStore + IndexStore + OutputStore
               wallets.rs  wallets::WalletRepository
               tasks.rs    tasks::Store
               reads.rs    balance/history reads for api
  workers/     mod.rs send.rs
```

Moves: `sdk/indexing/src/storage/sqlite/*` → `models/*` + `storage/index.rs`;
`sdk/wallets/src/storage/sqlite.rs` → `models/wallet.rs` + `storage/wallets.rs`;
`packages/tasks/src/storage/sqlite.rs` → `models/task*.rs` + `storage/tasks.rs`.

Deletes: `sdk/indexing/src/storage/memory/`, `packages/tasks/src/storage/memory.rs`,
the `sqlite`/`memory` features and `sqlx` deps in SDK crates and
`packages/tasks`, and `wallets::testing`/`indexing::testing` if unused.

The SDK keeps the traits, `Changes` validation, `LedgerEntry` and the output
helpers, so any third-party store computes rows identically.

Contract suites move to `apps/server/tests/`, SQLite only.
`schema/*.sql` stays at the root, and `migrate` executes it.

`examples/custom_store` and the other examples that use the shipped stores
need a store. Deferred (decided 6).

AGENTS.md lines to change:

- **L39** (`models/`): one file per table, SDK tables included, all SQL.
- **L43** (`sdk/wallets`): drop "batch orchestration" and "SQLite behind
  feature `sqlite`"; contract, keys and signers only, never the queue.
- **L45** (`sdk/indexing/src/storage/`): contracts only, no implementations.
- **L91–92**: replace "SQLite and memory stay inside `storage/` … memory is a
  legitimate backend" with "implementations live in `apps/server/src/storage`".
- **L93–95**: drop `wallets::testing`. **L151**: contracts run on SQLite in
  `apps/server/tests`. **Table**: add an `apps/server/src/workers/` row.

## Schema changes

`schema/sdk.sql`, `"transaction"`:

| Column | Change |
| --- | --- |
| `block_id`, `transaction_order`, `execution_status` | Nullable; NULL unless mined |
| `state` | INTEGER: 0 expected, 1 mined, 2 failed |
| `sender` | TEXT `<chain:network:address>`; NULL for indexed-only rows |
| `raw` | BLOB signed bytes; NULL for indexed-only rows |

Add `CHECK ((state = 1) = (block_id IS NOT NULL))` and index
`transaction_expected (sender) WHERE state = 0`. No `nonce`, `pending_input`
or unique pending indexes. `history_entry` stays an inner join (mined only).

`schema/sdk.sql`, `output`: columns `from_address`, `to_address`, `asset`,
`amount`, `block_id`, `spent_by` (txid, nullable), `spent_block_id`
(nullable); drop `evidence`, `address_id` and the tombstone CHECK. Index
`output_unspent (to_address) WHERE spent_by IS NULL` and `output_from
(from_address)`.

`schema/queue.sql`:

- `payload BLOB` becomes `input TEXT`.
- Drop `q_result`.
- `q.state` (old pending|running integer) becomes `status INTEGER` with `IN (0, 1, 2, 3)` (pending, running, succeeded, failed).
- Add `state TEXT NOT NULL DEFAULT '{}'` (JSON job state) and `reason TEXT` (set when failed).

## Deletions

- **Chains:** `sender/*` and `sender.rs` become `builder.rs` + `Broadcaster`;
  reservation marks, Solana `SourceBusy` and the rebroadcast loop go.
- **Wallets:** `queue.rs`, `submissions.rs`, `sender.rs`, `pause.rs` (if
  send-only), `wallet.send`, batch orchestration, `storage/sqlite.rs`.
- **Indexing:** old commit machinery, `storage/{memory,sqlite}/`, inherent reads.
- **Tasks:** `storage/{memory,sqlite}.rs`, the four retired outcomes.
- **Output:** `evidence` (column, `IndexedOutput` field, Bitcoin interpreter
  write) and the spend-first tombstone.

## Failure cases

| Case | Result |
| --- | --- |
| Crash before enqueue | No job; client got no 202 and retries |
| Crash after enqueue, before state saved | Job reclaimed; builds fresh (nothing saved or broadcast) |
| Crash after state saved, before Expected commit | Retry reuses state bytes; commits Expected; no orphan |
| Crash after Expected commit, before/during broadcast | Lease expires; identical commit no-op; rebroadcast same bytes |
| Node refuses | Commit Failed; job failed (`refused`) |
| Transient RPC error | Retry, same bytes, bounded backoff; exhausted → failed (`attempts-exhausted`), row stays Expected |
| Solana blockhash expired, unseen | Job failed (`outcome-unknown`); row Expected; may still be mined |
| Deadline passed while pending | Job failed (`expired`) |
| Two sends from one wallet | Same lane; second runs after first ends |
| Two server processes | Lease CAS; lanes still one head at a time |
| Failed row later mined | Upgraded to Mined; chain wins |
| Accepted tx dropped from mempool | Not handled; row stays Expected (decided 2) |
| Confirmed reorg / purge | Purge deletes all network data except wallet rows and watch registrations/birthdays (decided 1) |

## Rollout order

1. Schema: `transaction` columns; `output` universal columns, no `evidence`; queue `input`, `status`, `state`, `reason`; drop `q_result`.
2. Storage centralization: move the SQLite code into the server's
   models/storage, delete the memory stores and SDK features, and move the
   contract suites. No behavior change.
3. Indexing: `Change`/`Changes`, the rewritten commit and the derived advance.
   Then delete the old machinery and `FeeSettlement`.
   Interpreters emit universal `output` rows (Bitcoin in-order pairing).
4. Tasks: JSON state, `save_state` and the two outcomes.
5. Chains: `TransactionBuilder` plus `Broadcaster`, and delete the sender
   modules.
6. Wallets: delete send, queue and batch orchestration.
7. Server: `workers/send.rs`, `POST /wallets/{id}/transactions` → `202 { job }`,
   and `GET /jobs/{id}` (job outcome, state txids, row state).
8. Tests: crash windows, refusal, Solana expiry, batch prefix and reorg.
   Update AGENTS.md.

## Decided

1. **Purge.** Deletes everything about the network: blocks, transactions
   (Expected rows included), ledger, outputs, memberships, checkpoint and
   watch progress. Keeps wallet rows and watch registrations/birthdays. No
   orphan sweep.
2. **Dropped transactions.** Not handled. Expected-but-unmined rows stay;
   dealt with later. The send job only sends (build, save, broadcast); it
   does not track mining.
3. **Sender watched.** Not required at the SDK level; `sdk/wallets` does not
   require it. `apps/server` guarantees it: the server API requires/forces
   the sender address to be watched before sending.
4. **`sdk/payments`.** Decide after the rollout, based on what is left.
5. **Chain factories.** Stay as they are: chain crates keep `WalletFactory`,
   `sdk/wallets` keeps trait + collection, composition stays in `payments`.
6. **Examples and third parties without a store.** Deferred.
7. **Bitcoin coins.** `output` is universal; coins are `to = sender AND
   spent_by IS NULL`; Expected sets `spent_by`, Failed clears it, Mined
   fills `spent_block`. See Output store (universal).
8. **Ledger vs output.** Ledger rows are derived from output rows in the same
   commit; ledger stays the running-total table.
