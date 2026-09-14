use axum::{extract::State, http::StatusCode};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::State as HttpState;

pub fn routes() -> OpenApiRouter<HttpState> {
    OpenApiRouter::new()
        .routes(routes!(live))
        .routes(routes!(ready))
}

// named Utoipa liveness endpoint binds its GET route and OpenAPI response contract through
// routes!(live)
// design-lint: allow single-use-free-function -- Utoipa requires the named liveness handler
#[utoipa::path(
    get,
    path = "/health/live",
    responses((status = 204, description = "Process is alive")),
    tag = "health"
)]
async fn live() -> StatusCode {
    StatusCode::NO_CONTENT
}

// Axum owns the state extractor signature; this health handler maps an open ready channel to 204
// and unavailable or closed state to 503
// design-lint: allow unclassified-free-function -- Axum owns the state extractor signature
#[utoipa::path(
    get,
    path = "/health/ready",
    responses(
        (status = 204, description = "Wallets and indexes are ready"),
        (status = 503, description = "Wallets or indexes are not ready")
    ),
    tag = "health"
)]
async fn ready(State(state): State<HttpState>) -> StatusCode {
    if state.readiness.has_changed().is_ok() && *state.readiness.borrow() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}
