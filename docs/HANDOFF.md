# Handoff

Updated 2026-09-29. Read `AGENTS.md`. Checkpoint: `6afd612`; subsequent work
is uncommitted. The goal remains open: finish server/indexing, prove all examples
against local nodes and finish verification of the surviving requirements.

## Current work

- **Indexing:** SQLite and memory maintain signed absolute balances for watched
  holdings, updated atomically only for newly inserted effects. SQLite migration 3
  seeds existing rows; unwatched reads still fold. Concrete `verify_balance` checks
  stored totals without repairing them. Focused run: 320 tests passed, 3 ignored;
  subsequent naming/layout cleanup still needs the final rerun.
- **Server:** application-owned encrypted wallet registry shares the SQLite file
  without owning indexing's schema version. Registry-first creation, private
  orphan keys, boot restoration, bounded blocking I/O, and wrong-passphrase refusal
  are implemented. Server unit suite: 73 passed. HTTP cuts, healthz, offline
  backfill cancellation and sweep delegation are implemented; restart/spend/sweep
  acceptance passed, including empty sweep refusal and zero final balance.
  Final HTTP cleanup removed config birthdays from API and deleted PaymentsParams;
  75 bin tests passed, plus restart/sweep acceptance and six evidence-projection
  cases. OpenAPI regenerated; nine resource paths, no retired HTTP birthday schema.
- **Units:** keyed by `(Network, AssetId)` to prevent same-address token metadata
  collisions across Ethereum networks. Four base tests and the two-network chain
  composition test passed. Callers updated.
- **Examples:** Ethereum deposits now use actual local transactions rather than
  genesis allocations. Bitcoin funding must include coinbase maturity **and**
  confirmation depth; confirmation mining includes that depth. Submission draining
  happens while indexing still runs. Transfer and all_chains now both exit 0
  against all three local nodes; all_chains also exits 0 on the same database
  again, restoring 2 Bitcoin, 3 Ethereum and 2 Solana watches.
- **CI:** explicit `wallets --features submissions` test step added.
- **Sweep:** shared `Amount::Exact/All` plus all three chain paths are implemented;
  focused suites passed 633 tests across wallets and three chains. Mixed batches
  permit one source-final sweep per source.
  Ethereum quotes a positive candidate under its total-fee ceiling, then validates the
  resolved value; Solana rebuilds/reprices the same memo/lifetime. Never resolve a
  sweep through an HTTP balance read followed by an ordinary fixed send.
  Live testing found and fixed misuse of Ethereum's maximum-total-fee ceiling as
  a reserve: a positive one-wei probe now obtains an actual checked fee quote, then
  the final amount is validated again. Default-limit regression passes.
- **RPC statistics:** package runtime/doc tests passed, including all 49 transport
  tests. Configuration exposes optional composition statistics without changing wallet
  dependencies. Health snapshot refresh uses a separate observation hook; existing
  notification cadence remains unchanged. Latest integrated tests remain pending.
- **Fresh watches:** live Solana runs proved genesis fallback was wrong on a pruned
  node. New no-checkpoint Fresh resolves after the verified source tip, separately
  from stored CAS state. A race test exposed stale publication reads regressing the
  admission checkpoint; fixed. Fresh/admission/health focused gate: 106 passed,
  3 ignored. Source-tip fallback does not invent a stored checkpoint.
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
- Transfer and all_chains passed after the Bitcoin maturity and Fresh-start fixes.
  Logs: `/tmp/payment-sdk-nodes/transfer-fresh.log`, `all_chains-fixed.log`,
  `all_chains-repeat.log`. Transfer verified confirmed history and balances on
  all three chains; all_chains verified submissions and watch restoration.
  Final all-six rerun after the remaining API changes is still required.
- Integrated server on local nodes created six wallets, received funds, returned
  confirmed balances/history, restored every ID/address/key after restart and signed
  fixed transfers on Bitcoin/Ethereum/Solana. Bitcoin sweep was accepted; Ethereum
  exposed the fee-ceiling defect above, so complete sweep proof is still pending.
  Logs `server-live.log` and `server-sweep-resume.log`: fixed ETH sweep accepted
  and confirmed with a small fee refund; Solana accepted but indexing correctly
  quarantined because the test validator had pruned saved block 5214 while the
  server was down for coding (first available6402). This interrupted run does not
  prove Solana sweep indexing. Fresh full flow is `server-live-final.py` over
  `server-final/`, with immediate restart rather than a long stopped interval.
- Full workspace gates, feature isolation, doc tests and OpenAPI regeneration remain
  required on the integrated final tree. Earlier green reports are not final proof.

Local binaries/logs are under `/tmp/payment-sdk-nodes`. Nodes use only development
funds and loopback ports 18443/8545/8899. Bitcoin fee estimation requires training
transactions; empty maturity blocks alone do not train it. Runtime scenarios use
isolated temporary working directories. Stop only processes started for this work.

## Plan retirement audit

The eleven requested plans are removed. Current requirements live in
SYSTEM_REQUIREMENTS, ARCHITECTURE and crate READMEs; removal is not a claim that
final integration gates or live sweep verification have passed.

All of 017/019/021/024/030/033/034 were read in full. Most recorded changes are
implemented or explicitly superseded; do not recreate old backfill stores, cursor
epochs, raw-block caches, or repository query requirements from their old drafts.
035/036 changes are present; 037 deliberately retains Bitcoin coinbase/input stages
and validated Solana wire twins. Preserve those negative findings when retiring docs.

Requirements still needing implementation or an explicit, evidenced disposition:

- 034: activity query and both backend contracts pass, including self-transfer
  effects, failed fees, transactionless credits, paging and replay. Opening-evidence
  writers are implemented; indexing focused suite passed 83 library + 157 repository
  tests (3 ignored). Ethereum adapter and HTTP projection tests are being verified.
  Evidence remains optional and never changes an amount or proves full history.
  Independent review found batch-size-dependent deficit invalidation; regression
  failed on both stores, then all 11 evidence tests passed after checking each
  completed block. Temporary within-block insertion order does not invalidate it.
- 021/030: optional HTTP confirmation-depth balance refold is explicitly superseded
  by the simple-server surface in SYSTEM_REQUIREMENTS; no arbitrary-depth query.
- 038: finish encrypted restart proof, route cuts, healthz/RPC statistics, single and
  batch sweeps, operator workflows, and exact public documentation.
- Resume is in-memory quarantine control: another CLI process cannot reach it.
  Fix-and-restart is the real operator recovery; do not add fake `--resume` behavior
  or an administration service merely to preserve the old proposal's command name.
- Backfill cancellation can be an explicit offline durable operation; preserve
  registrations, keys and unrelated networks. Confirmed reorg uses offline rescan.

Surviving invariants have moved into `SYSTEM_REQUIREMENTS.md`, architecture and
owning READMEs. Source references to retired plans have been replaced with the
actual invariants; regenerated OpenAPI also removed stale prose.
AGENTS now points at surviving documents and describes persisted totals. Keep
SQLite/memory under indexing storage. Retired-reference search is clean.

## Completion

Run all AGENTS gates plus submissions, feature-isolation and OpenAPI checks. Prove
server create → receive → balance/history → restart → sign/send on all three local
chains, and sweep behavior including refusal cases. Rerun all six examples from
clean state and check their repeat-run story. Read tests for scope before claiming
requirements passed. The retired plans
remain available in Git history; their surviving acceptance criteria still apply. No further commit has
been authorized automatically.
