use std::fmt;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use zeroize::Zeroize;

use super::{
    Payments,
    asset::AssetTag,
    contract::{Completeness, Start, Wallet, WalletPath},
    error::{Error, ErrorBody},
};

pub(super) fn routes() -> OpenApiRouter<Payments> {
    OpenApiRouter::new()
        .routes(routes!(create, list))
        .routes(routes!(import))
        .routes(routes!(read))
        .routes(routes!(balance))
}

/// `POST /v1/wallets` body: generate a wallet for one configured asset.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateWallet {
    /// The configured asset tag the new wallet holds (`GET /v1/assets`).
    #[schema(value_type = String, pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: AssetTag,
    /// Where to begin observing the new address. A generated key has no past,
    /// so `"fresh"` is the honest answer; a position is accepted because the
    /// same body registers a key an operator may have used elsewhere first.
    start: Start,
}

#[utoipa::path(
    post,
    path = "/v1/wallets",
    operation_id = "create_wallet",
    summary = "Generate a wallet for one configured asset",
    description = "Generates a wallet of one configured asset. The key never leaves the server; \
        it is persisted unencrypted in the storage registry so the wallet survives a restart \
        (development custody, not an HSM or KMS). `start` states the birthday to index from: \
        `\"fresh\"` for a key with no past, or a native block position, whose earlier history \
        is then backfilled. The call is \
        not idempotent: a retried request mints a second wallet and a second key.",
    request_body = CreateWallet,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "Wallet generated and registered for indexing", body = Wallet),
        (status = 400, description = "Malformed body or asset tag", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Asset is not configured", body = ErrorBody),
        (status = 409, description = "Wallet ID or address is already registered",
            body = ErrorBody),
        (status = 503, description = "Wallet generation or registration is unavailable",
            body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn create(
    State(payments): State<Payments>,
    request: Result<Json<CreateWallet>, JsonRejection>,
) -> Result<(StatusCode, Json<Wallet>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let wallet = payments
        .wallets
        .create(&request.asset)
        .await
        .map_err(Error::by_asset)?;
    payments
        .registered(wallet, request.asset, request.start)
        .await
}

/// Secret key text in its chain's textual form; redacted in `Debug` and
/// zeroized on drop. It is never serialized.
#[derive(Deserialize)]
#[serde(transparent)]
struct SecretText(String);

impl fmt::Debug for SecretText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretText(<redacted>)")
    }
}

impl Drop for SecretText {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// `POST /v1/wallets/import` body: register a wallet from a caller-held key.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct ImportWallet {
    /// The configured asset tag the wallet holds.
    #[schema(value_type = String, pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: AssetTag,
    /// The private key in the textual form the asset's chain defines
    /// (`docs/API.md` lists the built-in chains' forms).
    #[schema(value_type = String, format = Password)]
    secret: SecretText,
    /// Where to begin observing the imported address: a native block position
    /// for a key with history, or `"fresh"` for one that has never been used.
    start: Start,
}

#[utoipa::path(
    post,
    path = "/v1/wallets/import",
    operation_id = "import_wallet",
    summary = "Register a wallet from a caller-held private key",
    description = "Registers a wallet from a private key. The asset's chain parses the key text; \
        the wallet is indexed from `start` — a native block position, whose history below the \
        checkpoint is backfilled, or `\"fresh\"` for the block after the current checkpoint — \
        and persisted unencrypted in the storage registry (development custody, not an HSM or \
        KMS). A position further below the checkpoint than the index's policy allows answers \
        400 naming the earliest acceptable start.",
    request_body = ImportWallet,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "Wallet imported and registered for indexing", body = Wallet),
        (status = 400, description = "Malformed body, asset tag, or key text", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Asset is not configured", body = ErrorBody),
        (status = 409, description = "Wallet ID or address is already registered",
            body = ErrorBody),
        (status = 503, description = "Wallet registration is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn import(
    State(payments): State<Payments>,
    request: Result<Json<ImportWallet>, JsonRejection>,
) -> Result<(StatusCode, Json<Wallet>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let secret =
        wallets::SecretBytes::from_hex(&request.secret.0).map_err(|_| Error::invalid_secret())?;
    let wallet = payments
        .wallets
        .from(&request.asset, secret)
        .await
        .map_err(Error::by_asset)?;
    payments
        .registered(wallet, request.asset, request.start)
        .await
}

/// `GET /v1/wallets`: every wallet and watched address.
// linter:disable rust/redundant-module-prefix -- Utoipa publishes the schema under this name.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct WalletList {
    /// Wallets in ID order.
    wallets: Vec<Wallet>,
}

