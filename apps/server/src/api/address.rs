use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    Payments,
    asset::AssetTag,
    contract::Wallet,
    error::{Error, ErrorBody},
};

pub(super) fn routes() -> OpenApiRouter<Payments> {
    OpenApiRouter::new().routes(routes!(watch))
}

/// `POST /v1/addresses` body: observe an address without its key.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct WatchAddress {
    /// The configured asset tag whose history to record.
    #[schema(value_type = String, pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: AssetTag,
    /// Address text in any form the asset's chain accepts.
    address: String,
}

#[utoipa::path(
    post,
    path = "/v1/addresses",
    operation_id = "watch_address",
    summary = "Observe an address without its key",
    description = "Observes an address from registration without its key. The entry is \
        watch-only; sending from it answers 422.",
    request_body = WatchAddress,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "Address registered for indexing", body = Wallet),
        (status = 400, description = "Malformed body, asset tag, or address text",
            body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Asset is not configured", body = ErrorBody),
        (status = 409, description = "The address is already registered", body = ErrorBody),
        (status = 503, description = "Address registration is unavailable", body = ErrorBody)
    ),
    tag = "addresses"
)]
async fn watch(
    State(payments): State<Payments>,
    request: Result<Json<WatchAddress>, JsonRejection>,
) -> Result<(StatusCode, Json<Wallet>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    payments.observed(request.asset, &request.address).await
}
