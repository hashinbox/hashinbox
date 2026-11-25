//! The markets: each a [`Market`] (one file each, public and keyless, every
//! asset in one request) answering its USD [`Quote`] of each asset it
//! lists.
//! Quotes are USD; a market with only USDT pairs (Binance) counts them as
//! USD 1:1 for a non-stable asset and leaves stablecoins out, so a
//! stablecoin's peg is measured against USD.

use std::{collections::BTreeMap, future::Future, pin::Pin, time::Duration};

use anyhow::Error;
use base::Decimal;

use crate::models::{NetworkAsset, market::Quote};

mod binance;
mod bitstamp;
mod coinbase;
mod coingecko;
mod http;
mod kraken;

pub(crate) use binance::Binance;
pub(crate) use bitstamp::Bitstamp;
pub(crate) use coinbase::Coinbase;
pub(crate) use coingecko::CoinGecko;
pub(crate) use http::client;
pub(crate) use kraken::Kraken;

/// A boxed `Send` future: keeps [`Market`] object-safe.
pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One market's client.
pub(crate) trait Market: Send + Sync {
    /// Its `market.name`.
    fn name(&self) -> &'static str;

    /// How long to wait between asks, by its public rate limit; zero:
    /// every tick.
    fn interval(&self) -> Duration {
        Duration::ZERO
    }

    /// Its current USD quote of each of `assets` it lists (by ticker);
    /// assets it does not list are left out.
    fn quotes<'a>(&'a self, assets: &'a [NetworkAsset])
    -> BoxFuture<'a, Result<Vec<Quote>, Error>>;
}

/// `prices` (USD, by ticker) as `market`'s quotes, answered now.
fn quoted(market: &'static str, prices: BTreeMap<String, Decimal>) -> Vec<Quote> {
    let at = ::tasks::now();
    let quotes = prices.into_iter().map(|(asset, usd)| Quote {
        market,
        asset,
        usd,
        at,
    });
    quotes.collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use anyhow::anyhow;

    use super::{BoxFuture, Market, quoted};
    use crate::models::{NetworkAsset, market::Quote, network_asset::NetworkAssetKind};

    /// A native asset of each of `tickers`.
    pub(crate) fn assets(tickers: &[&str]) -> Vec<NetworkAsset> {
        let asset = |ticker: &&str| NetworkAsset {
            asset: "native".to_owned(),
            kind: NetworkAssetKind::Native,
            contract: None,
            ticker: (*ticker).to_owned(),
            decimals: 8,
            display_decimals: None,
        };
        tickers.iter().map(asset).collect()
    }

    /// A market quoting `price` for every asset, or failing when `None`;
    /// counts its asks.
    pub(crate) struct Fixed {
        pub(crate) name: &'static str,
        pub(crate) price: Option<&'static str>,
        pub(crate) interval: Duration,
        pub(crate) asked: Arc<AtomicUsize>,
    }

    impl Fixed {
        pub(crate) fn new(name: &'static str, price: Option<&'static str>) -> Self {
            Self {
                name,
                price,
                interval: Duration::ZERO,
                asked: Arc::default(),
            }
        }
    }

    impl Market for Fixed {
        fn name(&self) -> &'static str {
            self.name
        }

        fn interval(&self) -> Duration {
            self.interval
        }

        fn quotes<'a>(
            &'a self,
            assets: &'a [NetworkAsset],
        ) -> BoxFuture<'a, Result<Vec<Quote>, anyhow::Error>> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let price: base::Decimal =
                    self.price.ok_or_else(|| anyhow!("HTTP 451"))?.parse()?;
                let prices = assets
                    .iter()
                    .map(|asset| (asset.ticker.clone(), price.clone()));
                Ok(quoted(self.name, prices.collect()))
            })
        }
    }
}
