//! `market_candle_1h`: one market's hour of one asset's USD price, built
//! from its quotes by the market job (`workers::market`).
//! [`MarketCandles1h`] is any candles (what the table reads and writes);
//! [`MarketCandles1hAssetMap`] one market's newest by asset,
//! [`MarketCandles1hExchangeAssetMap`] every market's, and
//! [`MarketCandles1hExchangeAssetMapCached`] that map shared, in memory:
//! the job adds to it, a leg is valued by it. Each answers an asset's
//! price with `close`: the median close of its newest hour.

use std::{
    collections::BTreeMap,
    sync::{Arc, PoisonError, RwLock},
};

use base::Decimal;
use diesel::{prelude::*, sqlite::Sqlite, upsert::excluded};

use super::{market::Quote, schema::market_candle_1h};

/// One `market_candle_1h` row: open, high, low and close are exact decimal
/// USD.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Queryable,
    QueryableByName,
    Selectable,
    Insertable,
    serde::Serialize,
    utoipa::ToSchema,
)]
#[diesel(table_name = market_candle_1h, check_for_backend(Sqlite))]
pub(crate) struct MarketCandle1h {
    /// The market (`market.name`) it is of.
    pub(crate) market: String,
    /// A `network_asset` ticker: `BTC`.
    pub(crate) asset: String,
    /// The hour, Unix seconds, hour-aligned.
    pub(crate) start: i64,
    pub(crate) open: String,
    pub(crate) high: String,
    pub(crate) low: String,
    pub(crate) close: String,
    /// When its last quote answered, Unix ms.
    pub(crate) updated_at: i64,
}

/// The hour of `at` (Unix ms): its start, Unix seconds.
fn hour_of(at: u64) -> i64 {
    i64::try_from(at / 1000 / 3600 * 3600).unwrap_or(i64::MAX)
}

impl MarketCandle1h {
    /// The candle of `quote`'s hour started by it.
    pub(crate) fn open(quote: &Quote) -> Self {
        let usd = quote.usd.to_string();
        Self {
            market: quote.market.to_owned(),
            asset: quote.asset.clone(),
            start: hour_of(quote.at),
            open: usd.clone(),
            high: usd.clone(),
            low: usd.clone(),
            close: usd,
            updated_at: i64::try_from(quote.at).unwrap_or(i64::MAX),
        }
    }

    /// Whether `quote` is of it: its market, asset and hour.
    fn holds(&self, quote: &Quote) -> bool {
        self.market == quote.market && self.asset == quote.asset && self.start == hour_of(quote.at)
    }

    /// Moves high, low and close by `quote` (same market, asset, hour); a
    /// kept high or low that does not decode is replaced.
    pub(crate) fn add(&mut self, quote: &Quote) {
        let (usd, kept) = (&quote.usd, |text: &str| text.parse::<Decimal>().ok());
        if kept(&self.high).is_none_or(|high| *usd > high) {
            self.high = usd.to_string();
        }
        if kept(&self.low).is_none_or(|low| *usd < low) {
            self.low = usd.to_string();
        }
        self.close = usd.to_string();
        self.updated_at = i64::try_from(quote.at).unwrap_or(i64::MAX);
    }
}

collection! {
    /// Candles of any markets, assets and hours.
    #[derive(Clone, Debug, Default)]
    MarketCandles1h(MarketCandle1h)
}

impl MarketCandles1h {
    /// `asset`'s price: the median close of its candles of its newest
    /// hour (a market whose newest is older is left out): the middle one,
    /// or the mean of the two middle ones; a close that does not decode is
    /// left out. `None` when none.
    pub(crate) fn close(&self, asset: &str) -> Option<Decimal> {
        let of_asset = self.iter().filter(|candle| candle.asset == asset);
        let newest = of_asset.clone().map(|candle| candle.start).max()?;
        let newest = of_asset.filter(|candle| candle.start == newest);
        let mut closes: Vec<Decimal> = newest
            .filter_map(|candle| candle.close.parse().ok())
            .collect();
        closes.sort_unstable();
        let middle = closes.len() / 2;
        match closes.len() {
            0 => None,
            n if n % 2 == 1 => Some(closes.swap_remove(middle)),
            _ => (&closes[middle - 1] + &closes[middle]).checked_div(&Decimal::from(2_u64)),
        }
    }

    /// `quote` into its market's candle of its hour (opened if new); that
    /// candle.
    pub(crate) fn add(&mut self, quote: &Quote) -> &MarketCandle1h {
        if let Some(at) = self.0.iter().position(|candle| candle.holds(quote)) {
            self.0[at].add(quote);
            return &self.0[at];
        }
        let at = self.0.len();
        self.0.push(MarketCandle1h::open(quote));
        &self.0[at]
    }
}

/// One market's candles by asset (ticker): each asset's newest hour only.
#[derive(Clone, Debug, Default)]
pub(crate) struct MarketCandles1hAssetMap(BTreeMap<String, MarketCandles1h>);

