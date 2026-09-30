# System requirements

Product scope and acceptance criteria. [ARCHITECTURE.md](../ARCHITECTURE.md)
defines ownership; crate READMEs and source define exact APIs. OpenAPI is generated
from the server. Historical plans do not override these requirements.

## Product

One `payment-server` process composes Bitcoin, Ethereum and Solana, an embedded
index, one SQLite file and an encrypted application wallet registry. It must:

- Create wallets, persist their keys, and return canonical public addresses.
- Observe public addresses without keys; index from explicit resolved birthdays.
- Return exact confirmed/pending balances and paginated transaction history, with
  honest completeness. Balance uses chain-defined finality, not an HTTP-selected
  confirmation depth; this supersedes the older optional depth-refold proposal.
- Submit fixed transfers or explicit spend-all transfers, singly or in ordered batches.
- Restore signing capability, watches, progress and indexed facts after restart.
- Keep other networks running when one indexer fails or quarantines.

The final HTTP surface is `/healthz`, `/v1/assets`, `/v1/wallets`,
`/v1/wallets/{id}`, `/v1/wallets/{id}/balance`,
`/v1/wallets/{id}/transactions`, `/v1/transactions`, and `/v1/addresses`,
plus the generated OpenAPI document. Remove HTTP import/address-parse conveniences,
status/resume/backfill-cancel operations and caller-supplied creation birthdays.
SDK/configuration imports and explicit birthdays remain available to embedders.

No business ledger, payment state machine, automatic collection/sweep job, gas
sponsorship, durable outgoing-operation/idempotency store, hardware wallet,
HSM/KMS/remote custody, raw-block archive, separate indexer service, HA or
multi-writer deployment. Chain-owned reservations are necessary send coordination,
not a business reservation service. Encryption does not claim production custody.

Pre-release internal APIs may change directly; no compatibility aliases or invented
versioned DTOs. SQLite migrations are ordered, transactional and versioned; reject
foreign or unknown-newer files. No legacy redb reader is required. Preserve
application tables, registrations and unrelated networks during network recovery.
External chain, JSON-RPC and HTTP standards remain compatibility contracts.

## Ownership

- Packages contain reusable mechanics and never import SDK/apps. Base values contain
  no universal RPC/transaction model and import no concrete chain, indexing or wallets.
- Chains own native RPC, protocol models, signatures, fees, finality, send pipelines
  and indexing translation. Native modules name no SDK wallet/indexing models.
- Indexing owns watches, bounded scanning, continuity, confirmation and pending facts.
  Required persistence is only `WatchStore + IndexStore`; custom sinks need no queries.
- Repository models, calculations and queries belong under indexing `storage/`.
  SQLite and memory stay there as independently feature-gated shipped backends;
  shared contract suites belong in `tests/`.
- Wallets collect factories and orchestrate batches. They own no custody, stored watch
  list or ID-to-wallet map. Sending is an optional factory capability (`Authorship`).
- Configuration parses documents and constructs transports through registered chains;
  it imports no concrete chain. Wallet/index contracts remain transport-independent.
- Server selects concrete chains only in `src/chains.rs`; HTTP modules import no
  concrete chain or indexing backend. HTTP owns DTOs, validation, limits and encoding.
  Secrets never appear in responses, schemas, logs, errors or ordinary `Debug`.

## Observation and indexing

- One repository serves every network; no database, schema, pool or repository per
  asset. Every operation isolates its exact `(chain, network)`.
- A watch is ID, canonical address, asset and inclusive birthday, with no key material.
  Same-address native/token registrations remain distinct observations. IDs are unique;
  exact repeats preserve progress, conflicting definitions fail.
- Persist observations before publishing them to scanning. Coordinate watch/unwatch
  and sync, including uncertain writes. A cancelled await does not prove rollback.
- Track contiguous hash-verified per-watch progress, including empty blocks. Resume
  after saved progress or at the birthday. New watches never reset existing ones.
  Share fetches across overlapping ranges without skipping any watch's history.
