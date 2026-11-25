//! The market job over fake markets (each market's candles, a market off
//! or not due, a new hour, a restart), and a leg's USD value at the latest
//! price, on SQLite temp files. Offline.

use std::{sync::atomic::Ordering, time::Duration};

use base::{
    BlockStore,
    testing::fixtures::{block, payment, run},
};
use diesel::{RunQueryDsl as _, sql_types::Nullable, sql_types::Text};

use super::{
    storage::{address, pending, raw, record, watch},
    support::{db::seeded, scripted::network},
};
use crate::{
    db::Db,
    models::{
        market::{self, Quote},
        market_candle_1h::{
            self, MarketCandle1h, MarketCandles1h, MarketCandles1hExchangeAssetMapCached,
        },
    },
    usecase::{
        market::{Market, tests::Fixed},
        sdk::Stores,
        workers::market::Candles,
    },
};

/// The hour block 0 is mined in, Unix seconds.
const HOUR: u64 = 1_717_200_000;
/// When block 0 was made: 10 minutes into [`HOUR`].
const MINED: u64 = HOUR + 600;

#[derive(diesel::QueryableByName, Debug, PartialEq, Eq)]
struct Usd {
    #[diesel(sql_type = Nullable<Text>)]
    amount_usd: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    total_received_usd: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    total_sent_usd: Option<String>,
}

/// Every leg's USD value and totals, by id.
fn usd(directory: &tempfile::TempDir) -> Vec<[Option<String>; 3]> {
    diesel::sql_query("SELECT amount_usd, total_received_usd, total_sent_usd FROM leg ORDER BY id")
        .load::<Usd>(&mut raw(directory))
        .unwrap()
        .into_iter()
        .map(|row| [row.amount_usd, row.total_received_usd, row.total_sent_usd])
        .collect()
}

/// `text` as kept.
fn some(text: &str) -> Option<String> {
    Some(text.to_owned())
}

/// Alice watched, then block 0 mined at [`MINED`], paying her 1.5 BTC.
async fn mined(storage: &Stores) {
    let network = network();
    watch(storage, &address(&network, "alice"), 0).await;
    let mut zero = block(
        &network,
        0,
        0,
        vec![payment(&network, "p", "alice", 150_000_000, None)],
    );
    zero.header.timestamp = Some(MINED);
    BlockStore::insert(storage, &run(&network, vec![zero]))
        .await
        .unwrap();
}

/// `market`'s quote of BTC at `usd`, `at` Unix seconds.
fn quote(market: &'static str, usd: &str, at: u64) -> Quote {
    Quote {
        market,
        asset: "BTC".to_owned(),
        usd: usd.parse().unwrap(),
        at: at * 1000,
    }
}

/// The candles of `quotes`.
fn candles_of(quotes: &[Quote]) -> MarketCandles1h {
    let mut candles = MarketCandles1h::default();
    for quote in quotes {
        candles.add(quote);
    }
    candles
}

/// `quotes` in memory.
fn cached(quotes: &[Quote]) -> MarketCandles1hExchangeAssetMapCached {
    let cached = MarketCandles1hExchangeAssetMapCached::default();
    cached.load(candles_of(quotes));
    cached
}

/// The BTC candles `latest` holds.
fn btc(latest: &MarketCandles1hExchangeAssetMapCached) -> Vec<MarketCandle1h> {
    let candles = latest.collection().into_iter();
    candles.filter(|candle| candle.asset == "BTC").collect()
}

/// The market job over `markets`, keeping its candles in `latest`.
fn job(db: &Db, markets: Vec<Fixed>, latest: &MarketCandles1hExchangeAssetMapCached) -> Candles {
    let markets = markets
        .into_iter()
        .map(|market| Box::new(market) as Box<dyn Market>);
    Candles::new(db.clone(), markets.collect(), latest.clone())
}

/// The kept BTC candles, by hour then market.
async fn kept(db: &Db) -> Vec<MarketCandle1h> {
    db.read(|connection| {
        use crate::models::schema::market_candle_1h::dsl;
        use diesel::{ExpressionMethods as _, QueryDsl as _};
        let btc = dsl::market_candle_1h.filter(dsl::asset.eq("BTC"));
        Ok(btc.order((dsl::start, dsl::market)).load(connection)?)
    })
    .await
    .unwrap()
}

/// `candles`' market, open, high, low and close.
fn ohlc(candles: &[MarketCandle1h]) -> Vec<[&str; 5]> {
    fn row(c: &MarketCandle1h) -> [&str; 5] {
        [&c.market, &c.open, &c.high, &c.low, &c.close]
    }
    candles.iter().map(row).collect()
}

#[tokio::test]
async fn each_market_keeps_its_own_candle_and_the_price_is_their_median() {
    let (_, db, _directory) = seeded().await;
    let latest = MarketCandles1hExchangeAssetMapCached::default();
    let markets = vec![
        Fixed::new("binance", Some("100")),
        Fixed::new("kraken", Some("101")),
        Fixed::new("coinbase", Some("300")),
        Fixed::new("bitstamp", None),
    ];
    job(&db, markets, &latest).tick(tasks::now()).await.unwrap();
    assert_eq!(
        ohlc(&kept(&db).await),
        [
            ["binance", "100", "100", "100", "100"],
            ["coinbase", "300", "300", "300", "300"],
            ["kraken", "101", "101", "101", "101"],
        ],
        "bitstamp failed: none"
    );
    assert_eq!(btc(&latest).len(), 3, "in memory too");
    assert_eq!(latest.close("BTC").unwrap().to_string(), "101");
}

