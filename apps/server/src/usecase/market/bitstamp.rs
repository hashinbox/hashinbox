//! Bitstamp: `GET /api/v2/ticker/`, every market in one request (public
//! calls: 400 a second). USD markets, stablecoins too; the price is the
//! last trade's (`last`).

use std::collections::BTreeMap;

use anyhow::{Error, anyhow};
use base::Decimal;
use serde_json::Value;

use super::{
    BoxFuture, Market,
    http::{decimal_of, get},
    quoted,
};
use crate::models::{NetworkAsset, market::Quote};

/// The every-market ticker endpoint.
const URL: &str = "https://www.bitstamp.net/api/v2/ticker/";

/// Bitstamp spot prices.
pub(crate) struct Bitstamp {
    client: reqwest::Client,
}

impl Bitstamp {
    pub(crate) const fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

/// `ticker`'s market: `BTC/USD`.
fn pair(ticker: &str) -> String {
    format!("{ticker}/USD")
}

impl Market for Bitstamp {
    fn name(&self) -> &'static str {
        "bitstamp"
    }

    fn quotes<'a>(
        &'a self,
        assets: &'a [NetworkAsset],
    ) -> BoxFuture<'a, Result<Vec<Quote>, Error>> {
        Box::pin(async move {
            if assets.is_empty() {
                return Ok(Vec::new());
            }
            Ok(quoted(
                self.name(),
                parse(&get(&self.client, URL, &[]).await?, assets)?,
            ))
        })
    }
}

/// `[{"pair": "BTC/USD", "last": "64000.5", ...}, ...]`, by the ticker of
/// each of `assets`.
fn parse(body: &Value, assets: &[NetworkAsset]) -> Result<BTreeMap<String, Decimal>, Error> {
    let markets = body.as_array().ok_or_else(|| anyhow!("not a list"))?;
    let mut prices = BTreeMap::new();
    for row in markets {
        let Some(asset) = assets
            .iter()
            .find(|asset| row["pair"] == pair(&asset.ticker))
        else {
            continue;
        };
        prices.insert(asset.ticker.clone(), decimal_of(&row["last"])?);
    }
    Ok(prices)
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::usecase::market::tests::assets;

    #[test]
    fn parses_the_last_trade_of_each_usd_market() {
        let body = serde_json::from_str(include_str!("fixtures/bitstamp.json")).unwrap();
        let prices = parse(&body, &assets(&["BTC", "USDT", "DAI"])).unwrap();
        assert_eq!(prices.len(), 2, "DAI not listed; BTC/EUR not USD");
        assert_eq!(prices["BTC"].to_string(), "60282.88");
        assert_eq!(prices["USDT"].to_string(), "0.99874");
    }
}
