//! `GET /v1/wallets/{id}/balance`: a wallet's balance in its own asset.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use utoipa::IntoParams;

use super::{Error, ErrorBody};
use crate::{
    models::{Completeness, Wallet},
    state::AppState,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(balance))
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
#[schema(as = Balance)]
struct ReadResponse {
    /// Confirmed indexed amount in exact display units, e.g. `12.5`.
    /// Completeness and chain spending rules apply separately.
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
    /// How much of the history behind `amount` the index can prove it holds.
    /// `incomplete` while this address still owes a historical range, and
    /// `overdrawn` when indexed debits exceed known credits.
    completeness: Completeness,
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
        (status = 200, description = "The wallet's balance", body = ReadResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody),
        (status = 503, description = "The index is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn balance(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
) -> Result<Json<ReadResponse>, Error> {
    let wallet = Wallet::find(state.registry.as_ref(), &state.wallets, &path.id).await?;
    let subject = wallet.subject()?;
    let unit = state
        .units
        .of(&subject.address.network, &subject.asset)
        .ok_or_else(Error::undeclared)?;
    // The reader that owns the wallet's network answers in atomic units;
    // the unit the asset's own chain declared scales them.
    let balance = state.balance.balance(subject).await?;
    // Every indexed amount is exact and atomic; the asset's own decimals,
    // which the index never learns, are applied here.
    let display = |amount| unit.display(amount).map_err(Error::unscalable);
    Ok(Json(ReadResponse {
        amount: display(&balance.amount)?.to_string(),
        pending_incoming: display(&balance.pending.incoming)?.to_string(),
        pending_outgoing: display(&balance.pending.outgoing)?.to_string(),
        observed_height: balance.observed_at.map(|block| block.height.0),
        pending_height: balance.pending.block.map(|block| block.height.0),
        completeness: balance.completeness.into(),
    }))
}

/// The wallet whose balance is read.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
struct WalletPath {
    /// The wallet ID a create or watch call returned.
    id: String,
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use super::*;
    use crate::api::emits_required;
    use crate::api::harness::App;

    #[test]
    fn the_balance_requires_exactly_what_it_emits() {
        emits_required(
            "Balance",
            ReadResponse {
                amount: "1.5".to_owned(),
                pending_incoming: "0.25".to_owned(),
                pending_outgoing: "0".to_owned(),
                observed_height: None,
                pending_height: None,
                completeness: Completeness::Incomplete,
            },
        );
    }

    #[tokio::test]
    async fn the_balance_is_the_index_fold_in_the_assets_units() {
        let app = App::ready().await;
        let id = app.wallet_id().await;
        app.mine().await;

        let balance = app
            .call("GET", &format!("/v1/wallets/{id}/balance"), None, true)
            .await;

        assert_eq!(balance.status, StatusCode::OK);
        assert_eq!(
            balance.json(),
            json!({
                "amount": "1.5",
                "pending_incoming": "0.25",
                "pending_outgoing": "0",
                "observed_height": 8,
                "pending_height": 9,
                "completeness": "incomplete"
            }),
            "both halves are the index's own fold, scaled by the asset's decimals, \
             and the fold says how much of the history behind it it can prove"
        );
    }
}