impl MarketCandles1hAssetMap {
    /// `quote` (of this market) into its asset's candle of its hour; an
    /// older hour's dropped. That candle.
    pub(crate) fn add(&mut self, quote: &Quote) -> &MarketCandle1h {
        let candles = self.0.entry(quote.asset.clone()).or_default();
        candles.0.retain(|kept| kept.start >= hour_of(quote.at));
        candles.add(quote)
    }

    /// `candle` (of this market) kept, unless its asset holds a newer
    /// hour; the one it held dropped (one market, one asset: one candle an
    /// hour).
    fn insert(&mut self, candle: MarketCandle1h) {
        let candles = &mut self.0.entry(candle.asset.clone()).or_default().0;
        if candles.iter().any(|kept| kept.start > candle.start) {
            return;
        }
        candles.clear();
        candles.push(candle);
    }

    /// Every candle it holds.
    pub(crate) fn collection(&self) -> MarketCandles1h {
        self.0
            .values()
            .flat_map(|candles| candles.iter().cloned())
            .collect()
    }
}

/// Every market's candles, by market (`market.name`), then asset.
#[derive(Clone, Debug, Default)]
pub(crate) struct MarketCandles1hExchangeAssetMap(BTreeMap<String, MarketCandles1hAssetMap>);

/// `candles` by market, then asset, each asset's newest hour only.
impl From<MarketCandles1h> for MarketCandles1hExchangeAssetMap {
    fn from(candles: MarketCandles1h) -> Self {
        let mut map = Self::default();
        for candle in candles {
            map.0
                .entry(candle.market.clone())
                .or_default()
                .insert(candle);
        }
        map
    }
}

impl MarketCandles1hExchangeAssetMap {
    /// `quote` into its market's candle of its hour; that candle.
    pub(crate) fn add(&mut self, quote: &Quote) -> &MarketCandle1h {
        self.0
            .entry(quote.market.to_owned())
            .or_default()
            .add(quote)
    }

    /// Every candle it holds.
    pub(crate) fn collection(&self) -> MarketCandles1h {
        self.0
            .values()
            .flat_map(|market| market.collection())
            .collect()
    }

    /// `asset`'s price: the median close of the markets' newest hour of it
    /// ([`MarketCandles1h::close`]).
    pub(crate) fn close(&self, asset: &str) -> Option<Decimal> {
        self.collection().close(asset)
    }
}

/// [`MarketCandles1hExchangeAssetMap`] in memory, shared: clones share it.
/// The market job adds to it; a kept leg and the markets route read it.
#[derive(Clone, Default)]
pub(crate) struct MarketCandles1hExchangeAssetMapCached(
    Arc<RwLock<MarketCandles1hExchangeAssetMap>>,
);

impl MarketCandles1hExchangeAssetMapCached {
    /// Replaces what it holds by `candles` (as kept).
    pub(crate) fn load(&self, candles: MarketCandles1h) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = candles.into();
    }

    /// Whether it holds none.
    pub(crate) fn is_empty(&self) -> bool {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .is_empty()
    }

    /// `quotes` each into its market's candle of its hour; the candles
    /// they moved, to save.
    pub(crate) fn add<'a>(&self, quotes: impl IntoIterator<Item = &'a Quote>) -> MarketCandles1h {
        let mut held = self.0.write().unwrap_or_else(PoisonError::into_inner);
        quotes
            .into_iter()
            .map(|quote| held.add(quote).clone())
            .collect()
    }

    /// Every candle it holds.
    pub(crate) fn collection(&self) -> MarketCandles1h {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .collection()
    }

    /// `asset`'s price ([`MarketCandles1hExchangeAssetMap::close`]).
    pub(crate) fn close(&self, asset: &str) -> Option<Decimal> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .close(asset)
    }
}

