//! `/v1/wallets/{id}/indexes/{asset}/legs`: an index's mined legs, latest
//! first, a page at a time (its pending legs are on the index itself).

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, wallet_address, wallet_indexes::IndexPath};
use crate::{
    api::AppState,
    models::{self, Leg, leg},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(legs))
}

/// Query of `GET /v1/wallets/{id}/indexes/{asset}/legs`.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
struct LegsQuery {
    /// `next_cursor` of the previous page (a `seq`); absent for the first
    /// page.
    before: Option<i64>,
    /// Page size, 1 to 200.
    #[serde(default = "LegsQuery::default_limit")]
    #[param(minimum = 1, maximum = 200, default = 50)]
    limit: i64,
}

impl LegsQuery {
    const fn default_limit() -> i64 {
        50
    }
}

/// `GET /v1/wallets/{id}/indexes/{asset}/legs`: a page of mined legs.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = LegPage)]
struct LegsResponse {
    /// Live mined legs, latest first, each with the balance after it
    /// (`total`).
    legs: Vec<Leg>,
    /// Pass as `before` for the next page; null on the last page. Always
    /// present.
    #[schema(required)]
    next_cursor: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}/indexes/{asset}/legs",
    operation_id = "list_wallet_index_legs",
    summary = "List an index's mined legs",
    description = "The index's legs in live blocks, latest first, each with the balance right \
        after it. Its pending legs are on the index itself.",
    params(IndexPath, LegsQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "One page of legs", body = LegsResponse),
        (status = 400, description = "Malformed wallet ID, query or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn legs(
    State(state): State<Arc<AppState>>,
    Path(path): Path<IndexPath>,
    query: Result<Query<LegsQuery>, QueryRejection>,
) -> Result<Json<LegsResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    if !(1..=200).contains(&query.limit) {
        return Err(Error::invalid_request(
            "legs limit must be between 1 and 200",
        ));
    }
    let address = wallet_address(&state.payments, &path.id)?;
    let id = models::Wallet::id(&address)
        .and_then(|wallet| models::child(&wallet, &[&path.asset]))
        .map_err(|error| Error::invalid_request(format!("{error:#}")))?;
    let (before, limit) = (query.before.unwrap_or(i64::MAX), query.limit);
    let legs = state
        .db
        .read(move |connection| Ok(leg::db::find_page(connection, &id, before, limit)?))
        .await?;
    let next_cursor = (i64::try_from(legs.len()) == Ok(limit))
        .then(|| legs.last().and_then(|leg| leg.seq))
        .flatten();
    Ok(Json(LegsResponse { legs, next_cursor }))
}
