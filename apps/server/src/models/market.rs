//! `market`: a source of market data (later of swaps too), one row per
//! implementation (`usecase::market`), seeded (`seed.sql`). Its `feed`
//! says whether the market job asks it for quotes; a [`Quote`] is what one
//! answers.

use base::Decimal;
use diesel::{prelude::*, sqlite::Sqlite};

use super::schema::market;

/// One `market` row.
#[derive(
    Clone, Debug, PartialEq, Eq, Queryable, Selectable, serde::Serialize, utoipa::ToSchema,
)]
#[diesel(table_name = market, check_for_backend(Sqlite))]
pub(crate) struct Market {
    /// Its implementation: `binance`, `kraken`, `coinbase`, `bitstamp` or
    /// `coingecko`.
    pub(crate) name: String,
    /// Whether it is asked for quotes.
    pub(crate) feed: bool,
    /// Unix ms it was last changed.
    pub(crate) updated_at: i64,
}

/// One market's USD price of one asset at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Quote {
    /// The market (`market.name`).
    pub(crate) market: &'static str,
    /// A `network_asset` ticker: `BTC`.
    pub(crate) asset: String,
    pub(crate) usd: Decimal,
    /// When it answered, Unix ms.
    pub(crate) at: u64,
}

/// The `market` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// Every market, by name.
    pub(crate) fn find_all(connection: &mut SqliteConnection) -> QueryResult<Vec<Market>> {
        market::table
            .order(market::name)
            .select(Market::as_select())
            .load(connection)
    }

    /// Market `name`, if kept.
    pub(crate) fn find(
        connection: &mut SqliteConnection,
        name: &str,
    ) -> QueryResult<Option<Market>> {
        market::table
            .find(name)
            .select(Market::as_select())
            .first(connection)
            .optional()
    }

    /// The names of the markets asked for quotes, sorted.
    pub(crate) fn find_feeds(connection: &mut SqliteConnection) -> QueryResult<Vec<String>> {
        market::table
            .filter(market::feed.eq(true))
            .order(market::name)
            .select(market::name)
            .load(connection)
    }

    /// Sets market `name`'s `feed` at `at` (Unix ms); how many (0: no such
    /// market).
    pub(crate) fn set_feed(
        connection: &mut SqliteConnection,
        name: &str,
        feed: bool,
        at: i64,
    ) -> QueryResult<usize> {
        diesel::update(market::table.find(name))
            .set((market::feed.eq(feed), market::updated_at.eq(at)))
            .execute(connection)
    }
}