/// The `market_candle_1h` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// Every candle of hour `start` (Unix seconds).
    pub(crate) fn find_of_hour(
        connection: &mut SqliteConnection,
        start: i64,
    ) -> QueryResult<MarketCandles1h> {
        market_candle_1h::table
            .filter(market_candle_1h::start.eq(start))
            .select(MarketCandle1h::as_select())
            .load(connection)
            .map(MarketCandles1h::from)
    }

    /// Market `market`'s candles of `asset` from hour `start` (Unix
    /// seconds) on, oldest first.
    pub(crate) fn find_of_market(
        connection: &mut SqliteConnection,
        market: &str,
        asset: &str,
        start: i64,
    ) -> QueryResult<MarketCandles1h> {
        market_candle_1h::table
            .filter(market_candle_1h::market.eq(market))
            .filter(market_candle_1h::asset.eq(asset))
            .filter(market_candle_1h::start.ge(start))
            .order(market_candle_1h::start)
            .select(MarketCandle1h::as_select())
            .load(connection)
            .map(MarketCandles1h::from)
    }

    /// When market `market`'s last quote answered, Unix ms; `None`: no
    /// candle kept.
    pub(crate) fn find_last_at(
        connection: &mut SqliteConnection,
        market: &str,
    ) -> QueryResult<Option<i64>> {
        market_candle_1h::table
            .filter(market_candle_1h::market.eq(market))
            .select(diesel::dsl::max(market_candle_1h::updated_at))
            .first(connection)
    }

    /// Keeps `rows`, each replacing the one of its market, asset and hour
    /// (SQLite takes no batch upsert from diesel: one statement each, in
    /// the caller's transaction).
    pub(crate) fn save(
        connection: &mut SqliteConnection,
        rows: &[MarketCandle1h],
    ) -> QueryResult<()> {
        for row in rows {
            diesel::insert_into(market_candle_1h::table)
                .values(row)
                .on_conflict((
                    market_candle_1h::market,
                    market_candle_1h::asset,
                    market_candle_1h::start,
                ))
                .do_update()
                .set((
                    market_candle_1h::open.eq(excluded(market_candle_1h::open)),
                    market_candle_1h::high.eq(excluded(market_candle_1h::high)),
                    market_candle_1h::low.eq(excluded(market_candle_1h::low)),
                    market_candle_1h::close.eq(excluded(market_candle_1h::close)),
                    market_candle_1h::updated_at.eq(excluded(market_candle_1h::updated_at)),
                ))
                .execute(connection)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{MarketCandle1h, MarketCandles1h, MarketCandles1hExchangeAssetMapCached};
    use crate::models::market::Quote;

    /// `market`'s quote of BTC at `usd`, `at` Unix ms.
    fn quote(market: &'static str, usd: &str, at: u64) -> Quote {
        Quote {
            market,
            asset: "BTC".to_owned(),
            usd: usd.parse().unwrap(),
            at,
        }
    }

    /// `candle`'s open, high, low and close.
    fn ohlc(candle: &MarketCandle1h) -> [&str; 4] {
        [&candle.open, &candle.high, &candle.low, &candle.close]
    }

    #[test]
    fn a_candle_opens_on_its_first_quote_and_moves_by_the_next() {
        let mut candle = MarketCandle1h::open(&quote("a", "100", 7_200_000 + 5_000));
        assert_eq!(candle.start, 7_200, "its hour, Unix seconds");
        assert_eq!(ohlc(&candle), ["100", "100", "100", "100"]);
        candle.add(&quote("a", "120", 7_210_000));
        candle.add(&quote("a", "90", 7_220_000));
        candle.add(&quote("a", "95", 7_230_000));
        assert_eq!(ohlc(&candle), ["100", "120", "90", "95"]);
        assert_eq!(candle.updated_at, 7_230_000);
    }

    #[test]
    fn candles_add_each_quote_to_its_markets_candle_of_its_hour() {
        let mut candles = MarketCandles1h::default();
        candles.add(&quote("a", "100", 0));
        candles.add(&quote("a", "110", 10_000));
        candles.add(&quote("b", "200", 10_000));
        candles.add(&quote("a", "300", 3_600_000));
        assert_eq!(candles.len(), 3);
        assert_eq!(ohlc(&candles[0]), ["100", "110", "100", "110"]);
        assert_eq!(candles[2].start, 3600, "a new hour, a new candle");
        assert_eq!(candles[2].open, "300");
    }

    #[test]
    fn the_price_is_the_median_close_of_an_asset() {
        let mut candles = MarketCandles1h::default();
        for (market, usd) in [("a", "100"), ("b", "102"), ("c", "300")] {
            candles.add(&quote(market, usd, 0));
        }
        assert_eq!(
            candles.close("BTC").unwrap().to_string(),
            "102",
            "odd: the middle"
        );
        candles.add(&quote("d", "104", 0));
        assert_eq!(
            candles.close("BTC").unwrap().to_string(),
            "103",
            "even: the mean"
        );
        assert_eq!(candles.close("ETH"), None, "unknown asset");
    }

    #[test]
    fn the_price_leaves_out_a_market_behind_the_newest_hour() {
        let mut candles = MarketCandles1h::default();
        candles.add(&quote("kraken", "50", 0));
        candles.add(&quote("binance", "100", 3_600_000));
        assert_eq!(
            candles.close("BTC").unwrap().to_string(),
            "100",
            "binance only"
        );
    }

    #[test]
    fn memory_keeps_each_markets_newest_hour_and_returns_what_moved() {
        let cached = MarketCandles1hExchangeAssetMapCached::default();
        let moved = cached.add(&[quote("a", "100", 0), quote("b", "200", 0)]);
        assert_eq!(moved.len(), 2);
        let moved = cached.add(&[quote("a", "300", 3_600_000)]);
        assert_eq!(ohlc(&moved[0]), ["300", "300", "300", "300"]);
        let held = cached.collection();
        assert_eq!(held.len(), 2, "a's last hour dropped, b's kept");
        assert_eq!(
            cached.close("BTC").unwrap().to_string(),
            "300",
            "b is behind"
        );
    }
}
