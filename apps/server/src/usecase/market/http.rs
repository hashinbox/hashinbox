//! What the markets share: their HTTP client and reading a JSON price.

use std::time::Duration;

use anyhow::{Error, anyhow};
use base::Decimal;
use serde_json::Value;

/// How long one request may take: less than a tick.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The client every market shares: a [`TIMEOUT`] per request and a generic
/// user agent (Coinbase refuses requests without one).
///
/// # Errors
///
/// When the TLS backend does not start.
pub(crate) fn client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent("payment-sdk-market")
        .build()
}

/// `GET url?query`'s JSON body; its status recorded on the current span
/// (the market job's `market` span: `status`).
///
/// # Errors
///
/// When the request fails, its status is not 2xx (a geo-block's 451 or
/// 403, a rate limit's 429, ...), or its body is not JSON.
pub(crate) async fn get(
    client: &reqwest::Client,
    url: &str,
    query: &[(&str, String)],
) -> Result<Value, Error> {
    let response = client
        .get(url)
        .query(query)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    let status = response.status();
    tracing::Span::current().record("status", status.as_u16());
    if !status.is_success() {
        return Err(anyhow!("HTTP {status}"));
    }
    Ok(response.json().await.map_err(reqwest::Error::without_url)?)
}

/// A price as a JSON string (`"64000.5"`) or number (`64000.5`), as
/// written.
///
/// # Errors
///
/// When it is neither, or not plain decimal text.
pub(crate) fn decimal_of(value: &Value) -> Result<Decimal, Error> {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => return Err(anyhow!("{value} is not a price")),
    };
    text.parse().map_err(|_| anyhow!("{text} is not a decimal"))
}
