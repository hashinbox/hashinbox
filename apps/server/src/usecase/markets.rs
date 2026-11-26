//! The markets over the `market` and `market_candle_1h` models: each one
//! with its last quote, turning its feed on or off, its hourly candles of
//! an asset, and each asset's latest price.

use std::collections::{BTreeMap, BTreeSet};

use base::Decimal;

use crate::{
    db::Db,
    error::Error,
    models::{
        market::{self, Market},
        market_candle_1h::{self, MarketCandles1h, MarketCandles1hExchangeAssetMapCached},
    },
    usecase::now,
};

/// How far back candles are read, Unix seconds.
const WEEK_S: i64 = 7 * 24 * 60 * 60;

/// A market and when it last answered.
pub(crate) struct Quoted {
    pub(crate) market: Market,
    /// Unix ms; `None`: no candle kept.
    pub(crate) last_at: Option<i64>,
}

/// Every market, by name, each with its last quote.
///
/// # Errors
///
/// The read.
pub(crate) async fn list(db: &Db) -> Result<Vec<Quoted>, Error> {
    Ok(db
        .read(|connection| {
            let markets = market::db::find_all(connection)?;
            let quoted = markets.into_iter().map(|market| {
                let last_at = market_candle_1h::db::find_last_at(connection, &market.name)?;
                Ok(Quoted { market, last_at })
            });
            quoted.collect()
        })
        .await?)
}

/// Each asset's latest USD price in `candles`, by ticker
/// ([`MarketCandles1h::close`]).
pub(crate) fn prices(candles: &MarketCandles1hExchangeAssetMapCached) -> BTreeMap<String, Decimal> {
    let candles = candles.collection();
    let tickers: BTreeSet<&str> = candles.iter().map(|candle| candle.asset.as_str()).collect();
    let prices = tickers.into_iter().filter_map(|ticker| {
        let usd = candles.close(ticker)?;
        Some((ticker.to_owned(), usd))
    });
    prices.collect()
}

/// Turns market `name`'s feed on (`feed`) or off; the market job reads it
/// on its next tick. Logged as `market.feed.set` (`market feed`, info).
///
/// # Errors
///
/// Not found for no such market; the write.
pub(crate) async fn set_feed(db: &Db, name: &str, feed: bool) -> Result<Quoted, Error> {
    let named = name.to_owned();
    let quoted = db
        .write(move |connection| {
            if market::db::set_feed(connection, &named, feed, now())? == 0 {
                return Ok(None);
            }
            let market = market::db::find(connection, &named)?;
            let last_at = market_candle_1h::db::find_last_at(connection, &named)?;
            Ok(market.map(|market| Quoted { market, last_at }))
        })
        .await?;
    let quoted = quoted.ok_or_else(|| Error::not_found("no such market"))?;
    tracing::info!(target: crate::events::MARKET_FEED_SET, market = name, feed);
    Ok(quoted)
}

/// Market `name`'s candles of `asset` from hour `after` (Unix seconds) on,
/// at most the last 7 days, oldest first.
///
/// # Errors
///
/// Not found for no such market; the read.
pub(crate) async fn candles(
    db: &Db,
    name: &str,
    asset: &str,
    after: Option<i64>,
) -> Result<MarketCandles1h, Error> {
    let (name, asset) = (name.to_owned(), asset.to_owned());
    let from = after.unwrap_or(0).max(now() / 1000 - WEEK_S);
    let candles = db
        .read(move |connection| {
            if market::db::find(connection, &name)?.is_none() {
                return Ok(None);
            }
            let candles = market_candle_1h::db::find_of_market(connection, &name, &asset, from)?;
            Ok(Some(candles))
        })
        .await?;
    candles.ok_or_else(|| Error::not_found("no such market"))
}