- `Birthday::Fresh` is an assertion that the address has no past. Resolve it to the
  checked successor of the admitted checkpoint, or the verified source tip when no
  checkpoint exists. Never silently turn it into genesis on a pruned source.
  `Birthday::At` preserves the supplied native position and checks configured limits.
- Native position controls traversal/birthdays/restart; produced height controls
  confirmation. Positions may be sparse. Preserve block hash and paired parent
  position/hash. Inspect the first produced block at or after a sparse birthday.
- Historical watches catch up before joining forward selection. Forward work receives
  the bounded pass budget first; missing history remains explicit and does not stop
  healthy forward indexing. A historic addition does not require a network purge.
- Unwatch stops observation and drops its progress, retaining indexed facts. Re-watch
  re-inspects from its own birthday. Cancellation raises an owing watch's birthday
  atomically to after the checkpoint, with no invented progress and no fact deletion.
- Confirmed boundary is Bitcoin depth, Ethereum finalized, Solana rooted. Commit only
  confirmed facts. Pending blocks remain in memory and are rebuilt after restart.
- Verify canonical hashes before resuming. Pending forks truncate memory without store
  rollback. Confirmed forks require explicit offline recovery of that network only.
- One commit atomically records facts, outputs, checkpoint and affected watch progress;
  expected-state conflicts fail, identical retries are harmless. Reinspection for a
  new watch preserves existing transactions and other watches' progress.
- Preserve all transaction movements, native transaction ordering, failed fees and
  transactionless credits. Never invent a transaction ID for a block credit or a
  synthetic UTXO for an account. Historical replay must not resurrect spent outputs.
- Callbacks after commit are notification-only and cannot veto or guarantee external
  delivery. Custom repositories can persist application reactions transactionally;
  distinguish duplicate retries from new watch associations.

## Repository reads

- Persist signed absolute totals for watched holdings, atomically with new movements.
  Deficits are evidence of missing history; do not discard them by storing zero.
  Public unsigned balances report zero with `Overdrawn` where necessary.
- Money uses checked atomic integers and exact decimals, never floats or SQLite
  integer aggregation over u256 text. Display units resolve by `(Network, AssetId)`.
- Explicit verification compares persisted totals with an independent fold; it does
  not silently repair them. Registration/re-watch seeds totals from retained facts.
- Each query reads facts, checkpoint and completeness in one snapshot. Consumers must
  not rely on the indexer to correct a misleading repository answer.
- Complete history requires proven coverage from the origin through the answer's
  checkpoint. Birthday-only coverage is incomplete; an overdrawn result stays so.
- Verified opening balance requires retained evidence for the same holding and exact
  canonical block, an explicit writer and invalidation/replay rules. It proves an
  opening amount, not complete historical transactions. Never add unwritten evidence
  columns or let a node witness overwrite the indexed number.
- A separate optional repository activity query exposes balance movements, including
  credits with no transaction. Transaction history continues to contain transactions.
- Pending-inclusive consumers combine pending samples with repository results using
  the result's own checkpoint and revision checks. Commits may neither omit nor
  double-count a block; historical output reads must not claim premature spendability.
- History pagination preserves native order and matching-transaction limits. Catch-up
  inserts must not be presented as a completed export. Native fees retain their own
  asset when projecting token history.

## Wallet custody

The server owns encrypted application tables in the same SQLite file, with a schema
version independent of indexing. Require an environment-sourced passphrase; no
implicit plaintext mode. Use authenticated encryption, random salt/nonces, and
zeroizing key/plaintext memory. Fail closed on wrong passphrase or modified rows.

Creation is registry-first, observation-second, publication-last. A crash before
observation leaves a private unserved key; no read may publish it as a usable wallet.
A successful creation must survive restart with the same ID/address and ability to
sign. Restore only keys matching durable observations; watch-only addresses remain
watch-only. Offline index purge preserves keys. No endpoint exports a key.

