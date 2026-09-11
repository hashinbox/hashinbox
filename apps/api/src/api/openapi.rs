use std::sync::Arc;

use axum::{Extension, Json};
use utoipa::openapi::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::State;
use super::error::{Body, Error};

pub fn routes() -> OpenApiRouter<State> {
    OpenApiRouter::new().routes(routes!(read))
}

// Axum owns the extension extractor signature; this handler serializes the injected OpenAPI
// contract and maps encoding errors to HTTP
// design-lint: allow unclassified-free-function -- Axum owns the extension extractor signature
#[utoipa::path(
    get,
    path = "/openapi.json",
    responses(
        (status = 200, description = "OpenAPI 3 contract", body = Object),
        (status = 500, body = Body)
    ),
    tag = "openapi"
)]
async fn read(
    Extension(contract): Extension<Arc<OpenApi>>,
) -> Result<Json<serde_json::Value>, Error> {
    serde_json::to_value(contract.as_ref())
        .map(Json)
        .map_err(|_| Error::invalid_response("OpenAPI contract could not be encoded"))
}
