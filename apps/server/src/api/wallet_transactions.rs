//! An ordered send from a wallet (`POST /v1/wallets/{id}/transactions`, a
//! queued job).

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use base::Decimal;
use serde::{Deserialize, Serialize};
use tasks::Worker as _;
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, asset_info, wallet_address};
use crate::{
    api::AppState,
    usecase::workers::broadcast::{Broadcast, Payment, SendIntent},
};

/// Maximum transfers one send request accepts.
const MAX_TRANSFERS: usize = 50;

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(send))
}

/// The wallet a send is from.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
struct WalletPath {
    /// `{chain}.{network}.{address}`.
    id: String,
}

/// The default asset.
fn native() -> String {
    "native".to_owned()
}

/// One ordered transfer of a send.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct TransferItem {
    /// Recipient address text, as the wallet's chain writes it.
    destination: String,
    /// Exact positive decimal in display units.
    amount: String,
}

/// `POST /v1/wallets/{id}/transactions` body: an ordered send from the
/// path's wallet in one asset.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = Send)]
#[serde(deny_unknown_fields)]
struct SendRequest {
    /// An asset id of the wallet's network; default `native`.
    #[serde(default = "native")]
    asset: String,
    /// The transfers, in execution order.
    #[schema(min_items = 1, max_items = 50)]
    transfers: Vec<TransferItem>,
}

impl SendRequest {
    /// The send from `from`: each transfer's amount in `info`'s atomic units
    /// and its destination parsed by the wallet's chain; a failure names its
    /// transfer.
    fn intent(
        self,
        payments: &payments::Payments,
        from: &base::Address,
        info: &base::AssetInfo,
    ) -> Result<SendIntent, Error> {
        let mut transfers = Vec::with_capacity(self.transfers.len());
        for (index, item) in self.transfers.into_iter().enumerate() {
            let at = |error: Error| error.at(index);
            let units = item
                .amount
                .parse::<Decimal>()
                .ok()
                .filter(|amount| amount > &Decimal::zero())
                .and_then(|amount| info.atomic(&amount).ok())
                .ok_or_else(|| {
                    at(Error::invalid_request(
                        "amount must be a positive exact decimal within the asset's decimals",
                    ))
                })?;
            let to = payments
                .parse(&from.network, &item.destination)
                .map_err(|error| at(error.into()))?;
            transfers.push(Payment {
                to: to.value,
                amount: units.to_string(),
            });
        }
        Ok(SendIntent {
            chain: from.network.chain.to_string(),
            network: from.network.name.clone(),
            address: from.value.clone(),
            asset: info.asset.id.clone(),
            transfers,
        })
    }
}

/// An enqueued send.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = SendAccepted)]
struct SendResponse {
    /// The send job; `GET /v1/jobs/{id}` reports it.
    job: String,
}

#[utoipa::path(
    post,
    path = "/v1/wallets/{id}/transactions",
    operation_id = "send_transfers",
    summary = "Enqueue an ordered send from a wallet",
    description = "Enqueues one send job of `asset` from the keyed wallet: its transfers are \
        signed up front and broadcast in order. A 202 response means queued, not submitted: \
        `GET /v1/jobs/{id}` reports the outcome. Indexing records the transactions once they \
        are mined, for the assets the wallet is watched in. A watch-only wallet answers 422.",
    params(WalletPath),
    request_body = SendRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 202, description = "Send queued", body = SendResponse),
        (status = 400, description = "Malformed body, wallet ID, amount, or destination",
            body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network or asset is not configured", body = ErrorBody),
        (status = 422, description = "A watch-only or unknown wallet", body = ErrorBody),
        (status = 503, description = "The queue or store is unavailable", body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn send(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
    request: Result<Json<SendRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<SendResponse>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    if request.transfers.is_empty() || request.transfers.len() > MAX_TRANSFERS {
        return Err(Error::invalid_request(format!(
            "between 1 and {MAX_TRANSFERS} transfers are allowed"
        )));
    }
    let address = wallet_address(&state.payments, &path.id)?;
    let info = asset_info(&state.payments, &address.network, &request.asset)?;
    let id = crate::models::Wallet::id(&address)
        .map_err(|error| Error::invalid_request(format!("{error:#}")))?;
    if !state
        .db
        .read(move |connection| Ok(crate::models::wallet::db::is_keyed(connection, &id)?))
        .await?
    {
        return Err(Error::unprocessable("the wallet holds no key here"));
    }
    let intent = request.intent(&state.payments, &address, &info)?;
    let queued = Broadcast::enqueue(&state.queue, &intent)
        .await
        .map_err(|error| match error {
            tasks::StoreError::Invalid(reason) => Error::invalid_request(reason),
            error => Error::unavailable(error.to_string()),
        })?;
    // A send is not unique: it is always queued.
    let (tasks::Admission::Queued(id) | tasks::Admission::Duplicate(id)) = queued;
    let job = id.to_string();
    Ok((StatusCode::ACCEPTED, Json(SendResponse { job })))
}
