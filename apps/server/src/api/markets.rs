//! `/v1/markets`: the market data sources, with each asset's latest USD
//! price from memory; turning one's feed on or off; and one market's
//! hourly candles of an asset over the last 7 days.

use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{
    api::AppState,
    models::{market::Market, market_candle_1h::MarketCandle1h},
    usecase::markets::{self, Quoted},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(set_feed))
        .routes(routes!(list_candles))
}

/// A market and when it last answered.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = MarketListing)]
struct MarketResponse {
    #[serde(flatten)]
    market: Market,
    /// When its last quote answered, Unix ms; always present, null: no
    /// candle kept.
    #[schema(required)]
    last_at: Option<i64>,
}

impl From<Quoted> for MarketResponse {
    fn from(quoted: Quoted) -> Self {
        Self {
            market: quoted.market,
            last_at: quoted.last_at,
        }
    }
}

/// `GET /v1/markets`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = MarketList)]
struct ListResponse {
    /// Every market, by name.
    markets: Vec<MarketResponse>,
    /// Each asset's latest USD price, exact decimal, by ticker: the median
    /// close of the markets' candles of the current hour; empty until the
    /// market job's first tick after a start.
    prices: BTreeMap<String, String>,
}

#[utoipa::path(
    get,
    path = "/v1/markets",
    operation_id = "list_markets",
    summary = "List the markets and the latest prices",
    description = "Every market data source: its name, whether it is asked for quotes \
        (`feed`), when it was last changed and when its last quote answered (`last_at`); \
        and, by ticker, each asset's latest USD price: the median close of the current \
        hour's candles.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The markets", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "markets"
)]
async fn list(State(state): State<Arc<AppState>>) -> Result<Json<ListResponse>, Error> {
    let markets = markets::list(&state.db).await?.into_iter();
    let prices = markets::prices(&state.candles).into_iter();
    Ok(Json(ListResponse {
        markets: markets.map(MarketResponse::from).collect(),
        prices: prices
            .map(|(ticker, usd)| (ticker, usd.to_string()))
            .collect(),
    }))
}

/// The market a route acts on.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct MarketPath {
    /// Its name, as `GET /v1/markets` lists it.
    name: String,
}

/// `PATCH /v1/markets/{name}` body.
#[derive(Clone, Deserialize, utoipa::ToSchema)]
#[schema(as = SetMarket)]
#[serde(deny_unknown_fields)]
struct SetRequest {
    /// Whether it is asked for quotes.
    feed: bool,
}

#[utoipa::path(
    patch,
    path = "/v1/markets/{name}",
    operation_id = "set_market",
    summary = "Turn a market's feed on or off",
    description = "Stores whether the market is asked for quotes; the market job reads it \
        on its next tick (within 10 s).",
    params(MarketPath),
    request_body = SetRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Stored", body = MarketResponse),
        (status = 400, description = "Malformed body", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "No such market", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "markets"
)]
async fn set_feed(
    State(state): State<Arc<AppState>>,
    Path(path): Path<MarketPath>,
    request: Result<Json<SetRequest>, JsonRejection>,
) -> Result<Json<MarketResponse>, Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let quoted = markets::set_feed(&state.db, &path.name, request.feed).await?;
    Ok(Json(quoted.into()))
}

/// Which candles `GET /v1/markets/{name}/candles` reads.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct CandlesQuery {
    /// The asset's ticker (`BTC`).
    asset: String,
    /// Only the hours starting at or after this, Unix seconds; at most the
    /// last 7 days.
    after: Option<i64>,
}

/// `GET /v1/markets/{name}/candles`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = MarketCandles)]
struct CandlesResponse {
    /// The market's hourly candles of the asset, oldest first.
    candles: Vec<MarketCandle1h>,
}

#[utoipa::path(
    get,
    path = "/v1/markets/{name}/candles",
    operation_id = "list_market_candles",
    summary = "List a market's hourly candles of an asset",
    description = "The market's own hourly USD candles of the asset (open, high, low and \
        close of its quotes) from `after` on, at most the last 7 days, oldest first; the \
        current hour's moves until the hour ends.",
    params(MarketPath, CandlesQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The candles", body = CandlesResponse),
        (status = 400, description = "Malformed query", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "No such market", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "markets"
)]
async fn list_candles(
    State(state): State<Arc<AppState>>,
    Path(path): Path<MarketPath>,
    query: Result<Query<CandlesQuery>, QueryRejection>,
) -> Result<Json<CandlesResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let candles = markets::candles(&state.db, &path.name, &query.asset, query.after).await?;
    Ok(Json(CandlesResponse {
        candles: candles.into_iter().collect(),
    }))
}
