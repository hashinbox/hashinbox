# 074 — Testing: recorded-truth chain mocks, one deep server suite (plan, not implemented)

## Decisions

- **Offline, in-process only.** Real nodes are used only by hand, to record.
- **Generic pieces in `sdk/base`** (`base::testing`, feature `testing`):
  `TestChain`, `Caps`, `Topology`, `Model`, `Chain<M>`, `Node<M>`, controls,
  fault hooks, `Clock`, shapes-check helper. Nothing chain-specific.
- **Each chain implements them** in `sdk/chains/<chain>/src/testing/` (feature
  `testing`): its `Model`, recordings, corpus, keys, fixtures, `TestChain`.
- **One-way deps**: server → chain `testing` → `base/testing` → `rpc/testing`.
  The SDK never imports the server.
- **Mocks answer like real nodes**: exactly the methods `rpc.rs` calls; shapes,
  codes, exact messages and edge behaviours (`null` receipts below a pruning
  floor, pruned/skipped slots, rate limits, refusals) come from recordings,
  refreshed by hand. The shapes check guards against drift.
- **The full suite is `apps/server/tests`** (current contents replaced): cases
  start at the HTTP API (wallets, indexes, sends, triggers), the REAL workers
  (index, broadcast, expire, triggers) run over mock nodes, results are read
  through the API (DB only where the API cannot show it). One scenario set
  for every chain, single node and cluster; unmet capabilities skip visibly.
- **The server only uses** base + chain `testing` (`tests/support/`: `App`, API
  client, `scenarios!`). A new chain (TON, Sui, Tron…) implements `Model` +
  `TestChain` and gets the suite. **Kept**: `Http` tests, per-file units, `Script`.

## 1. Today's tests (why they change)

- Fake network (`apps/server/tests/mod.rs:27-209`, chain `Empty`, `routes.rs:69`
  injects `Blocks`); tests call `crate::models`, raw SQL, `usecase::workers`.
- Guessed replies/refusals (`bitcoin/tests/chain.rs:74`); no 429/lag tests; the
  Sepolia `null`-receipts stall was unreproducible.

## 2. Levels

| Level | Where | Proves |
|---|---|---|
| Unit | `#[cfg(test)]` in each file | parsing, classifiers, `Cluster`/`Failover`/`Retry` policy (`Script`, `Faults`) |
| Transport | `packages/rpc/tests/http.rs` | `Http` alone on a loopback listener: statuses, 429/503 HTML body, timeout, truncated/invalid JSON, wrong `id`, reordered/short batch, oversize body |
| Truth | `base::testing` units; `sdk/chains/<c>/src/testing/` | `Chain`/`Node` controls; mock vs recordings (shapes check); every classifier over every corpus row |
| Server | `apps/server/tests` | API in → real workers over mock nodes → API out, per chain × topology |
| Record | `#[ignore]` + `RECORD=1`, by hand | refreshes recordings from public/testnet nodes; never in CI |

## 3. Layout

| Path | Owns |
|---|---|
| `packages/rpc/src/testing.rs` | `Script`; `Faults<C: Client>`: down, delay, transport `ErrorKind`, `(code, message)`, rate limit after N, fail next, broken/partial result |
| `sdk/base/src/testing/` (`testing = ["rpc/testing"]`) | `TestChain`, `Caps`, `Topology`; `Model` trait; `Chain<M>` (block tree, mempool, controls), `Node<M>` (view + `Faults`, `impl rpc::Client`); `Clock`; `Recording` + skeleton compare |
| `sdk/chains/<c>/src/testing/model.rs` | `impl Model`: state, tx apply, answers from recordings; `pub type Mock = base::testing::Chain<Model>` |
| `…/testing/recorded/*.json` | `{provider, network, recorded_at, method, params, result\|error}`, ≤ 200 KB/file |
| `…/testing/{chain,corpus,record,fixtures}.rs` | `impl TestChain`; corpus test over `refusals.json`; record step; keys, token, test `NetworkConfig` |
| `apps/server/tests/support/` | `App`, API client, `until`, `scenarios!` — uses only `base::testing` + chain `testing` |
| `apps/server/tests/cases/*.rs` | `async fn case<T: TestChain>(app: &App, chain: &T)` |
| `apps/server/src/models/*` | store/SQL tests, moved beside their models |

## 4. Mock chain and nodes (generic in `base::testing`, `Model` per chain)

