//! CoinGecko's keyless public API:
//! `GET /api/v3/simple/price?ids=bitcoin,...&vs_currencies=usd`, every
//! asset in one request. Its keyless limit is a few calls a minute, so it
//! is asked at most once a [`INTERVAL`]. An aggregate of many exchanges, in
//! USD, stablecoins too.

use std::{collections::BTreeMap, time::Duration};

use anyhow::{Error, anyhow};
use base::Decimal;
use serde_json::Value;

use super::{
    BoxFuture, Market,
    http::{decimal_of, get},
    quoted,
};
use crate::models::{NetworkAsset, market::Quote};

/// The prices endpoint.
const URL: &str = "https://api.coingecko.com/api/v3/simple/price";

/// The least time between two asks: its keyless rate limit.
const INTERVAL: Duration = Duration::from_secs(60);

/// CoinGecko's aggregate prices.
pub(crate) struct CoinGecko {
    client: reqwest::Client,
}

impl CoinGecko {
    pub(crate) const fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

/// `ticker`'s coin id.
fn id(ticker: &str) -> Option<&'static str> {
    Some(match ticker {
        "BTC" => "bitcoin",
        "ETH" => "ethereum",
        "SOL" => "solana",
        "BNB" => "binancecoin",
        "TRX" => "tron",
        "USDT" => "tether",
        "USDC" => "usd-coin",
        "DAI" => "dai",
        _ => return None,
    })
}

impl Market for CoinGecko {
    fn name(&self) -> &'static str {
        "coingecko"
    }

    fn interval(&self) -> Duration {
        INTERVAL
    }

    fn quotes<'a>(
        &'a self,
        assets: &'a [NetworkAsset],
    ) -> BoxFuture<'a, Result<Vec<Quote>, Error>> {
        Box::pin(async move {
            let coins: Vec<(&str, &str)> = assets
                .iter()
                .filter_map(|asset| Some((id(&asset.ticker)?, asset.ticker.as_str())))
                .collect();
            if coins.is_empty() {
                return Ok(Vec::new());
            }
            let ids: Vec<&str> = coins.iter().map(|(id, _)| *id).collect();
            let query = [("ids", ids.join(",")), ("vs_currencies", "usd".to_owned())];
            Ok(quoted(
                self.name(),
                parse(&get(&self.client, URL, &query).await?, &coins)?,
            ))
        })
    }
}

/// `{"bitcoin": {"usd": 64000.5}, ...}`, by the ticker of each of `coins`
/// (`(id, ticker)`).
fn parse(body: &Value, coins: &[(&str, &str)]) -> Result<BTreeMap<String, Decimal>, Error> {
    if !body.is_object() {
        return Err(anyhow!("not an object"));
    }
    let mut prices = BTreeMap::new();
    for (id, ticker) in coins {
        let usd = &body[id]["usd"];
        if usd.is_null() {
            continue;
        }
        prices.insert((*ticker).to_owned(), decimal_of(usd)?);
    }
    Ok(prices)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_the_usd_price_of_each_coin() {
        let body = serde_json::from_str(include_str!("fixtures/coingecko.json")).unwrap();
        let coins = [("bitcoin", "BTC"), ("usd-coin", "USDC"), ("nope", "NOPE")];
        let prices = parse(&body, &coins).unwrap();
        assert_eq!(prices.len(), 2, "nope not answered");
        assert_eq!(prices["BTC"].to_string(), "60191");
        assert_eq!(prices["USDC"].to_string(), "0.999586");
    }
}
