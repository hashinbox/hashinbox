//! `/v1/networks/{chain}/{network}/tip`: a network's newest block, or the
//! block it had at a given time, read from its node.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::api::AppState;

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(read))
}

/// The network read.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct TipPath {
    /// Chain name, as `GET /v1/networks` lists it.
    chain: String,
    /// Network name, as `GET /v1/networks` lists it.
    network: String,
}

/// When.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct TipQuery {
    /// RFC 3339 time (`2026-10-01T00:00:00Z`) or Unix seconds: answer the
    /// last block at or before it. Absent: the newest block.
    at: Option<String>,
}

/// `GET /v1/networks/{chain}/{network}/tip`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = ChainTip)]
struct TipResponse {
    /// Native position (height or slot).
    position: u64,
    /// Unix seconds, when reported; always present.
    #[schema(required)]
    timestamp: Option<u64>,
}

#[utoipa::path(
    get,
    path = "/v1/networks/{chain}/{network}/tip",
    operation_id = "read_tip",
    summary = "Read a network's newest block, or its last block at or before a time",
    description = "Read from the node. With `at`, a binary search over block times: \
        about log2(tip) block reads, and on Bitcoin, whose block times only roughly \
        ascend, possibly a few blocks off.",
    params(TipPath, TipQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The block", body = TipResponse),
        (status = 400, description = "Malformed `at`", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "The network is not configured, or `at` is before its first block", body = ErrorBody),
        (status = 422, description = "The network cannot find blocks by time", body = ErrorBody),
        (status = 503, description = "The node is unavailable", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn read(
    State(state): State<Arc<AppState>>,
    Path(path): Path<TipPath>,
    query: Result<Query<TipQuery>, QueryRejection>,
) -> Result<Json<TipResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let time = query
        .at
        .map(|at| match at.parse::<u64>() {
            Ok(seconds) => Ok(seconds),
            Err(_) => at
                .parse::<jiff::Timestamp>()
                .ok()
                .and_then(|time| u64::try_from(time.as_second()).ok())
                .ok_or_else(|| {
                    Error::invalid_request("at must be an RFC 3339 time or Unix seconds")
                }),
        })
        .transpose()?;
    let network = base::Network::new(base::Chain::new(path.chain), path.network);
    let dated = state
        .payments
        .service(&network)?
        .dated
        .clone()
        .ok_or_else(|| Error::unprocessable("this network cannot find blocks by time"))?;
    let stamp = match time {
        None => dated.tip().await?,
        Some(time) => dated
            .position_at(time)
            .await?
            .ok_or_else(|| Error::not_found("the time is before the network's first block"))?,
    };
    Ok(Json(TipResponse {
        position: stamp.position,
        timestamp: Some(stamp.timestamp),
    }))
}
