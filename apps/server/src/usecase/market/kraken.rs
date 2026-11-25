//! Kraken spot: `GET /0/public/Ticker?pair=XXBTZUSD,...`, every asset in
//! one request (public calls: about one a second). USD markets, stablecoins
//! too; the price is the last trade's (`c[0]`).

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

/// The ticker endpoint.
const URL: &str = "https://api.kraken.com/0/public/Ticker";

/// Kraken spot prices.
pub(crate) struct Kraken {
    client: reqwest::Client,
}

impl Kraken {
    pub(crate) const fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

/// `ticker`'s market, as Kraken names it in its answer too (it refuses the
/// whole request for one it does not list, so only these are asked).
fn pair(ticker: &str) -> Option<&'static str> {
    Some(match ticker {
        "BTC" => "XXBTZUSD",
        "ETH" => "XETHZUSD",
        "SOL" => "SOLUSD",
        "USDT" => "USDTZUSD",
        "USDC" => "USDCUSD",
        "DAI" => "DAIUSD",
        _ => return None,
    })
}

impl Market for Kraken {
    fn name(&self) -> &'static str {
        "kraken"
    }

    fn quotes<'a>(
        &'a self,
        assets: &'a [NetworkAsset],
    ) -> BoxFuture<'a, Result<Vec<Quote>, Error>> {
        Box::pin(async move {
            let markets: Vec<(&str, &str)> = assets
                .iter()
                .filter_map(|asset| Some((pair(&asset.ticker)?, asset.ticker.as_str())))
                .collect();
            if markets.is_empty() {
                return Ok(Vec::new());
            }
            let pairs: Vec<&str> = markets.iter().map(|(pair, _)| *pair).collect();
            let query = [("pair", pairs.join(","))];
            Ok(quoted(
                self.name(),
                parse(&get(&self.client, URL, &query).await?, &markets)?,
            ))
        })
    }
}

/// `{"error": [], "result": {"XXBTZUSD": {"c": ["64000.5", "0.1"], ...}}}`,
/// by the ticker of each of `markets` (`(pair, ticker)`).
fn parse(body: &Value, markets: &[(&str, &str)]) -> Result<BTreeMap<String, Decimal>, Error> {
    if let Some(errors) = body["error"].as_array().filter(|errors| !errors.is_empty()) {
        return Err(anyhow!("{errors:?}"));
    }
    let mut prices = BTreeMap::new();
    for (pair, ticker) in markets {
        let last = &body["result"][pair]["c"][0];
        if last.is_null() {
            continue;
        }
        prices.insert((*ticker).to_owned(), decimal_of(last)?);
    }
    Ok(prices)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_the_last_trade_of_each_pair() {
        let body = serde_json::from_str(include_str!("fixtures/kraken.json")).unwrap();
        let markets = [
            ("XXBTZUSD", "BTC"),
            ("USDTZUSD", "USDT"),
            ("NOPEUSD", "NOPE"),
        ];
        let prices = parse(&body, &markets).unwrap();
        assert_eq!(prices.len(), 2, "NOPE not answered");
        assert_eq!(prices["BTC"].to_string(), "60255.1");
        assert_eq!(prices["USDT"].to_string(), "0.99878");
    }

    #[test]
    fn an_error_answer_is_an_error() {
        let body = serde_json::json!({"error": ["EQuery:Unknown asset pair"]});
        let error = parse(&body, &[("XXBTZUSD", "BTC")]).unwrap_err();
        assert!(error.to_string().contains("Unknown asset pair"), "{error}");
    }
}