Factories generate/validate native keys and canonical addresses. `Wallets::create`
and `from` retain neither instances nor IDs. The server's `Signers` is its map.
Membership validation checks native type, configuration and shared dependencies;
asset tags alone are not proof that wallets share an outbox.

## Sending

- A batch is 1..=50 ordered occurrences (`wallets::MAX_TRANSFERS`). `Wallets::send_all` is the
  authoritative guard; concrete senders re-check before chain I/O. Empty and oversized batches are
  `InvalidBatch` with no failed index.
- Each occurrence keeps its zero-based position. Order and multiplicity are preserved; identical
  items are separate payments.
- One request targets one asset. Mixed assets (including ETH + USDC) are rejected before any
  external effect.
- Validation order: amount, wallet lookup, asset compatibility per occurrence in order; then
  complete chain preparation; then ordered broadcast. Every chain prepares the whole batch before
  the first broadcast.
- `SendError`: definitely acknowledged IDs, optional `failed_index`, source error, optional
  `ambiguous_transaction_id`. An index is present only when one original occurrence truthfully
  failed. Accepted IDs are never inferred from the index.
- The ambiguous ID is derived by the chain from the exact locally signed envelope only; conversions
  preserve it, HTTP projects it (always `503`). Provider prose or a mismatched returned ID never
  becomes one. It is reconciliation metadata, not an idempotency key.
- Submitted is not confirmed. Only indexing establishes inclusion and confirmation. An RPC failure
  never implies a dropped transaction.
- Exact signed bytes are kept across ambiguous outcomes; replays are byte-identical; nothing is
  rebuilt or re-signed once it may have reached the network.
- Submission coordination is process-local: one active API writer per managed source address.
  Restart, response loss, or a new invocation after an unknown outcome can double-pay; callers MUST
  NOT auto-retry an unknown logical payment.

| Chain | Batch behavior |
|---|---|
| Bitcoin | one transaction; may fund from several source wallets read at one output checkpoint; one output per transfer, per-source change, deterministic fee split; spent outputs stay reserved against concurrent sends until indexed spent or refused; returns one ID |
| Ethereum | one transaction per transfer, consecutive nonces per sender; whole batch simulated, balance-checked, and signed first; stops at first failure with accepted prefix |
| Solana | one transfer+Memo transaction per occurrence; whole batch fee-checked, signed, and simulated first; stops at first failure/ambiguity with acknowledged prefix |

## Chain requirements

### Bitcoin

- Parse addresses with the `bitcoin` crate and enforce the configured network; startup checks
  network (no genesis-hash check).
- Keep exact outpoints and every input/output; history exposes inputs and outputs as separate
  movements. Pending reorgs restore the spendable view without rewriting confirmed storage.
- Sign only implemented script/address kinds; verify each input belongs to its signer. Fees are
  checked integer satoshis.

### Ethereum

- Startup verifies chain ID (no genesis-hash check). Build validates chain ID, nonce, gas, and
  EIP-1559 fees; recovered signer MUST equal the sender. Values are exact `U256`.
- A factory selects exactly one asset: native ETH or one allowlisted ERC-20. Generated wallets
  use independent keys; same-address native/token observations remain distinct holdings.
- Token startup verifies chain ID, deployed code, decimals, and a balance read at one canonical
  block through one endpoint (hence USDC requires exactly one RPC endpoint).
- ERC-20 send: target only the configured contract, zero value, simulate the exact call, accept
  only ABI `bool true`; check token funds and native ETH for worst-case gas.
- History shows only the wallet asset's movements plus the native fee: an ETH wallet also lists
  the token sends it paid gas for, a token wallet only its token movements and its failed
  transactions. The repository selects them; no chain filters a page afterwards.
- One process-wide `Outbox` assigns nonces per sender address (shared by ETH and token
  factories). `eth_sendRawTransaction` is one attempt on the first endpoint, no failover; an
  ambiguous envelope blocks that sender until exact-hash lookup or byte-identical replay resolves.

### Solana

- SOL sending is supported; allowlisted SPL holdings are receive-only and expose no
  authorship capability. Keys are Ed25519; imports use exactly one 32-byte seed.
