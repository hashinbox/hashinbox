# Architecture decision records

Each file here records **one decision at the moment it was taken**, with the
context and the alternatives as they stood then. That is the whole value of the
format, so these files are not maintained against the current tree.

**Read every path, module, and type name in an ADR as historical.** Most of
these records predate the `refactor/chain-unification` work and still cite
`sdk/chains/base/src/*`, `apps/api/src/main.rs`, `apps/api/src/config.rs`,
`WalletProvider`, `IndexerSettings`, `Coordinator` and other names the
workspace no longer uses. The decision each one records is still in force
unless a later ADR supersedes it; only the spelling has moved.

For where something lives **today**:

- [`ARCHITECTURE.md`](../../ARCHITECTURE.md) — the crate map and who owns what.
- [`docs/CHAIN_LAYOUT.md`](../CHAIN_LAYOUT.md) — the one layout every chain
  crate follows, file by file, and where each chain is allowed to differ.
- [`docs/SYSTEM_REQUIREMENTS.md`](../SYSTEM_REQUIREMENTS.md) — the invariants.
- Current source and rustdoc — authoritative for exact signatures.

## Editing policy

- Do not rewrite an ADR to match a rename. A record that no longer matches the
  tree is doing its job.
- Edit an ADR only to correct what it says about the decision itself (an ADR
  that misstates its own outcome is wrong, not merely old). ADR-0004 and
  ADR-0026 were edited during the refactor for exactly that reason.
- A decision that changes gets a **new** ADR that supersedes the old one by
  number; the old file stays.
