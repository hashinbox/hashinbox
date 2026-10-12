# 056 — The server on payments: the trim map

Status: implemented (commit 071c859).

## Schema amendment (from the review question)

The spender is usually not a stored transaction: a spending transaction gets
a row only when it touches a watched address, and most spends do not. So
`spent_transaction_id REFERENCES "transaction"(id)` dangled on nearly every
spend. Decision (stronger than "nullable, no FK"): **delete the column**.
Keep `spent_block_id` only — repair keys on *when* the spend happened, never
on *who* spent it. `schema/sdk.sql` reflects this; the tombstone rule is
unchanged (creation fields NULL + `spent_block_id` NOT NULL).

## AppState: 13 fields → 4

| Today | Goes to |
|---|---|
| `networks`, `verify`, `control`, `purging` | `payments.networks` |
| `health` | `payments.networks.health()` / `health_receiver()` |
| `index`, `pending` | inside `Payments`; handlers use `payments.watch()` and nothing else |
| `registry` | today's `payments.registry`; becomes the wallets crate's `WalletRepository` (055 phase 2), which loads rows and constructs wallets |
| `units` | dies — plumbing for the above |
| `wallets` | `payments.wallets` |
| `signers` | **stays**, as a thin map (see below) |
| repo handle | stays, named: the app is concrete (see below) |
| `pool` | stays — `rpc_endpoints` rows only, wired into `networks.replace(persist)` |

```rust
pub(crate) struct AppState {
    pub(crate) payments: Payments<AssetTag>,
    pub(crate) signers: Arc<Signers>,
    pub(crate) repo: Arc<indexing::sqlite::Repository>,
    pub(crate) pool: SqlitePool,
}
```

**The app names its backend.** `apps/` is the concrete composition — it
opens sqlite, it holds `Arc<Repository>`, handlers read balance/history
through `state.repo` (method calls, no type named in handler modules). The
old check-8 clause banning a storage-backend name in `state.rs` is retired;
its chain and `payments::config` clauses stand. No `Reads` trait — that was
indirection with no customer in a single-backend app.

**Seam rule.** Indexing accepts `&dyn base::Holder` (`address()` + `asset()`)
and nothing from the wallets crate — `wallets` imports only `base`, either
direction. Watching a wallet is payments' startup join:
`wallet_repo.active()` → `payments.watch(id, wallet, …)`. Nothing here
depends on wallet types.

## Module-by-module

**main.rs (310 → ~80).** Keep: clap CLI, `Config::read`, storage open,
`Builder::start()`, router bind, `tokio::select!`. Delete: submissions
construction (payments), endpoint seeding/`with_stored_endpoints` (moves to
payments' endpoints wiring), `live()` (payments.networks), verifier fn
(payments), driver spawn + 8-step shutdown (payments.shutdown()). The
readiness wait payments does at start also replaces the server's
start-without-readiness behavior.

**api/networks.rs (474 → ~200).** Delete the live-state half (`LiveNetwork`,
`Verifier`, `Rejected`, impls) — payments' `networks.rs` already has
list/get/health/replace/pause/resume/rescan. Keep: route DTOs
(`ServiceState`, `RpcNode`, `ConfiguredNetwork`, `ReplaceRequest`,
`NetworkPath`), the three routes as thin calls, credential-hiding
(`origin()` in listings), and the `persist` closure = `RpcEndpoints::upsert`.

**models/signers.rs (112 → ~50).** Keep the id→wallet map and insert/remove.
`load()` = `wallet_repo.active()` — the wallets crate's store loads the
non-archived rows, reconstructs each `Wallet` from its stored key, verifies
the address — then insert into the map. No key handling, no `from_pk` calls
here; the store owns reconstruction.

**api/wallets.rs (330 → ~200).** The create flow (register → watch → signer)
stays a handler, but each step is one call — `wallet_repo.register` (or
`payments.wallets.generate/from_pk`), `payments.watch`, `signers.insert`.
The inline orchestration shrinks to sequencing. Tag mapping moves into the
wallets restore path (055 phase 2).

**operator.rs — deleted.** Online `payments.networks.rescan`
(pause → purge → resume) covers recovery; quarantined networks don't block
startup, so "repair needs a dead server" is gone. Revokes the AGENTS line
"exposed offline as `payment-server --rescan`" when it lands; `purge` stays
on `IndexStore` as the port method payments calls.

**models/endpoint.rs, ui.rs, models wire DTOs, config.rs, chains.rs,
asset.rs, storage.rs, api/mod.rs, api/health.rs, api/balance.rs,
api/transactions.rs, api/assets.rs, api/openapi.rs — stay.** Health keeps its
route but sources from `payments.networks.health()` plus wallet queue stats.

## Payments-side gaps to fill first

- `Networks::replace` already takes a `persist` closure; the server passes
  `RpcEndpoints::upsert`. No payments change.
- `wallet_repo.active()` (055 phase 2) is the only new store API the server
  consumes; until phase 2 lands, `payments.registry` serves the rows.

## Numbers

Server today: 3,984 lines. Moving/deleting: main.rs startup+shutdown ≈ 230,
api/networks.rs live half ≈ 200, signers load ≈ 60, wallet-flow duplication
≈ 130, operator.rs 81. Expect ≈ 2,700–2,800 after, almost all HTTP + DTOs.

## Rollout

1. `AppState` cutover to 4 fields; retire the check-8 storage clause (update
   AGENTS; the check script itself no longer exists on disk). Gates.
2. api/networks.rs thin-over-payments; delete live-state half. Gates.
3. main.rs slim (submissions/endpoints/verifier/shutdown → payments). Gates.
4. signers thin + wallet-flow dedup. Gates.
5. operator.rs deletion + AGENTS line revocation. Gates.
