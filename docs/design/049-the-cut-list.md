# 049: The cut list — what goes, what adjusts

Status: **proposal.** Consolidates the decisions made in review. Each removal
names what deletes and what adjusts around the hole. One decision is still
open (§5).

## 1. Locked decisions

- **The API takes a birthday again.** `POST /v1/wallets` and the watch
  endpoint regain a `start` field: `"fresh"` or a native position. The 038
  cut is reversed.
- **No restore.** Archiving stays one-way; nothing reactivates an archived
  wallet.
- **Memory backend stays** as a shipped backend.
- **Cancel-backfill stays** — with API birthdays, an old birthday on a
  pruned node is a product-level corner again, and unwatch/re-watch is not
  an acceptable operator answer for an API user's funds.

## 2. Removals

1. **`verify_balance`** — sqlite + memory impls, its tests, the
   SYSTEM_REQUIREMENTS line ("explicit verification compares persisted
   totals"), README mentions. Nobody calls it; `Fold` is public for anyone
   who wants the audit.
2. **`BlockCommit` payload** — keep the hook and `network` + `block`; delete
   `transactions`, `outputs`, `unmatched_spends` fields and their assembly
   in `announce` (`pass.rs`). Solana recovery, the only consumer, reads
   `block` alone.
3. **`NetworkEvent` / `SyncMonitor::network_changed`** — the event cadence
   with no shipped reader. `SyncReport` keeps updating its snapshot on its
   own poll path; the monitor port shrinks to what health actually uses.
4. **`Assembly` / `Index::over` / `Index::sync`** — embedder sugar. `Index::of`
   remains; `examples/custom_store` composes with it. The fluent builder and
   the all-units pass delete.
5. **Receipts operator log** (`storage/receipts.rs`, `wallet.received`) and
   `examples/bench.rs` — observability and a dev harness with no consumer.
6. **The restore path** — `api/wallets.rs`'s archived-wallet restore
   (the `filed.birthday()` re-registration). With no restore, DELETE
   becomes plain deletion: withdraw the watch, forget the signer, keep the
   durable row readable as watch-only history.

## 3. Adjustments

- **Request bodies.** `POST /v1/wallets` and the watch endpoint gain
  `start: "fresh" | number`. Refused synchronously when below what policy
  allows (`max_backfill_blocks`), same rule SDK `Birthday::At` already
  takes. Wire change: `docs/openapi.json` regenerates; the route-contract
  tests grow a birthday case (fresh, valid position, too-old refusal).
- **`WalletRegistry`/archive semantics.** Archive keeps the row and drops
  the key; nothing reads an archived row back into a live wallet. The
  restore-shaped code (original-birthday reuse) is gone, so the
  unserveable-birthday corner can only arrive from an explicit request —
  which is what cancel-backfill now settles.
- **System requirements edits:** drop the restore requirement, the
  cancel-backfill wording stays, the "no caller-supplied birthdays" line
  is replaced by "creation accepts `"fresh"` or a native position; the
  index refuses starts its policy cannot honor", and the
  `verify_balance`/explicit-verification line goes.

## 4. What survives, one line each

Backfill machinery (API birthdays make it load-bearing) · the admission
handshake, progress CAS, reconciliation guards (lost funds / wedged
network) · quarantine + health vocabulary (mission) · purge detection —
the `ConfirmedReorg` check stays either way; only its repair is §5 ·
spent-output retention, cursor revisions (duplicate-block rule, coin
selection) · sparse positions (protocol) · `Holder`/`Subject` move to
base and wallets drops indexing (task doc, separate track).

## 5. Open decision — the repair for a deep reorg

Purge (`--rescan`) + the re-added online rescan endpoints are one question.
Keep purge: a deep Bitcoin reorg repairs one network, everything else
serves. Remove it: that day you stop, delete the file, reindex all
networks; the online rescan endpoint and `SyncControl` go with it, and
042 stands complete. Both are honest; it's a disaster-plan choice, not a
code quality call. Until decided, purge and its contract suite stay.

## 6. Work order

1. Removals 1–5 (independent; gates after each).
2. Removal 6 + §3 adjustments (birthday back, restore out).
3. System-requirements edits in the same change set.
4. Gates: fmt, check, test, clippy, doctests, doc, `check-chains.sh`,
   `git diff --check`, MCP linter.
