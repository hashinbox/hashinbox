# 088: Payments, pending balances and reorgs: the agreed rules

Status: agreed (2026-11-19). The rules below were decided in review; designs 083–087 hold the
detail. Where this note and an older design disagree, this note wins.

## Principles

- Spending twice is worse than not spending: when unsure, the server counts money as spent (a
  lower spendable balance) rather than free.
- Each model touches only its own table; work across tables is orchestrated in the usecase.
- Functions are named for what they do: `delete_*` deletes rows, `set_*` updates them,
  `cap_*` limits a value.

## A payment and its broadcast job

- One payment is one broadcast job; its versions (bumps, a cancel) are transactions of that one
  payment.
- The API (or a trigger round) enqueues the job with the intent only (from, to, asset, amount,
  fee level). Signing needs node calls (nonce, gas, fee), so it stays in the job, not at enqueue.
- The job's first run signs the payment, then writes in one database write: the funds check, the
  pending transaction and its legs, its transaction index, and the job's state. Short of funds:
  nothing is written and the job fails `insufficient-funds`.
- The API refuses early what it can: a sender with no index for the asset (or, for a token, no
  native index, since the fee is paid in it) gets 422 `not-indexed` and no job; a malformed
  amount (`+1`, beyond the chain's range) gets 400. A paused index still sends: the key spends,
  not the index.
- Our transactions are marked by `transaction.broadcasted_at`, set when the pending record is
  written (before the broadcast). Transactions indexed from the chain never have it.
- Fees: a rate `{ base, tip }` from the chain's cached rate provider; `Fee::Set(level)` for a new
  payment, `Fee::Times(1.1)` for a bump. Ethereum asks the node for the exact gas of each new
  payment at signing (a payment that would revert is refused before signing); the cached gas is
  for estimates only (triggers, planning).

## The job must resolve every payment

- The job owns its payment until it is resolved, and only then ends. It never loops for ever and
  never ends early.
- Mined is not done: a version found above the confirmed block may still be reorged away, so the
  job waits; only a version found at or below the confirmed block is final.
- The only ways a job ends:
  - **final:** a version mined at or below the confirmed block, succeeded → done;
  - **reverted:** mined at or below the confirmed block, failed (only its fee paid) → failed;
  - **expired** (Solana): the version can never land and no bump is left → failed;
  - **refused:** every node refused it at the first send → failed;
  - **cancelled** (Ethereum, Bitcoin): our cancel landed final, the payment did not → failed.
- Until then the job keeps checking: missing → resend the same bytes; still missing after
  `bump_after` blocks → bump (a tenth up); bumps used up or the fee cap reached → a cancel in the
  same slot (Ethereum: zero to itself at the same nonce; Bitcoin: the same inputs back to itself),
  so that one of the two must land. Solana needs no cancel: its expiry ends it.
- Solana expiry is the job's decision, not the indexer's: a version is expired when the height of
  the newest kept block at or below the confirmed block is above its `expires_height`, its index
  has read through the confirmed block, and it was not found. The job then drops that version and
  signs a new one while bumps are left, else ends `expired`. With no kept block to read the height
  from, it waits (it never expires early).
- When the job ends without a version on chain (expired, refused, cancelled, given up), it deletes
  the payment's pending transactions, their legs and their transaction indexes in one write, so
  the spendable balance grows back by the amount and the fee. The job's own state keeps the
  outcome, the reason and every txid: that is the only record (no audit table).
- A reverted payment and a landed cancel were mined: their fee legs are chain history and stay
  with their block.

## Pending balances

- A pending transaction's legs have no position. `balance_confirmed_spendable` subtracts our
  pending spends (spent, burned, fee) and our spends mined above the confirmed block;
  `balance_pending` adds every pending leg to the newest balance.
- Triggers and the funds check read these balances, so they are only right while every pending
  spend is still in flight; the broadcast job guarantees that by resolving each payment and
  removing what never landed.
- A spend made outside this server (the same key elsewhere) is invisible until it is mined; that
  cannot be avoided.

## Transaction indexes

- Created before the broadcast, one per version, watched by the indexer.
- The indexer knows nothing about payments or expiry: it reads a transaction index until the
  database says otherwise, and records only what it read.
- It stores facts only: `indexed_at` (the position read through), `found_at` (the position its
  transaction is mined in, NULL until then), and a state for the index itself (indexing, paused,
  no data).
- Found and reverted are worked out, not stored: found when `found_at` is set; reverted from the
  transaction row's own outcome. When the `Confirmer` settles an index behind the line (its block
  not read), the receipt's outcome is written on the transaction row, so the same logic holds.

