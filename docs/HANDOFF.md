# Handoff

Updated 2026-09-29. Read `AGENTS.md`. Checkpoint: `7d8d842`; subsequent work
is uncommitted. The goal remains open: finish server/indexing, prove all examples
against local nodes, reconcile requirements, then retire the eleven design plans.

## Current work

- **Indexing:** SQLite and memory maintain signed absolute balances for watched
  holdings, updated atomically only for newly inserted effects. SQLite migration 3
  seeds existing rows; unwatched reads still fold. Concrete `verify_balance` checks
  stored totals without repairing them. Focused run: 320 tests passed, 3 ignored;
  subsequent naming/layout cleanup still needs the final rerun.
- **Server:** application-owned encrypted wallet registry shares the SQLite file
  without owning indexing's schema version. Registry-first creation, private
  orphan keys, boot restoration, bounded blocking I/O, and wrong-passphrase refusal
  are implemented. Four crypto/schema tests passed; restart/spend acceptance is
  still running. HTTP cuts, healthz, operator commands and sweep are next.
- **Units:** keyed by `(Network, AssetId)` to prevent same-address token metadata
  collisions across Ethereum networks. Four base tests and the two-network chain
  composition test passed. Callers updated.
- **Examples:** Ethereum deposits now use actual local transactions rather than
  genesis allocations. Bitcoin funding must include coinbase maturity **and**
  confirmation depth; confirmation mining includes that depth. Submission draining
  happens while indexing still runs. Latest maturity fix awaits rebuilt execution.
- **CI:** explicit `wallets --features submissions` test step added.
- **Sweep:** shared `Amount::Exact/All` plus all three chain paths are implemented;
  focused tests are running. Mixed batches permit one source-final sweep per source.
  Ethereum quotes a positive candidate under its total-fee ceiling, then validates the
  resolved value; Solana rebuilds/reprices the same memo/lifetime. Never resolve a
  sweep through an HTTP balance read followed by an ordinary fixed send.
- **RPC statistics:** package runtime/doc tests passed, including all 49 transport
  tests. Configuration exposes optional composition statistics without changing wallet
  dependencies. Health snapshot refresh uses a separate observation hook; existing
  notification cadence remains unchanged. Latest integrated tests remain pending.
- **Fresh watches:** live Solana runs proved genesis fallback was wrong on a pruned
  node. New no-checkpoint Fresh resolves after the verified source tip, separately
  from stored CAS state. A race test exposed stale publication reads regressing the
  admission checkpoint; the fix and deterministic tests are in progress.
- **Docs:** architecture reduced from 867 to 196 lines; canonical requirements and
  AGENTS reconciled with current ownership and the target simple-server contract.

Current agents own separate areas: server; indexing then RPC statistics/config
wiring; wallets/chain sweep. Root owns examples, runtime validation and docs. Avoid
simultaneous workspace gates while these edits are moving; shared target locks
already caused delays.

## Evidence and unfinished verification

- Fresh MCP after persistence cleanup: zero findings; all configured rules
  completed; unconfigured rules were not counted as passes. Later source edits require a new report.
- Updated `wallet`, `indexing`, `watch_only`, and `custom_store` examples exited 0
  against real local Bitcoin31, Anvil1.8.3 and Agave3.1.14 nodes.
- Transfer examples exposed insufficient Bitcoin fee-estimator history, then the
  coinbase/confirmation-depth bug above. With failure checks, both also reproduced
  the Fresh→genesis Solana failure. Latest example rebuild awaits the Fresh fix;
  neither full transfer scenario is yet proven.
- Checkpoint server served all three assets, readiness and wallet creation against
  those nodes. This baseline does **not** validate the new encrypted registry.
- Full workspace gates, feature isolation, doc tests and OpenAPI regeneration remain
  required on the integrated final tree. Earlier green reports are not final proof.

Local binaries/logs are under `/tmp/payment-sdk-nodes`. Nodes use only development
funds and loopback ports 18443/8545/8899. Bitcoin fee estimation requires training
transactions; empty maturity blocks alone do not train it. Runtime scenarios use
isolated temporary working directories. Stop only processes started for this work.

## Plan retirement audit

All of 017/019/021/024/030/033/034 were read in full. Most recorded changes are
implemented or explicitly superseded; do not recreate old backfill stores, cursor
epochs, raw-block caches, or repository query requirements from their old drafts.
035/036 changes are present; 037 deliberately retains Bitcoin coinbase/input stages
and validated Solana wire twins. Preserve those negative findings when retiring docs.

Requirements still needing implementation or an explicit, evidenced disposition:

- 034: repository activity query includes transactionless balance movements without
  inventing transactions; persisted evidence/writer for verified opening balances.
- 021/030: optional HTTP confirmation-depth balance refold was retained historically
  but is absent; decide it against the final simple-server surface.
- 038: finish encrypted restart proof, route cuts, healthz/RPC statistics, single and
  batch sweeps, operator workflows, and exact public documentation.
- Resume is in-memory quarantine control: another CLI process cannot reach it.
  Fix-and-restart is the real operator recovery; do not add fake `--resume` behavior
  or an administration service merely to preserve the old proposal's command name.
- Backfill cancellation can be an explicit offline durable operation; preserve
  registrations, keys and unrelated networks. Confirmed reorg uses offline rescan.

Move surviving invariants into `SYSTEM_REQUIREMENTS.md`, architecture and owning
READMEs before deleting plans. Replace source references to deleted documents.
AGENTS now points at surviving documents and describes persisted totals. Keep
SQLite/memory under indexing storage; remove historical source references when the
plans are retired.

## Completion

Run all AGENTS gates plus submissions, feature-isolation and OpenAPI checks. Prove
server create → receive → balance/history → restart → sign/send on all three local
chains, and sweep behavior including refusal cases. Rerun all six examples from
clean state and check their repeat-run story. Read tests for scope before claiming
requirements passed. Retire plans only after their surviving requirements have a
verified implementation or an explicit superseding decision. No further commit has
been authorized automatically.
