use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    Payments,
    asset::AssetTag,
    contract::{Completeness, Wallet, WalletPath},
    error::{Error, ErrorBody},
};

pub(super) fn routes() -> OpenApiRouter<super::Application> {
    OpenApiRouter::new()
        .routes(routes!(create, list))
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
}

#[utoipa::path(
    post,
    path = "/v1/wallets",
    operation_id = "create_wallet",
    summary = "Generate a wallet for one configured asset",
    description = "Generates a wallet, stores its key in the application registry, then \
        watches it from creation. Keys never leave the server. Retrying creates another wallet.",
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
    State(payments): State<Arc<Payments>>,
    request: Result<Json<CreateWallet>, JsonRejection>,
) -> Result<(StatusCode, Json<Wallet>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let _admission = payments.asset_admission(&request.asset)?;
    let material = payments
        .wallets
        .secret(&request.asset)
        .await
        .map_err(Error::by_asset)?;
    let wallet = payments
        .wallets
        .from(
            &request.asset,
            wallets::SecretBytes::new(material.as_bytes()),
        )
        .await
        .map_err(Error::by_asset)?;
    payments.registered(wallet, request.asset, material).await
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
async fn list(State(payments): State<Arc<Payments>>) -> Result<Json<WalletList>, Error> {
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
    State(payments): State<Arc<Payments>>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Wallet>, Error> {
    Ok(Json(payments.custody(&path.id).await?.into()))
}

/// A wallet's balance in its asset, on both sides of its chain's confirmed
/// line.
///
/// `amount` is the confirmed indexed total. Completeness states whether history
/// is missing; chain spending rules, such as coinbase maturity, still apply.
/// The two pending figures describe blocks above the confirmed line: they are
/// reported apart rather than netted, because "3.0 arriving" and "1.0
/// leaving" are different facts to a payments operator, and neither is ever
/// negative. Pending fees are not folded in; a pending transaction's fee is
/// reported with it in history.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(crate) struct Balance {
    /// Confirmed indexed amount in exact display units, e.g. `12.5`.
    /// Completeness and chain spending rules apply separately.
    pub(super) amount: String,
    /// Incoming value in blocks above the confirmed line. Never negative.
    pub(super) pending_incoming: String,
    /// Outgoing value in blocks above the confirmed line. Never negative.
    pub(super) pending_outgoing: String,
    /// Height of the confirmed line for the saved total; null before a
    /// checkpoint. Always present.
    #[schema(required)]
    pub(super) observed_height: Option<u64>,
    /// Height of the newest block above the confirmed line; null when nothing
    /// is pending. Always present.
    #[schema(required)]
    pub(super) pending_height: Option<u64>,
    /// How much of the history behind `amount` the index can prove it holds.
    /// `incomplete` while this address still owes a historical range, and
    /// `overdrawn` when indexed debits exceed known credits.
    pub(super) completeness: Completeness,
}

impl Balance {
    /// One indexed balance in `unit`, the unit the asset's own chain declared.
    ///
    /// Every amount the index reports is exact and atomic, so the asset's own
    /// decimals — which the index never learns — are applied here.
    ///
    /// # Errors
    ///
    /// A 503 when a stored amount is not a non-negative integer.
    pub(crate) fn new(balance: indexing::Balance, unit: &base::Unit) -> Result<Self, Error> {
        let display = |amount| unit.display(amount).map_err(Error::unscalable);
        let completeness = match balance.completeness {
            indexing::Completeness::Incomplete if balance.opening.is_some() => {
                Completeness::VerifiedOpeningBalance
            }
            other => other.into(),
        };
        Ok(Self {
            amount: display(&balance.amount)?.to_string(),
            pending_incoming: display(&balance.pending.incoming)?.to_string(),
            pending_outgoing: display(&balance.pending.outgoing)?.to_string(),
            observed_height: balance.observed_at.map(|block| block.height.0),
            pending_height: balance.pending.block.map(|block| block.height.0),
            completeness,
        })
    }
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}/balance",
    operation_id = "read_wallet_balance",
    summary = "Read a wallet's balance in its own asset",
    description = "The confirmed indexed total and pending incoming/outgoing amounts in the \
        asset's display units, with the confirmed and pending heights and history completeness. \
        Chain spending rules still apply. A watch-only address reads the same balance \
        without a key.",
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
    State(payments): State<Arc<Payments>>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Balance>, Error> {
    let custody = payments.custody(&path.id).await?;
    let _admission = payments.admission(&custody.watch.address.network)?;
    Ok(Json(payments.balance(&custody).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_evidence_only_upgrades_an_incomplete_balance() {
        let network = base::Network::new(base::ChainName::new("fixture"), "local");
        let query = indexing::BalanceQuery {
            address: base::Address::new(network.clone(), "holder"),
            asset: base::AssetId::native(network.chain),
        };
        let mut answer = indexing::Balance {
            amount: 7_u64.into(),
            observed_at: Some(base::BlockRef {
                position: base::BlockPosition(8),
                height: base::BlockHeight(8),
                hash: base::BlockHash(vec![8]),
                parent: None,
                timestamp: None,
            }),
            pending: indexing::PendingValue::none(),
            completeness: indexing::Completeness::Incomplete,
            opening: None,
        };
        let evidence = indexing::BalanceEvidence::capture(&query, &answer).expect("opening");
        for (coverage, expected) in [
            (
                indexing::Completeness::Incomplete,
                Completeness::VerifiedOpeningBalance,
            ),
            (
                indexing::Completeness::CompleteHistory,
                Completeness::CompleteHistory,
            ),
            (indexing::Completeness::Overdrawn, Completeness::Overdrawn),
        ] {
            answer.completeness = coverage;
            for witnessed in [false, true] {
                answer.opening = witnessed.then(|| evidence.clone());
                let wire = Balance::new(answer.clone(), &base::Unit::token(0)).expect("projection");
                assert_eq!(
                    wire.completeness,
                    if witnessed { expected } else { coverage.into() }
                );
                assert_eq!(wire.amount, "7", "evidence never changes the amount");
            }
            assert_ne!(
                Completeness::from(coverage),
                Completeness::VerifiedOpeningBalance,
                "history coverage never claims balance evidence"
            );
        }
    }
}
