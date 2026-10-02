# Handoff

Completed 2026-09-29. Checkpoint: `8576201`; final test fixes and this handoff
remain uncommitted. Read `AGENTS.md` and `docs/SYSTEM_REQUIREMENTS.md`.

## Delivered

- Shared repository, minimal watch/commit contracts, repository-owned queries,
  independently feature-gated SQLite/memory, contiguous watch progress and coherent
  pending reads. Fresh observations resolve from the verified tip without inventing
  stored progress; concurrent publication cannot regress the checkpoint.
- Atomic signed watched balances, independent verification, movement activity and
  explicitly recorded opening evidence. Evidence never repairs amounts or proves
  complete history; invalidation is consistent across batch sizes.
- Encrypted application wallet registry, private interrupted registrations,
  restored signing, simplified HTTP, memory-only health/RPC statistics, offline
  recovery and coordinated shutdown.
- Exact/spend-all transfers through existing chain outboxes, network-qualified
  asset units, and working examples with actual indexed local deposits.

## Runtime proof

The current server passed create → fund → confirmed balance/history → graceful
restart → restored IDs/addresses/signers → fixed send → confirmed sweep on Bitcoin,
Ethereum and Solana. Bitcoin/Solana ended at zero. Ethereum's remaining
4,200,000,205,800 wei exactly equals its unused maximum gas reserve.
Log: `/tmp/payment-sdk-nodes/server-live-final.log`.

All six examples passed from fresh directories; `all_chains` also passed immediately
on the same database, restoring all watches. Results and logs:
`/tmp/payment-sdk-nodes/examples-final-results.json`, `*-final.log`,
`all_chains-final-repeat.log`. Bitcoin needs a funded local node wallet and trained
fee estimator, as documented in `examples/README.md`.

The checksum-pinned Agave integration target passed all four tests, including
submission, canonical indexing, SQLite persistence and restart. Its fixture now
reserves Agave's required port range, reports startup failures, funds the actual
rent minimum and awaits successful finalization before inspecting confirmed facts.
Log: `/tmp/payment-sdk-validation-final/solana-node-final-pass.log`.
All owned development nodes and validator test processes were stopped.

## Verification

- Workspace check/test: passed; 1,275 tests including doctests passed. Three optional
  SQLite stress tests remain explicitly ignored.
- All-features Clippy with warnings denied, rustfmt, separate doctests and
  warning-denied rustdoc: passed. Refreshed server suite: 76 passed.
- Submission supervision, eight indexing feature combinations, indexing all-targets,
  minimal configuration and TOML configuration tests: passed.
- SDK dependency isolation, exact OpenAPI regeneration, chain layout and whitespace:
  passed. Fresh MCP: no findings, 39 configured rules completed, 17 unconfigured.
- Consolidated results: `/tmp/payment-sdk-validation-final/completed-results.json`;
  individual logs retain earlier fixture failures and their successful reruns.

## Retired plans

All eleven requested plans (017, 019, 021, 024, 030, 033–038) are removed. Surviving
contracts are in SYSTEM_REQUIREMENTS, ARCHITECTURE and crate READMEs; source
references to obsolete plans are gone.

Explicit supersessions remain: no HTTP-selected confirmation depth; fix-and-restart
for server quarantine, not a CLI pretending to control another process's memory;
no administration transport or orphan-key surface without a real consumer.
Keep justified native stages distinct, including Bitcoin coinbase/signable inputs
and validated Solana wire values. Similar fields alone do not justify merging them.

No further implementation or verification work remains for this goal. No additional
commit was made after the requested checkpoint.