#[tokio::test]
async fn a_market_whose_feed_is_off_is_not_asked() {
    let (_, db, _directory) = seeded().await;
    db.write(|connection| Ok(market::db::set_feed(connection, "kraken", false, 1)?))
        .await
        .unwrap();
    let kraken = Fixed::new("kraken", Some("200"));
    let asked = kraken.asked.clone();
    let markets = vec![Fixed::new("binance", Some("100")), kraken];
    job(&db, markets, &Default::default())
        .tick(tasks::now())
        .await
        .unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    assert_eq!(ohlc(&kept(&db).await).len(), 1, "binance only");
}

#[tokio::test]
async fn a_market_not_due_is_not_asked_and_keeps_its_candle() {
    let (_, db, _directory) = seeded().await;
    let latest = MarketCandles1hExchangeAssetMapCached::default();
    let slow = Fixed {
        interval: Duration::from_secs(60),
        ..Fixed::new("coingecko", Some("100"))
    };
    let asked = slow.asked.clone();
    let job = job(&db, vec![slow, Fixed::new("binance", Some("110"))], &latest);
    let now = tasks::now();
    job.tick(now).await.unwrap();
    job.tick(now + 10_000).await.unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 1, "not due: not asked");
    assert_eq!(btc(&latest).len(), 2, "its candle kept");
    assert_eq!(latest.close("BTC").unwrap().to_string(), "105");
    job.tick(now + 60_000).await.unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 2, "due again");
}

#[tokio::test]
async fn a_new_hour_starts_new_candles() {
    let (_, db, _directory) = seeded().await;
    let earlier = tasks::now() / 1000 - 3600;
    let last_hour = candles_of(&[quote("binance", "90", earlier)]);
    let saved = last_hour.to_vec();
    db.write(move |connection| Ok(market_candle_1h::db::save(connection, &saved)?))
        .await
        .unwrap();
    let latest = MarketCandles1hExchangeAssetMapCached::default();
    latest.load(last_hour);
    job(&db, vec![Fixed::new("binance", Some("100"))], &latest)
        .tick(tasks::now())
        .await
        .unwrap();
    let kept = kept(&db).await;
    assert_eq!(
        ohlc(&kept),
        [
            ["binance", "90", "90", "90", "90"],
            ["binance", "100", "100", "100", "100"],
        ],
        "the last hour kept, a new candle"
    );
    let memory = btc(&latest);
    assert_eq!(memory.len(), 1, "the newest hour only");
    assert_eq!(memory[0].start, kept[1].start);
}

#[tokio::test]
async fn candles_are_loaded_after_a_restart() {
    let (_, db, _directory) = seeded().await;
    let now = tasks::now();
    job(
        &db,
        vec![Fixed::new("binance", Some("100"))],
        &Default::default(),
    )
    .tick(now)
    .await
    .unwrap();
    let latest = MarketCandles1hExchangeAssetMapCached::default();
    job(&db, vec![Fixed::new("binance", Some("120"))], &latest)
        .tick(now + 10_000)
        .await
        .unwrap();
    assert_eq!(
        ohlc(&kept(&db).await),
        [["binance", "100", "120", "100", "120"]],
        "its open kept"
    );
    assert_eq!(btc(&latest)[0].open, "100", "loaded into memory");
}

#[tokio::test]
async fn a_leg_of_the_current_hour_is_valued_from_memory() {
    let (_, db, directory) = seeded().await;
    let latest = cached(&[
        quote("binance", "64000", MINED - 5),
        quote("kraken", "64002", MINED - 5),
    ]);
    mined(&Stores::new(db.clone(), latest)).await;
    assert!(kept(&db).await.is_empty(), "no candle kept");
    assert_eq!(
        usd(&directory),
        [[some("96001.5"), some("96001.5"), some("0")]],
        "1.5 BTC at 64001"
    );
}

#[tokio::test]
async fn a_leg_is_valued_at_the_latest_price_whatever_its_hour() {
    let (_, db, directory) = seeded().await;
    let latest = cached(&[
        quote("binance", "70000", HOUR + 7200),
        quote("kraken", "60000", HOUR),
    ]);
    mined(&Stores::new(db.clone(), latest)).await;
    assert_eq!(
        usd(&directory),
        [[some("105000"), some("105000"), some("0")]],
        "1.5 BTC at binance's 70000: kraken is behind"
    );
}

#[tokio::test]
async fn a_leg_with_no_price_has_none_and_counts_zero() {
    let (storage, _db, directory) = seeded().await;
    mined(&storage).await;
    assert_eq!(usd(&directory), [[None, some("0"), some("0")]]);
}

#[tokio::test]
async fn a_reorg_resets_the_usd_totals() {
    let (_, db, directory) = seeded().await;
    let latest = cached(&[quote("binance", "2", MINED)]);
    let storage = Stores::new(db.clone(), latest);
    let network = network();
    watch(&storage, &address(&network, "alice"), 0).await;
    record(&db, &pending(&network, "s", "alice", 300_000_000)).await;
    let mut zero = block(
        &network,
        0,
        0,
        vec![payment(&network, "s", "alice", 300_000_000, None)],
    );
    zero.header.timestamp = Some(MINED);
    BlockStore::insert(&storage, &run(&network, vec![zero]))
        .await
        .unwrap();
    assert_eq!(usd(&directory), [[some("6"), some("6"), some("0")]]);
    BlockStore::delete(&storage, &network, 0).await.unwrap();
    let [reorged] = usd(&directory).try_into().unwrap();
    assert_eq!(reorged[1..], [None, None], "pending: no totals");
}
