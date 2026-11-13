//! An ordered send from a wallet (`POST /v1/wallets/{id}/transactions`, a
//! queued job).

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
    /// The transfers, in order: 1 to [`MAX_TRANSFERS`].
    fn transfers(&self) -> Result<Vec<Transfer>, Error> {
        if self.transfers.is_empty() || self.transfers.len() > MAX_TRANSFERS {
            return Err(Error::invalid_request(format!(
                "between 1 and {MAX_TRANSFERS} transfers are allowed"
            )));
        }
        let transfers = self.transfers.iter().map(|item| Transfer {
            destination: item.destination.clone(),
            amount: item.amount.clone(),
        });
        Ok(transfers.collect())
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
    let (transfers, id) = (request.transfers()?, WalletId::parse(&path.id)?);
    let (container, db, queue) = (&state.container, &state.db, &state.queue);
    let job = wallets::send(container, db, queue, &id, &request.asset, transfers).await?;
    let job = job.to_string();
    Ok((StatusCode::ACCEPTED, Json(SendResponse { job })))
}
