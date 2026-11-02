use std::sync::Arc;

use axum::{Extension, Json};
use utoipa::openapi::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::api::AppState;

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(read))
}

#[utoipa::path(
    get,
    path = "/openapi.json",
    operation_id = "read_openapi_document",
    summary = "Read this OpenAPI 3 document",
    description = "The contract every route is generated from; public, like the health routes.",
    responses(
        (status = 200, description = "OpenAPI 3 contract", body = Object),
        (status = 500, description = "The contract could not be encoded", body = ErrorBody)
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
