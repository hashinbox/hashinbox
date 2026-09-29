# System requirements

Canonical scope, ownership rules, and behavioral invariants. `ARCHITECTURE.md` explains the
structure; current source and rustdoc are authoritative for exact signatures. Per-chain module
layout is in `docs/CHAIN_LAYOUT.md`. Indexing and storage detail: `docs/INDEXING.md`. HTTP
surface and configuration: `docs/API.md`.

## Product boundary

One API process (`payment-server`, `apps/server`) MUST:

1. initialize Bitcoin, Ethereum, and Solana RPC clients and embedded indexers;
2. generate, import, or watch addresses through one chain-neutral wallet collection keyed by
   `payment_api::AssetTag`;
3. return a wallet's canonical address, exact balance, and complete paginated history;
4. send one transfer or a non-empty ordered batch;
5. continuously index the authoritative wallet address/birthday set;
6. survive restarts without losing checkpoints, history, live outputs, or retained reorg recovery;
7. expose business and HTTP behavior without naming a concrete chain after composition.

Out of scope (MUST NOT exist): deposit records, ledgers or user credits, collection/sweep jobs,
reservations, gas sponsorship, payment state machines, durable outgoing-operation or
request-idempotency stores, hardware wallets, remote signers, HSM/KMS or production custody,
separate wallet/indexer processes or internal transports, public migration commands, HA or
multi-writer deployments.

Pre-release policy: replace obsolete internal Rust and wire formats directly. No compatibility
aliases, legacy readers, or `V1`/`V2` DTOs. **The on-disk index format is not an exception and
is not a compatibility contract.** This requirement previously carved one out -- the stored redb
layout was durable and MUST be preserved, because a deployment's existing index file had to keep
reading -- and it is deliberately revoked, not quietly dropped, by the move to one SQLite store
(`docs/design/030 §8 (ex-031)`). There is one storage backend, there is no reader for any earlier format, and
there is no upgrade path from one: no deployment exists, so the exception protected nothing and
cost every stored record a shape frozen for the life of the file. What replaces it is ordinary:
the file carries `PRAGMA user_version`, a schema step may alter what an earlier step wrote, and a
file this build cannot account for is refused rather than interpreted. External chain, HTTP, and
JSON-RPC standards remain compatibility contracts.

## Layering

| Layer | MUST | MUST NOT |
|---|---|---|
| `packages/*` | stay useful outside blockchain projects; JSON-RPC delegates framing to `jsonrpsee` and owns only bounded transport, retry, and ordered failover | import SDK or app crates; carry chain names, wallet DTOs, or indexing records |
| `sdk/base` | hold only semantics shared by substantially different chains (identity values, amounts, block coordinates) | import a chain, RPC, indexing, or wallets; define a universal RPC or transaction |
| `sdk/chains/*` | own native addresses, network checks, RPC, blocks, transactions, signing, fees, wallet factories, and indexing interpretation; use mature protocol libraries | leak native DTOs, envelopes, or outbox state into generic crates; depend on another chain |
| `sdk/indexing` (root) | define storage-independent sync, filters, history, checkpoints, outputs, and the store ports | know stored records, HTTP, wallets, or business labels |
| `sdk/indexing/src/storage/sqlite` (feature `sqlite`) | own every table, column, statement and schema step, and hold each whole operation in one transaction | expose stored rows through the crate's public API; own a sync task or filter registry |
| `sdk/indexing/src/supervisor` (feature `supervisor`) | drive one task per `SyncUnit`, own the per-network state machine, backoff, quarantine, and the `SyncMonitor` callback, until shutdown | own storage or wallets; end on one chain's failure |
| `sdk/wallets` | own `Wallets<A>`, the factory per asset key, wallet and watch entries, public metadata, birthdays, history presentation, and the chain-neutral submission-supervision port | own indexing persistence, a background runtime, or a custody policy of its own |
| `apps/api` | own the HTTP routes, wire models, and OpenAPI contract over `wallets` and `indexing` | depend on a chain crate, a storage crate, or a closed asset/chain enum |
| `apps/server` | be the only executable and composition root; name chains only in `src/chains{.rs,/}` | be depended on; hide the object graph behind a facade |

