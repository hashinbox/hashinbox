//! Coinbase Advanced Trade's public market data:
//! `GET /api/v3/brokerage/market/products?product_ids=BTC-USD&...`, every
//! asset in one request (public calls: 10 a second). USD markets,
//! stablecoins too; a product it does not list is left out of its answer.

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

/// The products endpoint.
const URL: &str = "https://api.coinbase.com/api/v3/brokerage/market/products";

/// Coinbase spot prices.
pub(crate) struct Coinbase {
    client: reqwest::Client,
}

impl Coinbase {
    pub(crate) const fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

/// `ticker`'s market: `BTC-USD`.
fn product(ticker: &str) -> String {
    format!("{ticker}-USD")
}

impl Market for Coinbase {
    fn name(&self) -> &'static str {
        "coinbase"
    }

    fn quotes<'a>(
        &'a self,
        assets: &'a [NetworkAsset],
    ) -> BoxFuture<'a, Result<Vec<Quote>, Error>> {
        Box::pin(async move {
            let query: Vec<(&str, String)> = assets
                .iter()
                .map(|asset| ("product_ids", product(&asset.ticker)))
                .collect();
            if query.is_empty() {
                return Ok(Vec::new());
            }
            Ok(quoted(
                self.name(),
                parse(&get(&self.client, URL, &query).await?, assets)?,
            ))
        })
    }
}

/// `{"products": [{"product_id": "BTC-USD", "price": "64000.5", ...}]}`, by
/// the ticker of each of `assets`.
fn parse(body: &Value, assets: &[NetworkAsset]) -> Result<BTreeMap<String, Decimal>, Error> {
    let products = body["products"]
        .as_array()
        .ok_or_else(|| anyhow!("no products"))?;
    let mut prices = BTreeMap::new();
    for row in products {
        let Some(asset) = assets
            .iter()
            .find(|asset| row["product_id"] == product(&asset.ticker))
        else {
            continue;
        };
        prices.insert(asset.ticker.clone(), decimal_of(&row["price"])?);
    }
    Ok(prices)
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::usecase::market::tests::assets;

    #[test]
    fn parses_the_price_of_each_product() {
        let body = serde_json::from_str(include_str!("fixtures/coinbase.json")).unwrap();
        let prices = parse(&body, &assets(&["BTC", "USDT", "SOL"])).unwrap();
        assert_eq!(prices.len(), 2, "SOL not answered");
        assert_eq!(prices["BTC"].to_string(), "60249.5");
        assert_eq!(prices["USDT"].to_string(), "0.99879");
    }
}
