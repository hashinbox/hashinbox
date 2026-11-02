//! `GET /v1/wallets/{id}/balance`: a wallet's balance in one asset.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use base::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, asset_info, wallet_address};
use crate::{
    models::{Asset, wallet_index},
    state::AppState,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(balance))
}

/// A wallet's balance in one asset over every block kept so far.
///
/// Blocks are kept up to the chain's produced tip, so `amount` includes
/// blocks the chain may still take back; a reorg archives them and the
/// amount follows.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = Balance)]
struct ReadResponse {
    /// The asset.
    asset: Asset,
    /// Exact display units, e.g. `12.5`; negative, e.g. `-0.5`, when indexed
    /// debits exceed indexed credits (history before the start is unknown).
    amount: String,
    /// Native position the address is indexed through in this asset; null
    /// before its first block or when it is not watched in it. Always present.
    #[schema(required)]
    indexed_through: Option<u64>,
    /// The chain's produced tip at the indexer's last step; null before the
    /// first. Always present.
    #[schema(required)]
    tip: Option<u64>,
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}/balance",
    operation_id = "read_wallet_balance",
    summary = "Read a wallet's balance in one asset",
    description = "The total of every indexed movement of the address in `asset` over every \
        kept block (unconfirmed ones included), in display units, with how far the address \
        is indexed and the chain's tip. A watch-only address reads the same way.",
    params(WalletPath, AssetQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The wallet's balance", body = ReadResponse),
        (status = 400, description = "Malformed wallet ID or query", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network or asset is not configured", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn balance(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
    query: Result<Query<AssetQuery>, QueryRejection>,
) -> Result<Json<ReadResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let address = wallet_address(&state.payments, &path.id)?;
    let info = asset_info(&state.payments, &address.network, &query.asset)?;
    let total = state.storage.balance(&address, &info.asset).await?;
    let network = address.network.clone();
    let indexed_through = state
        .storage
        .read(move |connection| wallet_index::db::find_all(connection, &network))
        .await?
        .into_iter()
        .find(|index| index.address == address && index.asset == info.asset)
        .and_then(|index| index.indexed_at)
        .map(|header| header.position);
    let tip = state
        .payments
        .service(&address.network)?
        .indexer
        .stats()
        .tip;
    Ok(Json(ReadResponse {
        asset: Asset::from(&info),
        amount: amount(&info, total)?,
        indexed_through,
        tip,
    }))
}

/// Atomic `total` of `info` in exact display units; a deficit is shown as
/// such: the magnitude is scaled, the sign kept.
///
/// # Errors
///
/// A 503 when `total` is not whole atomic units.
pub(super) fn amount(info: &base::AssetInfo, total: Decimal) -> Result<String, Error> {
    let unscalable = |error: base::DecimalError| {
        Error::unavailable(format!("indexed amount is not whole atomic units: {error}"))
    };
    Ok(if total < Decimal::zero() {
        let magnitude = info
            .display(&(Decimal::zero() - total))
            .map_err(unscalable)?;
        format!("-{magnitude}")
    } else {
        info.display(&total).map_err(unscalable)?.to_string()
    })
}

/// The wallet whose balance is read.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
struct WalletPath {
    /// `<chain>:<network>:<address>`.
    id: String,
}

/// The asset a balance is read in.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
struct AssetQuery {
    /// An asset id of the wallet's network (`GET /v1/assets`); default
    /// `native`.
    #[serde(default = "native")]
    #[param(default = "native")]
    asset: String,
}

fn native() -> String {
    "native".to_owned()
}
