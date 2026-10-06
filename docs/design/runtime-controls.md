# Runtime controls and logging

Status: **retired by `042`.** The runtime controls and the settings table were
deleted; composition is static. Kept as history.

## Ownership

- Chain contains networks; each network has ordered nodes, each with one RPC endpoint.
- One indexer per network, one repository for all networks. Nodes use automatic failover
  only where replay is safe; multi-call operations pin one verified connection.
- Server owns configuration, UI commands and lifecycle. SDK remains an embedded tool.
- Logging uses an injected standard `log::Log` sink. One bounded memory log supports
  exact chain/network/component/node filters. No global logger or durable log database.

## One SQLite file

The existing configured file holds three independently owned schemas. Opening the
indexing adapter first validates/stamps the file and applies its migrations. Then
open the wallet registry and migrate server settings before launching workers.
An unreachable node must not prevent opening the control UI.

| Tables | Owner / purpose | Migration version |
| --- | --- | --- |
| Existing index tables | `sdk/indexing/src/storage/sqlite`: watches, progress, confirmed facts, queries | Existing `PRAGMA user_version` |
| `wallet`, `wallet_schema` | `apps/server/src/registry.rs`: signing material | Existing `wallet_schema.version` |
| `chains` | Server: one row per chain, its complete configuration | Row `version` |

Ownership: `apps/server/src/settings.rs` owns this table, its reads/writes and
ordered document migrations; `apps/server/src/chains.rs` remains composition.

Only one new table:

```sql
CREATE TABLE chains (
    id TEXT PRIMARY KEY,
    config TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    version INTEGER NOT NULL CHECK (version > 0)
) WITHOUT ROWID;
```

`config` is a typed JSON document containing the chain's networks, ordered
nodes (stable IDs, one endpoint each), assets, requested run/pause state and any
in-progress rebuild. `revision` detects competing edits; `version` identifies the
saved document format. Runtime connection health, counters and logs are not saved.
No node/asset tables or general key-value settings framework.

Indexing keeps its tables and `PRAGMA user_version`. Wallet storage keeps its
existing metadata. Server settings never change either version. Separate bounded
connections use the same SQLite file, existing WAL, FULL durability, foreign keys
and a busy timeout; database work runs off async executors.

## Migration and initialization

The `chains` table has a fixed envelope; document migrations are ordered Rust
conversions owned beside server settings. Reject unknown versions,
validate, then migrate each configuration and update its `version` in one
`BEGIN IMMEDIATE` transaction. A failed conversion rolls back; reopening retries.
No extra migration table is needed for this fixed envelope. A future SQL layout
change needs an explicit design, not speculative machinery today.

Seed configured chains only when creating the table, in that same transaction.
An existing empty table means intentionally no chains: do not seed again.
Node IDs survive URL changes/reordering. Network identities are immutable; disable
referenced assets/networks rather than deleting wallets or their watches. Validate
unique networks, node IDs and asset tags before accepting a configuration.

Store wallet keys and RPC settings directly in SQLite, as explicitly requested.
No database encryption, application encryption or required passphrase. Never expose
keys or RPC credentials in listings, Debug, errors or logs.

## Settings changes and rebuilds

Persist desired settings under a revision. Verify candidates outside SQL, then
activate only if that revision is still current; otherwise discard the candidate.
Keep the last working connection on failure. Runtime status distinguishes desired
and active revision. Preserve sender reservations and signed ambiguous submissions.

Store requested run/pause state in the network configuration. Rebuild uses a request
ID, phase and fixed confirmed target in that document, not a generic job table.
Quiesce passes and admissions before purging. Persist `purging` first; after purge,
persist `scanning` before launching the new pass. A crash during `purging` safely
repeats the offline purge; `scanning` resumes saved watch progress without purging.
Block unsafe financial reads/sends until rebuild completes; other networks run.

Purge goes exclusively through the repository's existing offline capability. Retain
watch definitions/birthdays, keys, settings and unrelated networks. Drop pending
blocks/cache, reload watch progress, and clear only this network's memory logs.
No unresolved submission may be discarded to make a rebuild proceed.

## Verification

- Fresh initialization, repeated open, failed-step rollback and unknown versions.
- Index and registry versions/data unchanged by server settings migrations.
- Configuration seed once, including intentional empty settings.
- Plaintext persistence survives restart; API responses and logs contain no secrets.
- Purge retains keys/settings/other networks; crash recovery does not repeatedly
  purge scanning progress.
- Filtered logs, bounded retention, retry grouping, safe error fields; receipt events
  only after newly applied durable effects, never duplicate retries or Bitcoin change.

## Current implementation and verification

Implemented in source: injected logging and bounded memory retention; per-node filters;
one `chains` table; startup without connected nodes; revision-checked node editing;
verified replacement between complete operations; persisted pause/resume; purge/reindex
phases and progress UI. SQLite and memory repositories retain their existing ownership.

Existing network/asset parameters are immutable in this slice. New networks and their
assets can be added; existing nodes can be added, removed and reordered. Adding assets
to an already active network needs a separate lifecycle design that preserves senders.
The UI accepts chain-owned JSON for new network parameters.
RPC replacement waits for admitted operations and background submissions to settle;
an unresolved submission can delay activation indefinitely. Desired and active revisions
remain distinct during that wait; no force-switch discards recovery state.

Offline settings tests cover seed-once, unknown versions, rollback, competing revisions,
plaintext credentials and preservation of other schema owners. Admission tests prove
maintenance drains only its network. Supervisor tests prove stale progress is cleared
before a recovered network resumes. UI type checking, formatting, lint and build pass.

End-to-end node replacement/reindex/restart coverage is written but remains unverified
in this sandbox: binding the owned mock RPC listener fails with `Operation not permitted`.
Do not treat source completion as proof of restart, ambiguous-submission or reindex safety.

Latest checks: 345 indexing tests and 78 offline server tests pass, along with workspace
compilation, all-features Clippy, rustdoc, doctests, the minimal indexing build, UI build,
chain boundaries and whitespace checks. The design linter reports no findings across
39 configured rules; 17 rules are unconfigured. Full workspace socket tests, the complete
feature matrix and browser verification still need an environment that supports them.
