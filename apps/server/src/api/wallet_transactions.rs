//! A send from a wallet (`POST /v1/wallets/{id}/transactions`, one payment,
//! a queued job).

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{
    api::AppState,
    models::wallet::WalletId,
    usecase::wallets::{self, Transfer},
};

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

/// `POST /v1/wallets/{id}/transactions` body: one payment from the path's
/// wallet in one asset.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = Send)]
#[serde(deny_unknown_fields)]
struct SendRequest {
    /// An asset id of the wallet's network; default `native`.
    #[serde(default = "native")]
    asset: String,
    /// Recipient address text, as the wallet's chain writes it.
    destination: String,
    /// Exact positive decimal in display units.
    amount: String,
    /// How fast it should land; each chain prices it its own way. Default
    /// `normal`.
    #[serde(default)]
    fee: Fee,
}

/// How fast a payment should land: Ethereum tips at half, the same or twice
/// the node's suggestion; Bitcoin targets confirmation within 12 blocks, the
/// network's target or the next block; Solana pays no priority fee yet.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = FeeLevel)]
enum Fee {
    Low,
    #[default]
    Normal,
    High,
}

impl From<Fee> for base::FeeLevel {
    fn from(fee: Fee) -> Self {
        match fee {
            Fee::Low => Self::Low,
            Fee::Normal => Self::Normal,
            Fee::High => Self::High,
        }
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
    operation_id = "send_payment",
    summary = "Enqueue a payment from a wallet",
    description = "Enqueues one send job of `asset` from the keyed wallet: one payment, signed \
        as one transaction and broadcast. Several payments are several requests; a wallet's \
        sends run one at a time, in order, and each fails on its own. A 202 response means \
        queued, not submitted: `GET /v1/jobs/{id}` reports the outcome. Indexing records the \
        transaction once it is mined, for the assets the wallet is watched in. A watch-only \
        wallet answers 422.",
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
    let id = WalletId::parse(&path.id)?;
    let transfer = Transfer {
        destination: request.destination,
        amount: request.amount,
        fee: request.fee.into(),
    };
    let (container, db, queue) = (&state.container, &state.db, &state.queue);
    let job = wallets::send(container, db, queue, &id, &request.asset, transfer).await?;
    let job = job.to_string();
    Ok((StatusCode::ACCEPTED, Json(SendResponse { job })))
}