- Addresses are 32 bytes in canonical plain Base58. Amounts are checked `u64` lamports
  (1 SOL = 10^9) with exact decimal conversion.
- Use the modular `solana-*` crates; no `solana-client`, monolithic SDK, handwritten wire
  encoding, or copied System Program discriminants.
- Startup verifies `getGenesisHash` against configuration and that the Memo-v3 program
  (`spl_memo_interface::v3::ID`) is executable, before touching the database. One logical
  operation MUST NOT span RPC endpoints; the Solana endpoint is singular with no transparent retry.
- Indexing admits finalized slots only, traverses by slot/parent slot (sparse, never synthesized),
  interprets legacy and v0 transactions (including loaded addresses and inner System transfers),
  and emits no UTXO outputs. Inconsistent metadata invalidates the whole block.
- Each payment is one legacy transaction: one System transfer + one zero-account Memo-v3 carrying a
  fresh 256-bit CSPRNG Base58 token. Source is sole signer and fee payer. The token makes
  transactions unique; it is not request idempotency.
- Destinations MUST be on-curve and either absent or an existing non-executable, System-owned,
  zero-data account. General address parsing still accepts off-curve addresses.
- No priority fees, Compute Budget, commitment selector, lag/reference/quorum, or retry knobs;
  any such configuration or request member is rejected, never ignored.

Account acquisition (one per send, one endpoint, no retry, no chunking):

```text
getHealth() == "ok"
  -> getSlot(confirmed) = F
  -> getMultipleAccounts(confirmed, base64, minContextSlot = F) = (C, values)   // <= 100 addresses
  -> getSlot(confirmed, minContextSlot = C) = U
  -> atomic eligibility + balance handoff with operation floor P = U
```

- Addresses are deduplicated in first-occurrence order (source, then destination). Response MUST
  have exact cardinality, strict Base64, canonical owner, data length = space, `C >= F`, `U >= C`.
- Any failure is operation-wide and index-free: no floor, no facts, leases released, no fee call,
  signing, or broadcast. An unsupported account shape reports the earliest occurrence using it.
- This is endpoint-local consistency, not a freshness or maximum-lag guarantee.

Submission:

- A process-local outbox leases every source in byte order before RPC. A busy or ambiguously
  guarded source returns `SourceBusy` (`503`) with no RPC. Self-transfers fail before RPC.
- After the handoff: one confirmed blockhash, build every message, exact fee per message (`null`
  fee fails), checked cumulative `amount + fee` per source, sign and verify, simulate every
  signed transaction at the floor. Any failure here means zero broadcasts.
- Blockhash expiry uses confirmed block height (valid through `lastValidBlockHeight`), checked
  before each broadcast.
- Broadcast in order, preflight on, provider retries off; returned signature MUST equal the local
  first signature. On an unknown outcome: at most two byte-identical replays (three calls), each
  only after a valid null signature status and an unexpired blockhash.
- Once a send call starts, every timeout, disconnect, cancellation, RPC error, malformed response,
  or signature mismatch is ambiguous.
- A guarded source stays blocked until signature status or indexed history proves observation, or
  blockhash expiry plus a complete checkpoint-stable finalized history scan (pages <= 100, one
  unchanged checkpoint) proves absence. Missing evidence blocks indefinitely. Reconciliation
  retries on checkpoint notifications with backoff 500 ms to 10 s.


### Spend-all requests

- Wire `amount: "all"` and SDK `Amount::All` use the existing single/batch outbox.
  Exact amounts remain exact. Do not read an HTTP balance and race a later fixed send.
- At most one spend-all per source; it must be that source's final occurrence.
  Reject ambiguous selection before RPC; incoming batch transfers are not funding.
- Bitcoin uses all spendable unreserved inputs, subtracts deterministic fees and prior
  fixed outputs, and sends the remainder to the requested destination with no source
  change. Empty/dust remainders fail; never silently drop the sweep output.
