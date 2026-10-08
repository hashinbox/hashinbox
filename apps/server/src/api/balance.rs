//! `GET /v1/wallets/{id}/balance`: a wallet's balance in one asset.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use utoipa::IntoParams;

use super::{Error, ErrorBody};
use crate::{asset::AssetTag, models::Wallet, state::AppState};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(balance))
}

/// A wallet's balance in its asset, on both sides of its chain's confirmed
/// line.
///
/// `amount` is the confirmed indexed total, negative when indexed debits
/// exceed known credits; `behind` states how far the address's inspection
/// trails the confirmed line. Chain spending rules, such as coinbase
/// maturity, still apply.
/// The two pending figures describe blocks above the confirmed line: they are
/// reported apart rather than netted, because "3.0 arriving" and "1.0
/// leaving" are different facts to a payments operator, and neither is ever
/// negative. Pending fees are not folded in; a pending transaction's fee is
/// reported with it in history.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = Balance)]
struct ReadResponse {
    /// Confirmed indexed amount in exact display units, e.g. `12.5`; negative,
    /// e.g. `-0.5`, when indexed debits exceed known credits. Chain spending
    /// rules apply separately.
    amount: String,
    /// Incoming value in blocks above the confirmed line. Never negative.
    pending_incoming: String,
    /// Outgoing value in blocks above the confirmed line. Never negative.
    pending_outgoing: String,
    /// Height of the confirmed line for the saved total; null before a
    /// checkpoint. Always present.
    #[schema(required)]
    observed_height: Option<u64>,
    /// Height of the newest block above the confirmed line; null when nothing
    /// is pending. Always present.
    #[schema(required)]
    pending_height: Option<u64>,
    /// How many blocks this address's inspection is behind the confirmed
    /// line; 0 when caught up. While it is above 0, `amount` may still change
    /// as history below the line is read.
    behind: u64,
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}/balance",
    operation_id = "read_wallet_balance",
    summary = "Read a wallet's balance in one asset",
    description = "The confirmed indexed total and pending incoming/outgoing amounts in the \
        asset's display units, with the confirmed and pending heights and how many blocks the \
        address's inspection is behind. \
        Chain spending rules still apply. A watch-only address reads the same balance \
        without a key. An asset the wallet is not observed in reads what was indexed for its \
        address.",
    params(WalletPath, AssetQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The wallet's balance", body = ReadResponse),
        (status = 400, description = "Malformed query", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown, or the asset is not of its network",
            body = ErrorBody),
        (status = 503, description = "The index is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn balance(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
    query: Result<Query<AssetQuery>, QueryRejection>,
) -> Result<Json<ReadResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let wallet = Wallet::find(&state.payments, &path.id).await?;
    let subject = &wallet.subject(&state.payments.wallets, &query.asset)?;
    let unit = state
        .payments
        .units
        .of(&subject.address.network, &subject.asset)
        .ok_or_else(Error::undeclared)?;
    // The pending blocks are sampled before the rows are read, and narrowed
    // by the line the rows were read at, so a commit landing in between is
    // counted once.
    let pending = state
        .payments
        .pending
        .iter()
        .find(|(network, _)| *network == subject.address.network)
        .map(|(_, pending)| pending.clone())
        .ok_or_else(|| {
            indexing::IndexError::new(
                indexing::IndexErrorKind::NetworkMismatch,
                "index network is not configured",
                false,
            )
        })?;
    let sample = pending.sampled()?;
    // One snapshot: the repository's own read.
    let balance = state
        .repo
        .balance(subject)
        .await?
        .completed(subject, sample);
    // Every indexed amount is exact and atomic; the asset's own decimals,
    // which the index never learns, are applied here.
    let display = |amount| unit.display(amount).map_err(Error::unscalable);
    // A deficit is shown as such: the magnitude is scaled, the sign kept.
    let (sign, units) = balance.net.into_parts();
    let units = base::Decimal::from_atomic(units, 0);
    let magnitude = display(&units)?;
    let amount = match sign {
        num_bigint::Sign::Minus => format!("-{magnitude}"),
        _ => magnitude.to_string(),
    };
    Ok(Json(ReadResponse {
        amount,
        pending_incoming: display(&balance.incoming)?.to_string(),
        pending_outgoing: display(&balance.outgoing)?.to_string(),
        observed_height: balance.observed_at.map(|block| block.height.0),
        pending_height: balance.pending.map(|block| block.height.0),
        behind: balance.behind,
    }))
}

/// The wallet whose balance is read.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
struct WalletPath {
    /// The wallet ID a create or watch call returned.
    id: String,
}

/// The asset a balance is read in.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
struct AssetQuery {
    /// A configured asset tag of the wallet's network (`GET /v1/assets`).
    #[param(value_type = String, pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: AssetTag,
}