- **Methods** — only those `rpc.rs` calls; any other gets the real
  method-not-found:
  - BTC: `getblockchaininfo`, `getblockcount`, `getblockhash`,
    `getblockheader`, `getblock` v3, `estimatesmartfee`, `sendrawtransaction`.
  - ETH: `eth_chainId`, `_blockNumber`, `_getBlockByNumber`,
    `_getBlockReceipts`, `_getTransactionReceipt`, `_getTransactionByHash`,
    `_getTransactionCount`, `_estimateGas`, `_maxPriorityFeePerGas`, `_call`,
    `_sendRawTransaction`.
  - SOL: `getGenesisHash`, `getSlot`, `getBlock`, `getBlocks`, `getBlockTime`,
    `getFirstAvailableBlock`, `getLatestBlockhash`, `getFeeForMessage`,
    `getMinimumBalanceForRentExemption`, `getAccountInfo`, `sendTransaction`.
- **Rendering**: answers are recorded responses used as templates, filled
  from mock state; errors are recorded `(code, message)` rows. No hand-typed
  JSON. ETH/SOL signatures verified; BTC checks unspent inputs and fee floor
  (scripts: signing goldens).
- **`Chain<M>`**: `Arc<Mutex<_>>` block tree (state snapshot per block),
  canonical tip, one mempool, logical clock. Controls: `fund`, `mine(n)`,
  `hold`, `drop`, `replace`, `reorg(depth)` (old branch kept, unconflicting
  txs back to mempool), `prune(below)`, `skip(slot)`, `finality_lag(n)`.
- **`Node<M>`** (several per `Chain<M>`): head = canonical or pinned-fork tip −
  `lag`; `pruned_below` (recorded BTC pruned error, ETH `null` receipts, SOL
  -32001/-32009); `duplicate: Accept | Reject`; `Faults` (down, recorded
  -32005 / 429, slow, fail next, broken). `Model` gives `apply`, `seal` and
  `answer`; UTXO/account/slot specifics stay in the chain.

## 5. Time and finality

- **Chain time is logical**: each `mine` adds 600 s / 12 s / 400 ms.
- **Wall time is real but short**: `interval_ms` ≈ 5, small `pending_ttl_ms`,
  API polled with `until(5 s)`. No paused time in the server suite (SQLite's
  blocking pool would fire timeouts early); unit tests may pause.
- **Finality per node**: BTC from configured depth; ETH `finalized` = head −
  lag, `null` at genesis; SOL confirmed = head − 1, finalized = head − lag,
  blockhash expires after 150 slots.

## 6. `TestChain` (`base::testing`) and the server harness

```rust
pub trait TestChain: Sized + Send + Sync + 'static {
    const CAPS: Caps;                              // Token | Replace | Reorg | Prune | Skip | Cluster …
    fn start(nodes: usize) -> Self;                // impl in chain: Chain<Model> + n Node<Model>
    fn nodes(&self) -> Vec<Arc<dyn rpc::Client>>;
    fn network(&self) -> base::NetworkConfig;      // chain fixtures: tokens, depth, short ttl
    fn fund(&self, to: &str, amount: &str, asset: &str);
    fn mine(&self, n: u64);   fn hold(&self, on: bool);
    fn reorg(&self, depth: u64);   fn prune(&self, below: u64);
    fn node(&self, i: usize) -> NodeControl;       // lag, fork, pruned_below, duplicate, faults
}
```

- **`App::start(&T, Topology)`** seeds a tempdir SQLite as `serve` does, then
  runs `boot` (from `main`) with `Payments::with_client` — `Single` =
  `nodes()[0]`, `Cluster` = `Retry(Cluster::new(nodes, height, Policy))` —
  and every worker and the router.
- **Cases** call routes only and read balances, legs, pending, transactions,
  blocks, index state, jobs via the API; `app.db()` only for hidden state.
- **`scenarios!(Bitcoin, Ethereum, Solana)`** emits `<case>_<chain>_<topology>`
  tests; unmet `CAPS` print `skip`; a meta-test pins each skip list.
- **Code changes needed**: extract `boot(db, clients)` from `main`; make each
  chain's `height` probe generic over `C: Client` (now `&rpc::Http`).

## 7. Keeping mocks truthful

- **Shapes check (offline, per chain)**: for every recording the mock answers
  the same request on an equivalent state; field set and value types match
  recursively (`null`s included); error code and message match exactly.
- **Record step (by hand)**: `RECORD=1 cargo test -p chain-<c> record -- --ignored`, URLs from env.
  - ETH (Sepolia): blocks, receipts, pruned `null` receipts, and refusals
    (nonce, underpriced, funds, duplicate) from a funded test key, plus a
    free tier's rate-limit reply.
  - SOL (devnet): SPL blocks, skipped/cleaned-up slots, preflight refusals,
    "already been processed", "Blockhash not found".
  - BTC (testnet4/signet): v3 blocks, the pruned error, -25/-26/-22/-27.