Each chain crate MUST be deletable without breaking generic crates or other chains. Do not
universalize UTXO inputs, EVM nonces/envelopes, or Solana messages; a generic
`Transaction { from, to, amount }` loses consensus-significant data.

## Wallets

- `Wallets<A>`: `A` is the asset key (`payment_api::AssetTag` in the server:
  `[a-z0-9][a-z0-9-]{0,31}`, one tag per factory, named by configuration). One registration,
  `Wallets::add(asset, factory)`, = one `WalletFactory` (its `FactoryProfile` supplies one
  `Network`, one `base::AssetId`, and one `Sender`; never decimals, a ticker, or a balance).
  Duplicate asset keys fail. It takes no index,
  no storage and no wallet id: it unifies factories and stores nothing else (docs/design/030 §8 (ex-029)).
- Factories own native key generation, validation, and address text; no caller knows an address
  encoding. Key *text* is chain-neutral: every supported chain's import format is 32 bytes as 64
  hexadecimal characters, so callers decode with `crypto::SecretBytes::from_hex` and
  `WalletFactory::create` checks the length and the curve. `Wallets::create` drops the material
  and returns the wallet; `Wallets::secret(tag)` returns fresh material to a caller that asks for a
  key before a wallet. Neither keeps anything, and neither reaches the index.
  Secrets MUST stay out of responses, schemas, logs, errors, and ordinary `Debug`. A key supplied
  to `POST /v1/wallets/import` is request input, zeroized after parsing.
- **Observation is the index's, not the wallet map's.** `indexing::Watching::watch(id, &dyn Holder,
  Start)` writes one `Watch` row through `indexing::WatchStore` — id, address, birthday, `AssetId`,
  and **no key material** — and inserts into the hot set each `Service` reads on every pass. That
  set is keyed by watch identity, so one address is observed once per asset and a wallet holding a
  chain's own currency and a token at it is two observations over one inspection. A
  `wallets::Wallet` is a `Holder`; so is a bare `indexing::Subject { address, asset }`, which is
  what a watch-only address is. `Indexing::sync` takes no selection.
