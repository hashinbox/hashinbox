# payment-api: the payment HTTP routes over wallet and index abstractions

## Why this crate exists

`payment-api` is the public HTTP contract of the payment server: handler state
[`Payments`], the [`router`], the [`openapi`] document, [`Signers`], and the
JSON wire models. It demonstrates the workspace's layering rule: routes reach
wallets, balances, history, sends, and watched addresses only through
`wallets::Wallets` (keyed by this crate's [`AssetTag`]) and
`indexing::Index`. It depends on no chain crate and no storage backend
(`linter.toml` layer `api`), so adding a chain or a token never edits a route.

It also owns the one thing `wallets` stopped owning: the map from the wallet id
this API mints to the wallet it holds a key for ([`Signers`]). The index owns
what is *observed*; this crate owns what can *sign*; a wallet read is the two
answers put together (`docs/design/030 §8 (ex-029)` section 3).

The binary that builds the concrete chains, storage, and supervisors and then
serves this router is `payment-server` (`apps/server`), the one composition
root.

## Concepts

| Concept | Type | Provided by |
|---|---|---|
| handler state | [`Payments`] (`Arc<wallets::Wallets<AssetTag>>` + `Arc<base::Units>` + `Arc<Signers>` + `Arc<indexing::Index>` + `Arc<dyn indexing::History>` + `Arc<dyn indexing::Balances>` + `indexing::SyncHealth`) | the composition root, once, through [`PaymentsParams`] |
| the wallets this process can sign with | [`Signers`] ([`Signers::keep`], [`Signers::get`], [`Signers::ids`]) | this crate; filled by the composition root and by `POST /v1/wallets` |
| routes | [`router`] | this crate; hardening from `http_server` |
| OpenAPI document | [`openapi`] | this crate; committed as `docs/openapi.json` |
| configured asset | [`AssetTag`] (and [`TagError`]), [`Listing`], [`AssetList`] | this crate; `wallets::Wallets::assets` for what it is, `base::Units` for what an amount of it is |
| wallet or watched address | [`Wallet`], [`WalletList`], [`CreateWallet`], [`ImportWallet`] ([`SecretText`]), [`WatchAddress`] | `indexing::Watch` ∪ [`Signers`]; `watch_only` is "watched, and no key held here" |
| birthday | [`Start`] — `"fresh"` or a native block position — required on all three add bodies and on each `wallets[]` entry of the server's configuration | client; converted to `indexing::Start`, which refuses one further below the checkpoint than the policy allows |
| how complete an answer is | [`Completeness`] on [`Balance`] and on [`TransactionPage`] | `indexing::Completeness`, weakened by the index while that address still owes a historical range |
| balance | [`Balance`] | `indexing::Balances::balance` of the custody itself, scaled by the `base::Unit` of the asset it is held in |
| history page | [`TransactionPage`] of [`Transaction`] ([`Status`]: [`PendingStatus`], [`ConfirmedStatus`], [`FailedStatus`]; [`Movement`], [`Fee`], [`Asset`], [`Block`]) | `indexing::TransactionPage`, projected here into display units by looking each movement's and each fee's own `base::AssetId` up in `base::Units` |
| history query | [`HistoryQuery`] (opaque cursor, limit) | client |
| send | [`Transfer`] → [`TransferAccepted`]; [`TransferBatch`] of [`BatchedTransfer`] → [`BatchAccepted`] | `wallets::Wallets::author`, then the author's own `SingleSender::send`; `wallets::Wallets::send_all`. An asset this deployment only receives has no author and no sender: both answer 400 |
| addresses | `destination` text (request), [`Address`] + [`Network`] (history, parse) | client / `base::Address` |
| synchronization | [`SyncReport`] of [`NetworkCheckpoint`] ([`IndexState`], [`IndexFailure`], [`Backfill`]); [`ResumeNetwork`] → [`ResumeAccepted`] | `indexing::SyncHealth` + `Checkpoint`; [`Resume`], implemented by the composition root over `indexing::SyncControl`. [`Backfill`] is the history a network still owes: reported, never gating, so an unservable historical range does not make the API unready |
| errors | [`ErrorBody`] on every rejection, framework rejections included | this crate |

## Public API map

| Group | Items |
|---|---|
| construct | [`Payments::from_params`], [`PaymentsParams`], [`Signers::new`], [`router`], [`openapi`], [`Resume`] |
| routes | `GET /v1/assets`, `GET /v1/assets/{asset}/address?text=...`, `POST`/`GET /v1/wallets`, `POST /v1/wallets/import`, `POST /v1/addresses`, `GET /v1/wallets/{id}`, `GET /v1/wallets/{id}/balance`, `GET`/`POST /v1/wallets/{id}/transactions`, `POST /v1/transactions`, `GET /v1/status`, `POST /v1/status/resume`; public `GET /health/live`, `GET /health/ready`, `GET /openapi.json` |
| request bodies | [`CreateWallet`], [`ImportWallet`], [`WatchAddress`] (each carrying a required [`Start`]), [`Transfer`], [`TransferBatch`], [`BatchedTransfer`], [`HistoryQuery`], [`AssetPath`], [`AddressQuery`], [`ResumeNetwork`] |
| responses | [`AssetList`], [`Listing`], [`Wallet`], [`WalletList`], [`Balance`], [`TransactionPage`], [`Transaction`], [`Status`], [`PendingStatus`], [`ConfirmedStatus`], [`FailedStatus`], [`Movement`], [`MovementKind`], [`Fee`], [`Asset`], [`Block`], [`ParentBlock`], [`Address`], [`Network`], [`TransferAccepted`], [`BatchAccepted`], [`SyncReport`], [`NetworkCheckpoint`], [`IndexState`], [`IndexFailure`], [`Backfill`], [`Completeness`], [`ResumeAccepted`], [`ErrorBody`] |
| errors | [`router`] returns `http_server::ConfigError` for an invalid server configuration; every rejection answers with [`ErrorBody`] (see `docs/API.md` §Errors) |

## Usage

The composition root does exactly this after registering every factory; the
router is then served with `axum::serve`. The composition root publishes every
network's state on the health channel, and `GET /health/ready` answers `204` only
while that snapshot is ready: every network `Ready` or `Quarantined`, and at
least one `Ready`.

```rust
use std::sync::Arc;

use http_server::{BearerToken, Config, ConfigError, RequestLimits, TransportSecurity};
use indexing::{Balances, History, Index, SyncHealth};
use payment_api::{AssetTag, Payments, PaymentsParams, Signers, router};
use tokio::sync::watch;

fn app(
    wallets: wallets::Wallets<AssetTag>,
    units: Arc<base::Units>,
    signers: Arc<Signers>,
    index: Arc<Index>,
    history: Arc<dyn History>,
    balance: Arc<dyn Balances>,
    resume: Arc<dyn payment_api::Resume>,
) -> Result<(axum::Router, watch::Sender<SyncHealth>), ConfigError> {
    let config = Config::new(
        "127.0.0.1:8080".parse().expect("valid socket address"),
        TransportSecurity::PlaintextLoopback,
        Some(BearerToken::new("replace-me")?),
        RequestLimits::default(),
    );
    let (health, snapshot) = watch::channel(SyncHealth::starting(index.networks()));
    // `wallets` and `units` are the two halves of one `wallets::Composition`:
    // what each configured asset is, and what an amount of it is. `signers`
    // holds whatever this process already has keys for; the routes add to it
    // as wallets are created, always after the index's watch row.
    // `resume` is the write side of the same health the channel reports: the
    // supervisor's own `SyncControl`, behind this crate's port so no runtime is
    // named here. It is what `POST /v1/status/resume` reaches.
    // `history` and `balance` are the two reads the index does not own: each is
    // a repository's own query joined to one network's overlay, so
    // `wallets::Composition` hands back one handle per query, routed per
    // network. `index` is what watches addresses and reports checkpoints, and no
    // query is read through it.
    let payments = Payments::from_params(PaymentsParams {
        wallets: Arc::new(wallets),
        units,
        signers,
        index,
        history,
        balance,
        resume,
        health: snapshot,
    });
    let router = router(payments, &config)?;
    Ok((router, health)) // the supervisor's monitor publishes every transition
}
```

`tests/route_contract.rs` serves this router over test doubles of
`WalletFactory`, `Wallet`, and `Indexing`: no chain is needed to exercise every
route.

## Related crates

| Layer (`linter.toml`) | Crates | Relation |
|---|---|---|
| packages | `http-server` | server hardening under [`router`] |
| base, indexing, wallets | `base`, `indexing`, `wallets` | wire models convert from their types; handlers call `wallets::Wallets` and the composed `indexing::Index` |
| server | `payment-server` | composes chains and storage, then serves [`router`] |

Nothing but `payment-server` depends on this crate. Design notes:
[`docs/API.md`](../../docs/API.md), [`ARCHITECTURE.md`](../../ARCHITECTURE.md),
[`docs/SYSTEM_REQUIREMENTS.md`](../../docs/SYSTEM_REQUIREMENTS.md).