## Blocks, positions and the line

- A block is addressed by its position: the block number (Ethereum, Bitcoin) or the slot
  (Solana; nodes serve blocks by slot only). Height differs from position only on Solana (skipped
  slots) and matters only for its expiry; it is read from the kept block, not stored on indexes.
- `Tips` is one number: the newest block's position. Confirmed is `tip − confirmations` from the
  network's config, the same rule on every chain (Ethereum 64, about where `finalized` sits;
  Solana 32). `network_index` stores `tip` and `confirmed`, both `NOT NULL`; the row exists once
  the first tip is known.
- Progress pointers are positions, not foreign keys, and carry no hash or height:
  `wallet_index.indexed_at`, `transaction_index.indexed_at`, `transaction_index.found_at`. Facts
  of a read block (`leg.block`, `transaction.block`) keep their foreign key.
- An index moves onto a block when it has read through the block's parent:
  `indexed_at >= parent_position AND indexed_at < position` (this also covers a skipped slot or a
  settle position no block sits at).
- Only blocks that were really read are stored; no empty blocks. One live block per position
  (`UNIQUE (chain, network, position)`); keeping a run is a plain write per block. Reorgs are
  detected by the indexer: every forward read starts at the last kept block, and a run without it
  is a reorg.
- Solana's line is `Line::Depth(read_depth)` (default 1 000 slots; devnet 150): final blocks are
  read again now that Solana's finality equals its tip. `read_depth` must be at least
  `confirmations`, so the confirmed block is never pruned.
- Prune deletes, below the line, blocks that hold no legs and no transactions.

## Adjustments

- A settle (an index behind the line, on a chain that reads balances) reads the chain's balance at
  position S once and writes one adjustment: chain balance at S − the index's running total at S.
- The running total counts mined legs only: our pending payments are not in it, so the adjustment
  never double counts them. A payment already mined at or before S but not read is counted twice
  (by the adjustment and by its pending legs) until its job ends: too low, never too high.
- An adjustment changes only the total, never `total_received` or `total_sent` (those count real
  legs). A Bitcoin jump writes a write-off adjustment (leg index 1) for the coins it archives.
- An adjustment has a position and no block: `{wallet_index_id}.adjustment.{position}.{leg_index}`.
- Ether that a contract sends is not indexed (no transaction field or log shows it); a wallet's
  balance is corrected by reconciling it (`POST …/indexes/{asset}/reconcile`).

## A reorg from position X

A reorg always starts above the confirmed block. In one write, in this order (`to = X − 1`):

1. `transaction::db::delete_unbroadcast_from`: transactions mined at or above X without
   `broadcasted_at` are deleted, their legs with them by cascade. If they are in the new fork, the
   indexer reads them again.
2. `leg::db::delete_kind_from(…, Kind::Adjustment)`: adjustments at or above X are deleted.
3. `leg::db::set_pending_from`: the remaining legs at or above X (only ours are left) go back to
   pending, so our spends still count.
4. `transaction::db::set_pending_from`: the remaining transactions mined at or above X (only
   ours) go back to pending; their broadcast jobs resend, bump or cancel until resolved.
5. `block::db::delete_from`: the blocks at or above X are deleted (rewards with them).
6. `wallet_index::db::cap_indexed_at` and `transaction_index::db::cap_indexed_at`:
   `indexed_at = MIN(indexed_at, X − 1)`.
7. `transaction_index::db::clear_found_from`: `found_at` at or above X is cleared.

- A reorg is a hard delete: no soft delete, no `reorged_at`.
- Nothing below X changes: its blocks, legs and running totals stay right.
- Jobs, triggers and their runs are not touched: the jobs see their payments back to indexing on
  their next check.
- How safe "confirmed" is now depends only on `confirmations`: a reorg deeper than it would
  undo what was taken as final.

## By design

- A trigger acts at most once per `since` window; a payment that failed for good waits for the
  next window. Retrying is the broadcast job's business.
- Ether sent by contracts is not indexed (see Adjustments).