- **A watch can be withdrawn.** This requirement previously read "There is no `unwatch`", and it is
  deliberately revoked, not quietly dropped, by `indexing::Watching::unwatch`. Both grounds it
  rested on are gone: the durable watch port's method budget was spent on an `advance` that was
  never built (progress is written in the commit's own transaction, `docs/design/030` step 12), and
  the fate of an unwatched address's committed history is no longer undefined -- it is *kept*.
  `unwatch` stops future inspection and deletes no indexed row: the history, the live outputs and
  the credits of the address stay, so the act is reversible, and the address may be watched again
  under any id from any birthday. What it does drop is that observation's own
  `indexed_through` (`WatchStore::forget`), because the row claims every applicable block from its
  birthday through that block was inspected *for that observation*, and that holds only while the
  observation exists. Another observation of the same address keeps its own claim, earned under its
  own birthday -- progress is per observation, so there is no shared row for a withdrawal to orphan,
  while *inspection* stays address-scoped because `BlockInterpreter::inspect` takes addresses and no
  asset. A re-watch therefore reads its range again -- bounded by the same `max_backfill_blocks`,
  idempotent where the rows exist, and never settling a gap. It takes the same `PublicationPermit`
  a `watch` takes, so a batch
  mid-inspection for that address is discarded rather than written after the row is gone, and it is
  idempotent: an id nothing watches is not an error. What replaced the refusal is narrower than the
  refusal was: rows nothing claims are tolerated, a second removal path beside the offline
  `--rescan` is still forbidden, and an application that has lost the id cannot reach those rows
  through the HTTP surface at all.
- **An operator can stop asking for a range.** `indexing::Backfilling::cancel(&network)`, reached as
  `POST /v1/status/backfill/cancel`, settles every range that network owes: each affected watch is
  left as one created now would be -- same id, same address, birthday at the checkpoint's successor,
  **nothing inspected** -- and nothing indexed is removed. Settling by re-registration rather than by
  writing a claim is what makes it honest: a watch born above the line owes nothing below it, and
  `indexed_through` stays `None` until a block is actually inspected. It is a durable write and
  therefore MUST NOT live
  on `SyncControl`, which is a signal with no store handle. The raised birthday is what keeps each
  read's `completeness` honest once the range below it will never be read
  (`docs/design/024` section 7).
- **The order between the two durable facts about one id is a requirement.** `index.watch` MUST
  commit before the caller files the key. A crash between them leaves a watched address with no
  key — a watch-only address with correct history, repairable by re-importing the key — never a key
  nothing observes.
- `Wallets::assets`, `asset`, `parse`, and `secret` answer by tag, so an application never needs a
  chain enum or a wallet id to validate input. `Wallets::create` and `Wallets::from` return
  `Arc<dyn Wallet>` and keep nothing, so addressing, signing, balance and history are all
  reachable from the value the caller holds, without naming a chain crate. Sending and building a
  transaction are not: they belong to `Author`, which `Wallets::author(asset, wallet)` alone hands
  out. The id -> wallet map is the application's (`payment_api::Signers`); a container for it MUST
  NOT be added back to `wallets`.
- `Wallets::accepts(asset, &dyn Wallet)` and `Wallets::send_all(asset, transfers)` prove membership
  through `WalletFactory::validate` (`Origin`) — same type, configuration and shared handles —
  rather than through a tag recorded when a wallet was filed.
- `Start::Fresh` starts at the checked successor of the current checkpoint position, or 0 without a
  checkpoint; a skipped position activates at the next produced block. Nothing to fill in.
  `Start::At(p)` states an explicit birthday, and one below the checkpoint is filled in rather than
  discarded.
- **Progress per observation MUST be one contiguous, hash-verified block.**
  `WatchProgress::indexed_through` means every applicable block from that observation's birthday
  through that block was inspected for it, empty ones included. There is no second position and no
  expressible gap, which is what one reference can honestly describe.
  `WatchProgress::caught_up(checkpoint)` is **the one predicate** that decides both whether an
  observation is inspected for the blocks arriving now and whether reads touching it may claim
  completeness; its doc comment states both possible behaviours and which is chosen. The accepted
  cost is latency: an observation reading its own past does not see a payment arriving today until it
  reaches the present. A second notion of "still catching up" MUST NOT be introduced beside it, and
  existing coverage MUST be closed or re-read rather than relabelled -- a position carries no block
  hash, so it cannot become one (`docs/design/030 §8 (ex-032)`).
- Every add path MUST state a birthday and MUST NOT default one: `start` is a required property of
  `POST /v1/wallets`, `POST /v1/wallets/import` and `POST /v1/addresses`, and a required field of
  each `wallets[]` configuration entry, spelled the same way in both — the string `"fresh"` or a
  native block position. A start further below the checkpoint than `SyncPolicy::max_backfill_blocks`
  allows is refused synchronously by the call that made it, naming the earliest acceptable start,
  so a caller-supplied birthday can never become an unbounded background task.
- `WatchStore` and `Watch` are reusable SDK capabilities (`indexing::sqlite` implements
  `WatchStore` over one registration table keyed `(chain, network, asset, address)`, with its
  identity unique across the file). They MUST NOT move into `apps/api` or `apps/server`.
- Wallet capabilities: `indexing::Holder` and `Signer` are every wallet's; `SingleSender` and
  `TransactionFactory` are `Author`'s, the capability of a wallet whose asset this system also
  spends. A balance is not one of them: `indexing::Balances::balance` folds it from the index, so a
  watched address with no key answers it exactly as a wallet does.
  Code needing one capability takes that trait, not `dyn Wallet`.
- **Whether an asset is spent at all is declared once, by its factory.** `FactoryProfile::
  authorship` returns `Option<Authorship>`: the batch `Sender` and the `Author` upgrade of one of
  its own wallets. An asset this system receives and reports — a Solana SPL holding today — MUST
  declare `None`, and `Wallets::{author, send_all}` then refuse its tag at lookup. A chain MUST NOT
  carry its own refusal for an asset it does not send: an unspendable asset has no path to a
  builder or a send pipeline to refuse, because `Arc<dyn Wallet>` carries neither.
- History is the repository's, joined to the index's overlay by
  `indexing::ObservedHistory`: `indexing::History::history(&dyn Holder, Page)` (the selection
  contract is documented once, on `HistoryQuery`) and `apps/api` converts the page, scaling every
  movement and every fee by the `base::Unit` its own `AssetId` resolves to in the `base::Units`
  each chain's `Config` declared. `limit` counts matching transactions only.
- `indexing::Listener::committed(&BlockCommit)` is the one reaction hook: a list, synchronous,
  notification-only, at most once and never retracted, carrying the block, its transactions, its
  output changes and its unknown spends. A veto belongs in an `IndexStore`, and a store is a
  constructor argument, never a runtime registration.
- One chain-neutral address, `base::Address { network, value }`; `value` is always the chain's
  canonical text (Ethereum lowercase `0x` hex, Bitcoin canonical text of its network, Solana
  Base58), so string equality is address equality. User input is parsed only by the wallet's
  factory (`AddressParser::parse_address`, a `WalletFactory` supertrait, reached through `Wallets::parse`
  or, without a store or a node, through the chain's own `WalletConfig`, which implements the same
  trait and is what the factory delegates to); stored and
  filtered addresses convert back to native addresses strictly (exact canonical text).
- Restart MUST resume with the same address/birthday set that produced the checkpoint. The index
  does this itself, from its own `WatchStore` rows -- each with the block through which it has been
  inspected, from one snapshot -- before its first pass; a caller re-states
  nothing. Adding a *historical* address to an existing checkpoint still requires an explicit
  rescan of that network.

### Key custody

**The index holds no key material.** This reverses the earlier rule that it did
(`docs/design/030 §8 (ex-029)` §3.4): once the registry row exists for indexing reasons, a key in it is a
category error, and the same custody ruling this section already made about everything else now
applies to it. One rule, no per-chain exception — Bitcoin, Ethereum **and** Solana alike:

- A `Watch` row carries an id, an address, a birthday and an `AssetId`. There is no
  material field, and nothing writes one.
- A wallet generated or imported through the HTTP API is held in this process's memory for as long
  as the process runs, and nowhere else. After a restart its id reads back with
  `watch_only: true`, its history is intact, and a send from it answers `422`.
- A wallet listed in `wallets[]` startup configuration is reconstructed from its environment
  variable on every boot: that list is the application's key store, and it is what makes a wallet
  able to sign across a restart.
- A watched address has no key at all, which is the same state a restarted API-created wallet is
  in.
- Material never appears in a response, a schema, a log, an error, or `Debug`.

This is development custody in the process's own memory and the operator's own configuration, not
an HSM, KMS, or production custody claim (§Product boundary). The same statement appears in
`docs/API.md` §Routes ("Key custody") and `ARCHITECTURE.md` §Wallets; those three MUST agree.

**Custody is the caller's problem, and this is settled.** Keep the key you send to
`POST /v1/wallets/import`; treat `POST /v1/wallets` as development-only unless you add the
generated key to `wallets[]` yourself. The SDK MUST NOT grow a key store, an encryption-at-rest
option, or a key-management policy of its own; a deployment that needs one puts it in front of the
API.

The index holds no key material at all. There is no export path and none is owed: the
subcommand that once read the pre-`029` row was deleted with the redb backend, because no
deployment exists that could hold such a file. Key custody is the API caller's.

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
  movements. Reorg rollback restores spent outputs exactly.
- Sign only implemented script/address kinds; verify each input belongs to its signer. Fees are
  checked integer satoshis.

### Ethereum

- Startup verifies chain ID (no genesis-hash check). Build validates chain ID, nonce, gas, and
  EIP-1559 fees; recovered signer MUST equal the sender. Values are exact `U256`.
- A factory selects exactly one asset: native ETH or one allowlisted ERC-20. Generated wallets
  use independent keys; importing one EOA as both ETH and a token is rejected.
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

### Solana (native SOL only)

- SPL Token / Token-2022 are rejected as unsupported. Keys are Ed25519; import accepts exactly one
  32-byte seed as 64 lowercase hex characters from an environment variable. Custody is the same as
  every other chain's (§Wallets, §Key custody): a wallet generated through the API writes its
  observation, and no key material anywhere, through the index's `WatchStore`, so after a restart it
  reads back watch-only; a `wallets[]` startup import is rebuilt from the environment and writes the
  same key-free row.
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

## Composition and lifecycle (`apps/server`)

1. Validate the complete closed configuration.
2. Construct one long-lived client per chain; verify every chain identity (and Solana Memo) before
   any storage access.
3. Open the configured storage: one SQLite file, opened off the runtime, whose application id is
   validated and whose schema steps are applied in order before any chain reaches it.
4. Build one repository over it, serving every network: each operation names the network it is
   about, so there is no per-network handle and no per-asset one. That one value is the
   `WatchStore` (`add`, `all`, `forget`, `reanchor`) and `IndexStore` (`checkpoint`, plus the one `commit`) every
   index requires, plus the consumer capabilities this server's chains read.
5. Build chain services and one `Index` -- `Index::over(repository).chain(..).build()?` for a caller
   composing synchronously, `Index::of(indexers)` for one that composed some other way; either way
   construction rejects a duplicate network without launching a task. Share it through narrow
   `Indexing`/`Checkpoint` views, with `History`, `Balances` and `OutputStore` held per network.
6. Build `Wallets` over the `Index`, register one factory per asset tag, restore every
   persisted wallet and every observation from the store, then import every configured wallet
   before the first sync.
7. Start the indexing supervisor and the submission tasks; bind HTTP only after every network has
   settled - reached Ready with a persisted checkpoint at least once, or become Quarantined.
8. Supervise HTTP, sync, cancellation, and graceful shutdown in one process.

- Solana submission tasks register through an application-owned `mpsc` queue and `JoinSet`;
  registration succeeds only after the task is tracked. No handler or SDK object detaches an
  untracked task.
- One chain's terminal indexing failure quarantines that network and nothing else: the other chains
  keep syncing, `GET /v1/status` reports the state and the reason per network, and the process
  does not exit. Readiness is `all(Ready | Quarantined) && any(Ready)`; every network quarantined
  answers 503 and still does not exit, because a restart cannot fix a deterministic failure and
  exiting also kills the surface that explains it. `SyncControl::resume` is the only way out of a
  quarantine. A supervisor task that ends unexpectedly still publishes not-ready and closes
  admission; with a guarded envelope, shutdown waits (no deadline) while reconciliation stays
  available. Force-kill is the only other exit and accepts duplicate-payment risk.
- Readiness follows **forward** indexing only. History a network still owes below its checkpoint,
  for an address watched from before it was registered, MUST NOT hold `GET /health/ready` at 503
  or keep the listener from binding: such a range can be one a pruned node will never serve, and
  the rest of the chain is answerable. It is reported as `GET /v1/status`'s `networks[].backfill`
  — the owed range, how many addresses wait on it, and why the last attempt stopped — and every
  read it weakens reports `completeness` rather than a complete-looking answer
  (`docs/design/024` section 7). "Has not reached its tip" and "cannot finish one old range" stay
  distinct states.
- Graceful shutdown order: not-ready, stop HTTP admission, close registration, drain handlers,
  wait for guarded envelopes, then stop sync and storage.

## Testing

- Gates are listed in `AGENTS.md` (fmt, check, test, clippy, doc, `linter check .`).
- Default tests use owned RPC doubles and temporary repositories and never call a public network.
- Repository contract tests run every shared scenario against both shipped stores -- the SQLite
  file and `indexing::memory::Repository` -- as a pair of tests per scenario in
  `sdk/indexing/tests/repository_contract.rs`, so they stay statements about the ports:
  dense and sparse coordinates, atomic parent, foreign-network rejection, all-or-nothing commits,
  checkpoint-bound pagination, cursor stability under interleaved writes, and the offline purge.
- Route tests (`apps/api/tests/route_contract.rs`) serve the real router over test doubles of
  `WalletFactory`, `Wallet`, and `Indexing`: no chain and no storage are needed to cover every
  route.
- System tests run the composed binary against RPC doubles over one temporary index file
  (`apps/server/tests/{acceptance,wallet_api,server_api}.rs`): birthdays, restart restoration of
  generated, imported, and watched addresses, retained reorg, one and batch sends, readiness,
  shutdown, cross-chain network isolation, native/token coexistence, registry preservation.
- The real-validator Solana suite (`apps/server/tests/solana_node.rs`, pinned Agave
  `solana-test-validator`) is manual until a CI workflow owns the pinned tools.