- Ethereum reserves cumulative maximum gas fees, resolves under the source's nonce
  reservation, and validates the final value-dependent transaction. Token remainder
  and native gas are separate. EIP-1559 refunds can leave ETH; promise no exact-zero
  residue. Preserve nonce ordering, accepted prefixes and exact-envelope ambiguity.
- Solana resolves after account acquisition/fee pricing, rebuilds the same memo and
  lifetime with final amounts, validates final fees/context floors and simulates before
  submission. SPL sending remains unsupported.
- Watch-only sends return 422; an empty sweep returns 400. Fee bounds still apply.

## Composition, health and lifecycle

1. Validate closed configuration, credentials and server security.
2. Construct long-lived clients; verify all network identities and required native
   capabilities before opening storage. No per-request dependency construction.
3. Open the SQLite file off the async executor, validate its identity and migrate.
4. Compose all network indexers over one repository, plus factories and query handles.
   Duplicate networks/assets fail without launching background tasks.
5. Unlock application keys, restore signers and observations, and register configured
   wallets before the first pass. Keep the SDK independent of custody policy.
6. Start indexing and supervised submissions. Bind HTTP once every network has settled
   at least once, Ready or Quarantined. Incomplete historical catch-up does not gate it.
7. Supervise serving, cancellation, task failures and joins. One indexer's failure must
   not terminate healthy networks.

Retry transient failures with bounded jittered backoff; quarantine terminal errors and
panics without automatic retry. Readiness is `all(Ready | Quarantined) && any(Ready)`;
all-quarantined returns 503 while the process remains observable. Startup retry deadline
is not currently a product requirement.

`GET /healthz` is public, memory-only, 200 when ready and 503 otherwise. Include
network state/checkpoint/pending/failure/owed-history and per-endpoint attempts,
failures, consecutive failures and sanitized last failure. Do not publish URLs,
credentials, request content or remote error prose. Refresh state snapshots as passes
advance; notification callbacks keep their transition/failure cadence.

The SDK can resume a live quarantine. For the server, fixing its cause and restarting
is the operator resume workflow: a new CLI process cannot control an old process's
RAM. This supersedes the proposed `--resume`; do not add an administration transport
solely to preserve that name. Confirmed reorgs require offline `--rescan`; durable
backfill cancellation is an offline operator operation, not a product HTTP route.
Recovery must preserve keys, registrations and other networks and run no listener or
supervisor. Invalid targets fail before mutation.

Submission registration succeeds only once tasks are tracked. Shutdown marks unready,
stops HTTP admission, closes registration and drains handlers/submissions while
indexing can still reconcile. Only then stop and join indexing/storage. Ambiguous
payments have no invented shutdown deadline or promise of durable outgoing recovery.
Blocking database work runs off async executors with bounded admission.

## Verification

- Run every AGENTS gate, feature-isolation checks, submissions tests, OpenAPI comparison
  and a fresh MCP report; unconfigured/failed linter rules are not passes.
- Default tests own RPC doubles/temp stores and never contact public networks.
- Run shared repository contracts against SQLite and memory: atomicity, duplicate and
  uncertain commits, migration/restart, signed totals, activity, birthdays, sparse
  coordinates, pending/confirmed forks, paging, network isolation and offline purge.
- Test the real router's validation/auth/errors, fixed/all single/batch requests,
  watch-only/empty refusals, secret exclusion, memory-only health and RPC failure stats.
- Restart a created wallet over the same encrypted file and prove it can still sign
  and spend; prove wrong-password/corruption refusal and interrupted registration privacy.
- Execute all six examples and server create/receive/read/restart/send against owned
  local Bitcoin, Anvil and checksum-pinned Solana validators. A compile is not runtime
  proof; preserve logs and report precisely which behavior ran.
- Keep exact-chain guarantees covered: Bitcoin input/change/maturity/fee ownership,
  Ethereum native/token movements and nonce ambiguity, Solana memo/lifetime/context
  floors and complete finalized-history recovery. Observe fee-training and node-history
  preconditions honestly; never replace a failing real scenario with only a mock.