- Diffs reviewed; a surprise → recording → mock control → case. Reorg, lag,
  fork, finality lag are modeled (no node shows them on demand), in recorded shapes.

## 8. Fast path

- `cargo test` runs every level but Record: no node, Docker or network; a socket
  beyond `http.rs`'s listener and the App's loopback router is a bug.
- < 20 s warm, < 300 ms per server case, parallel, one SQLite file each;
  nextest `slow-timeout 5s × 6`, `retries = 0`.

## 9. Scenario matrix (S single · C cluster · — n/a · Src: R recorded, M modeled)

| Case (API in → workers → API out) | BTC | ETH | SOL | Src |
|---|---|---|---|---|
| Network tip, finality, index state | S C | S C | S C | R |
| Watch → index → legs, balances (native) | S C | S C | S C | R |
| Same for token (ERC-20 / SPL) | — | S C | S C | R |
| Send → pending → mined → balances | S C | S C | S C | R |
| Broadcast reaches each node once; duplicate | S C | S C | S C | R |
| Rejected: spent/nonce, fee, funds → send failed | S | S | S | R |
| Replaced | S | S | — | R |
| Dropped / expired → expire job frees funds | S | S | S | R |
| Unknown outcome (timeout mid-send) → same bytes rebroadcast | S C | S C | S C | M |
| Reorg above confirmed → legs/totals rewound | S C | S C | S C | M |
| Reorg below confirmed → quarantine visible | S | S | S | M |
| Pruned → index skips, gap visible (Sepolia stall) | S C | S C | S C | R |
| Receipts not ready near tip → index waits | — | S C | — | R |
| Skipped slots | — | — | S C | R |
| Trigger fires at tip → its sends broadcast | S C | S C | S C | R |
| Lagging / forked node → `max_lag` primary | C | C | C | M |
| Node down, rate limit → failover, jobs keep going | C | C | C | R |
| Broken / partial result → retry, no bad rows | S C | S C | S C | M |

## 10. Rollout and effort

1. **First slice: base + Ethereum + server (~4 days, ~1 700 lines)**: `Faults`
   ~150; `base::testing` (`TestChain`, `Chain<M>`, `Node<M>`, `Clock`,
   skeleton) ~500; `boot` + generic `height` ~100; `chain-ethereum/src/testing/`
   (`Model`, `TestChain`, Sepolia recordings) ~450; server support ~200; six cases
   ~300 — index native + ERC-20, send → pending → mined, duplicate, reorg,
   pruned `null` receipts (the stall), lagging node in a cluster.
2. **Bitcoin `src/testing/` (~350)**, testnet4 recordings, rest of the
   matrix; delete `apps/server/tests/{mod,pipe,routes,send}.rs`, fake
   network, `Empty`.
3. **Solana `src/testing/` (~500)**, with devnet recordings: commitments,
   skipped slots, SPL, blockhash expiry.
4. **`http.rs` (~250)**: independent, can run in parallel.
5. **Shrink**: `storage.rs` → model unit tests; drop `solana-node` and
   order-pinning `Script` tests. New chain: `src/testing/` 300–500 lines
   (`Model` + `TestChain`) + recordings.

## 11. Open questions (recommended answer)

1. **Endpoint routes build `Http` from URLs; a mock has none.** Inject a
   `Connector` (`Endpoint → Arc<dyn Client>`) into `boot`; tests map
   `mock://<node>`, so endpoint updates and `Swap::replace` go via the API.
2. **Keep `#[path]` in-binary tests?** Yes, with a grep test that limits cases
   to `support` (`boot`, router, `db`).
3. **`sdk/base` gains a `testing` feature (core crate).** Off by default,
   dev-deps only, pulls `rpc/testing`; no production code may name it (CI
   builds the workspace without it).
4. **How strict is the shapes check?** Field set and types for results; exact
   errors. Values are pinned only where the SDK branches on them.
5. **Bitcoin has no free public JSON-RPC.** Use a provider key from env
   (testnet4/signet), or a one-off local bitcoind, by hand.
6. **When to re-record?** Before a release and on provider/node upgrades.

## References

[BTC reject strings](https://raw.githubusercontent.com/bitcoin/bitcoin/master/src/validation.cpp) · [BTC pruned error](https://raw.githubusercontent.com/bitcoin/bitcoin/master/src/rpc/blockchain.cpp) · [SOL RPC errors](https://github.com/anza-xyz/agave/blob/master/rpc-client-api/src/custom_error.rs) · [SOL devnet limits](https://solana.com/docs/core/clusters) · [EIP-4444](https://eips.ethereum.org/EIPS/eip-4444) · [nextest slow tests](https://nexte.st/docs/features/slow-tests/)
