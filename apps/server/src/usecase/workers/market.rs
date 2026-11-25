//! The market job: one, never finished. Every [`PERIOD`] it reads which
//! markets are asked (`market.feed`; one with no row is never asked), asks
//! each one that is due (its `interval`), all at once, for its USD quote of
//! every `network_asset` ticker, adds each quote to its market's candle of
//! its hour in memory ([`MarketCandles1hExchangeAssetMapCached`]: what a
//! kept leg is valued at) and saves the candles it moved
//! (`market_candle_1h`). A market not due keeps its candle as it is. While
//! memory holds none (after a start), it loads the hour's kept so far. It
//! asks only the present: no past hour is fetched. It writes market data
//! only.
//!
//! Its `before` enqueues it when its queue starts; it is unique. Its topic
//! (`market`) conflicts with nothing else.

use std::{
    collections::BTreeMap,
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

use futures_util::future::join_all;
use tasks::{Ack, Context, Policy, Store, StoreError, Topic, Worker, millis};
use tracing::Instrument as _;

use super::{FOREVER, TIMEOUT};
use crate::{
    db::Db,
    events,
    models::{
        NetworkAsset, int, market,
        market::Quote,
        market_candle_1h::{self, MarketCandles1hExchangeAssetMapCached},
        network_asset,
    },
    usecase::market::Market,
};

/// The job kind.
pub(crate) const KIND: &str = "market";

/// How often the markets are asked.
const PERIOD: Duration = Duration::from_secs(10);

/// When a market was last asked, and whether it failed.
#[derive(Clone, Copy)]
struct Asked {
    /// Unix ms.
    at: u64,
    failed: bool,
}

/// Keeps each market's hourly candles, its newest in memory.
pub(crate) struct Candles {
    db: Db,
    markets: Vec<Box<dyn Market>>,
    candles: MarketCandles1hExchangeAssetMapCached,
    /// By market name.
    asked: Mutex<BTreeMap<&'static str, Asked>>,
}

impl Candles {
    pub(crate) fn new(
        db: Db,
        markets: Vec<Box<dyn Market>>,
        candles: MarketCandles1hExchangeAssetMapCached,
    ) -> Self {
        Self {
            db,
            markets,
            candles,
            asked: Mutex::default(),
        }
    }

    /// One tick at `now` (Unix ms): the markets that are asked and due
    /// asked at once, each quote added to its market's candle of its hour
    /// in memory, then the candles moved saved.
    ///
    /// # Errors
    ///
    /// When the read or the write fails.
    pub(crate) async fn tick(&self, now: u64) -> anyhow::Result<()> {
        let start = int(now / 1000 / 3600 * 3600)?;
        let empty = self.candles.is_empty();
        let (assets, feeds, kept) = self
            .db
            .read(move |connection| {
                let assets = network_asset::db::find_one_per_ticker(connection)?;
                let feeds = market::db::find_feeds(connection)?;
                let kept = match empty {
                    true => Some(market_candle_1h::db::find_of_hour(connection, start)?),
                    false => None,
                };
                Ok((assets, feeds, kept))
            })
            .await?;
        if let Some(kept) = kept {
            self.candles.load(kept);
        }
        let due: Vec<(&dyn Market, bool)> = {
            let asked = self.asked.lock().unwrap_or_else(PoisonError::into_inner);
            let on = self.markets.iter().map(AsRef::as_ref);
            let on = on.filter(|market| feeds.iter().any(|name| name == market.name()));
            on.filter_map(|market| {
                let last = asked.get(market.name());
                let due = last
                    .is_none_or(|last| now >= last.at.saturating_add(millis(market.interval())));
                due.then(|| (market, last.is_some_and(|last| last.failed)))
            })
            .collect()
        };
        let asks = due.iter();
        let answers = join_all(asks.map(|(market, failing)| ask(*market, &assets, *failing))).await;
        {
            let mut asked = self.asked.lock().unwrap_or_else(PoisonError::into_inner);
            for ((market, _), answer) in due.iter().zip(&answers) {
                let failed = answer.is_none();
                asked.insert(market.name(), Asked { at: now, failed });
            }
        }
        let moved = self.candles.add(answers.iter().flatten().flatten());
        self.db
            .write(move |connection| Ok(market_candle_1h::db::save(connection, &moved)?))
            .await?;
        Ok(())
    }
}

/// `market`'s quotes of `assets`, asked in a `market` span (`market`;
/// `status`, the HTTP status, `-` for none) and logged
/// (`market.feed.answered`: `ms`, `assets` quoted; debug); a failure
/// logged once per streak (`market.feed.failed`, then
/// `market.feed.recovered`; `failing`: the last ask failed). `None` when
/// it failed.
async fn ask(market: &dyn Market, assets: &[NetworkAsset], failing: bool) -> Option<Vec<Quote>> {
    let name = market.name();
    let span = tracing::info_span!("market", market = name, status = "-");
    async {
        let started = Instant::now();
        let answer = market.quotes(assets).await;
        let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let quoted = answer.as_ref().map_or(0, Vec::len);
        tracing::debug!(target: events::MARKET_FEED_ANSWERED, ms, assets = quoted);
        match &answer {
            Ok(_) if failing => {
                tracing::info!(target: events::MARKET_FEED_RECOVERED, market = name);
            }
            Err(error) if !failing => {
                let reason = format!("{error:#}");
                tracing::warn!(target: events::MARKET_FEED_FAILED, reason);
            }
            _ => {}
        }
        answer.ok()
    }
    .instrument(span)
    .await
}

impl Worker for Candles {
    const KIND: &'static str = KIND;
    /// One at all, retried for ever.
    const POLICY: Policy = Policy {
        unique: true,
        timeout: TIMEOUT,
        retry: FOREVER,
        ..Policy::DEFAULT
    };
    type Input = ();
    type State = ();

    /// `market`: it conflicts with nothing else.
    fn topic((): &()) -> Option<Topic> {
        Topic::new([KIND])
    }

    /// [`Candles::tick`], again after [`PERIOD`]; a failed read or write
    /// retries.
    async fn run(&self, _cx: Context<'_, (), ()>) -> Ack {
        match self.tick(tasks::now()).await {
            Ok(()) => Ack::Again(tasks::now().saturating_add(millis(PERIOD)), None),
            Err(error) => Ack::Retry(format!("{error:#}")),
        }
    }

    /// The one market job.
    async fn before(&self, store: &dyn Store) -> Result<(), StoreError> {
        Self::enqueue(store, &()).await.map(|_| ())
    }
}