#[utoipa::path(
    get,
    path = "/v1/wallets",
    operation_id = "list_wallets",
    summary = "List every wallet and watched address",
    description = "Every registered wallet and watched address in wallet ID order.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Wallets in ID order", body = WalletList),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The wallet collection is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn list(State(payments): State<Payments>) -> Result<Json<WalletList>, Error> {
    let wallets = payments
        .custodies()
        .await?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(Json(WalletList { wallets }))
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}",
    operation_id = "read_wallet",
    summary = "Read one wallet's public metadata",
    description = "The wallet's asset, network, canonical address, and watch-only flag; never \
        its key.",
    params(WalletPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The wallet's public metadata", body = Wallet),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn read(
    State(payments): State<Payments>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Wallet>, Error> {
    Ok(Json(payments.custody(&path.id).await?.into()))
}

/// A wallet's balance in its asset, on both sides of its chain's confirmed
/// line.
///
/// `amount` is settled and spendable and cannot revert: it is the index's
/// fold over the movements it stores at or below the confirmed line. The two
/// pending figures are what is still in blocks above that line: they are
/// reported apart rather than netted, because "3.0 arriving" and "1.0
/// leaving" are different facts to a payments operator, and neither is ever
/// negative. Pending fees are not folded in; a pending transaction's fee is
/// reported with it in history.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(crate) struct Balance {
    /// Confirmed and spendable: exact decimal in display units, e.g. `12.5`.
    /// Folded over durable rows only, so it cannot revert.
    pub(super) amount: String,
    /// Incoming value in blocks above the confirmed line. Never negative.
    pub(super) pending_incoming: String,
    /// Outgoing value in blocks above the confirmed line. Never negative.
    pub(super) pending_outgoing: String,
    /// Height of the confirmed line `amount` was folded at; null before a
    /// checkpoint. Always present.
    #[schema(required)]
    pub(super) observed_height: Option<u64>,
    /// Height of the newest block above the confirmed line; null when nothing
    /// is pending. Always present.
    #[schema(required)]
    pub(super) pending_height: Option<u64>,
    /// How much of the history behind `amount` the index can prove it holds.
    /// `incomplete` while this address still owes a historical range, and
    /// `overdrawn` when the fold proves credits are missing.
    pub(super) completeness: Completeness,
}

impl Balance {
    /// One folded balance in `unit`, the unit the asset's own chain declared.
    ///
    /// Every amount the index reports is exact and atomic, so the asset's own
    /// decimals — which the index never learns — are applied here.
    ///
    /// # Errors
    ///
    /// A 503 when a stored amount is not a non-negative integer.
    pub(crate) fn new(balance: indexing::Balance, unit: &base::Unit) -> Result<Self, Error> {
        let display = |amount| unit.display(amount).map_err(Error::unscalable);
        Ok(Self {
            amount: display(&balance.amount)?.to_string(),
            pending_incoming: display(&balance.pending.incoming)?.to_string(),
            pending_outgoing: display(&balance.pending.outgoing)?.to_string(),
            observed_height: balance.observed_at.map(|block| block.height.0),
            pending_height: balance.pending.block.map(|block| block.height.0),
            completeness: balance.completeness.into(),
        })
    }
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}/balance",
    operation_id = "read_wallet_balance",
    summary = "Read a wallet's balance in its own asset",
    description = "The balance of the wallet's address in its asset's display units, folded by \
        the index over the movements it stores: the height of the confirmed line it was folded \
        at, what is still arriving and leaving in blocks above that line, and how much of the \
        history behind the amount the index can prove it holds. A watch-only address answers it \
        exactly as a wallet does, because the fold needs no key.",
    params(WalletPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The wallet's balance", body = Balance),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody),
        (status = 503, description = "The index is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn balance(
    State(payments): State<Payments>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Balance>, Error> {
    let custody = payments.custody(&path.id).await?;
    Ok(Json(payments.balance(&custody).await?))
}
