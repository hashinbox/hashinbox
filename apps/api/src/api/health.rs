use axum::{extract::State, http::StatusCode};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::Payments;

pub fn routes() -> OpenApiRouter<Payments> {
    OpenApiRouter::new()
        .routes(routes!(live))
        .routes(routes!(ready))
}

#[utoipa::path(
    get,
    path = http_server::LIVENESS_PATH,
    operation_id = "read_liveness",
    summary = "Liveness probe",
    description = "Answers 204 while the process runs; public, no bearer token.",
    responses((status = 204, description = "Process is alive")),
    tag = "health"
)]
async fn live() -> StatusCode {
    StatusCode::NO_CONTENT
}

#[utoipa::path(
    get,
    path = http_server::READINESS_PATH,
    operation_id = "read_readiness",
    summary = "Readiness probe",
    description = "Answers 204 only while every configured index is ready; public, no bearer \
        token.",
    responses(
        (status = 204, description = "Wallets and indexes are ready"),
        (status = 503, description = "Wallets or indexes are not ready")
    ),
    tag = "health"
)]
async fn ready(State(payments): State<Payments>) -> StatusCode {
    if payments.ready() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}
