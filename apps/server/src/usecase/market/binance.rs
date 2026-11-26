//! Binance spot, its market-data-only host (not geo-blocked like
//! `api.binance.com`): `GET /api/v3/ticker/price?symbols=[...]`, every
//! asset in one request (weight 4 of 6000 a minute). It has no USD
//! markets: a USDT market counts as USD 1:1 for a non-stable asset, and a
//! stablecoin (USDT, USDC, DAI: priced against USD only, so its peg is
//! measured, not assumed) is left out.

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

/// The prices endpoint.
const URL: &str = "https://data-api.binance.vision/api/v3/ticker/price";

/// Binance spot prices.
pub(crate) struct Binance {
    client: reqwest::Client,
}

impl Binance {
    pub(crate) const fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

/// `ticker`'s market (it refuses the whole request for one it does not
/// list, so only these are asked).
fn symbol(ticker: &str) -> Option<&'static str> {
    Some(match ticker {
        "BTC" => "BTCUSDT",
        "ETH" => "ETHUSDT",
        "SOL" => "SOLUSDT",
        "BNB" => "BNBUSDT",
        "TRX" => "TRXUSDT",
        _ => return None,
    })
}

impl Market for Binance {
    fn name(&self) -> &'static str {
        "binance"
    }

    fn quotes<'a>(
        &'a self,
        assets: &'a [NetworkAsset],
    ) -> BoxFuture<'a, Result<Vec<Quote>, Error>> {
        Box::pin(async move {
            let markets: Vec<(&str, &str)> = assets
                .iter()
                .filter_map(|asset| Some((symbol(&asset.ticker)?, asset.ticker.as_str())))
                .collect();
            if markets.is_empty() {
                return Ok(Vec::new());
            }
            let symbols: Vec<&str> = markets.iter().map(|(symbol, _)| *symbol).collect();
            let query = [("symbols", serde_json::to_string(&symbols)?)];
            Ok(quoted(
                self.name(),
                parse(&get(&self.client, URL, &query).await?, &markets)?,
            ))
        })
    }
}

/// `[{"symbol": "BTCUSDT", "price": "64000.5"}, ...]`, by the ticker of
/// each of `markets` (`(symbol, ticker)`).
fn parse(body: &Value, markets: &[(&str, &str)]) -> Result<BTreeMap<String, Decimal>, Error> {
    let mut prices = BTreeMap::new();
    for row in body.as_array().ok_or_else(|| anyhow!("not a list"))? {
        let Some((_, ticker)) = markets.iter().find(|(symbol, _)| row["symbol"] == *symbol) else {
            continue;
        };
        prices.insert((*ticker).to_owned(), decimal_of(&row["price"])?);
    }
    Ok(prices)
}

#[cfg(test)]
mod tests {
    use super::{parse, symbol};

    #[test]
    fn parses_the_prices_of_the_markets_asked() {
        let body = serde_json::from_str(include_str!("fixtures/binance.json")).unwrap();
        let prices = parse(&body, &[("BTCUSDT", "BTC"), ("ETHUSDT", "ETH")]).unwrap();
        assert_eq!(prices.len(), 2, "SOL not asked");
        assert_eq!(prices["BTC"].to_string(), "60348.01");
        assert_eq!(prices["ETH"].to_string(), "1697.61");
    }

    #[test]
    fn a_stablecoin_has_no_market() {
        assert_eq!(symbol("BTC"), Some("BTCUSDT"));
        assert_eq!(symbol("USDT"), None);
        assert_eq!(symbol("USDC"), None);
    }
}
