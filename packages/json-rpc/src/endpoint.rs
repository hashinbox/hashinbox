use std::{fmt, time::Duration};

use serde::{Deserialize, Deserializer, de};
use url::Url;

use std::collections::HashMap;

use jsonrpsee::http_client::HeaderMap;

use crate::{Error, ErrorKind, Http};

/// The query parameter prefix naming a static header.
const HEADER: &str = "header.";

/// What one call to one node may take and carry: 15 s, 10 MiB requests,
/// 64 MiB responses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bounds {
    /// How long one execution may take.
    pub timeout_ms: u64,
    /// The largest request body this node accepts.
    pub max_request_bytes: u32,
    /// The largest response body this node may return.
    pub max_response_bytes: u32,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            timeout_ms: 15_000,
            max_request_bytes: 10 * 1024 * 1024,
            max_response_bytes: 64 * 1024 * 1024,
        }
    }
}

impl Bounds {
    pub(crate) const fn timeout(self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    /// Rejects a bound that would make every call fail.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when a bound is zero; the message
    /// names the bound and never a URL.
    pub fn validate(&self) -> Result<(), Error> {
        for (name, value) in [
            ("timeout_ms", self.timeout_ms),
            ("max_request_bytes", u64::from(self.max_request_bytes)),
            ("max_response_bytes", u64::from(self.max_response_bytes)),
        ] {
            if value == 0 {
                return Err(Error::new(
                    ErrorKind::InvalidConfiguration,
                    format!("JSON-RPC endpoint `{name}` must be a positive integer"),
                ));
            }
        }
        Ok(())
    }
}

/// One node: its URL, and the [`Bounds`] one call runs under.
///
/// The URL carries everything, like a database URI: scheme, host, port, path,
/// `user:password` userinfo (sent as HTTP Basic authorization) and query
/// parameters (API keys). A query parameter named `header.<Name>` is not sent
/// in the URL: every call carries it as the static header `<Name>` instead
/// (`?header.x-api-key=KEY`). The rest reaches the node unaltered.
///
/// A deployment document writes either the URL alone or the full object:
///
/// ```json
/// ["https://node.invalid/v3/KEY",
///  {"url": "http://user:pass@node.invalid/", "bounds": {"timeout_ms": 20000}}]
/// ```
///
/// `Debug` and `Display` show [`Self::redacted`], never userinfo or query values.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The node's URL: `http` or `https`, a host, and no fragment.
    pub url: String,
    /// What one call may take and carry.
    pub bounds: Bounds,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Endpoint")
            .field("url", &self.redacted())
            .field("bounds", &self.bounds)
            .finish()
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.redacted())
    }
}

impl Endpoint {
    /// One node at `url`, with the default bounds.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            bounds: Bounds::default(),
        }
    }

    /// The same node, under `bounds`.
    #[must_use]
    pub const fn with_bounds(mut self, bounds: Bounds) -> Self {
        self.bounds = bounds;
        self
    }

    /// The URL safe to show: userinfo becomes `***` and every query value
    /// becomes `***`; `invalid URL` when it does not parse.
    #[must_use]
    pub fn redacted(&self) -> String {
        let Ok(mut url) = Url::parse(&self.url) else {
            return "invalid URL".to_owned();
        };
        if !url.username().is_empty() || url.password().is_some() {
            // Fails only for URLs that cannot carry userinfo, which then has none.
            let _ = url.set_username("***");
            let _ = url.set_password(None);
        }
        if url.query().is_some() {
            let keys: Vec<String> = url.query_pairs().map(|(key, _)| key.into_owned()).collect();
            url.query_pairs_mut()
                .clear()
                .extend_pairs(keys.iter().map(|key| (key.as_str(), "***")));
        }
        url.into()
    }

    /// Rejects an unusable node.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when the URL is not an `http` or
    /// `https` URL naming a host, when it carries a fragment, or when a bound
    /// is zero. No message echoes the URL.
    pub fn validate(&self) -> Result<(), Error> {
        Self::check_url(&self.url)?;
        self.bounds.validate()
    }

    /// The client for this node; does no I/O.
    ///
    /// # Errors
    ///
    /// As [`Self::validate`].
    pub fn connect(&self) -> Result<Http, Error> {
        let mut url = Self::check_url(&self.url)?;
        self.bounds.validate()?;
        let headers = Self::headers(&url)?;
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| !key.starts_with(HEADER))
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        if kept.is_empty() {
            url.set_query(None);
        } else {
            url.query_pairs_mut().clear().extend_pairs(kept);
        }
        Http::build(url.as_str(), self.redacted(), self.bounds, headers)
    }

    /// The static headers `url`'s `header.` parameters name.
    fn headers(url: &Url) -> Result<HeaderMap, Error> {
        let mut named = HashMap::new();
        for (key, value) in url.query_pairs() {
            let Some(name) = key.strip_prefix(HEADER) else {
                continue;
            };
            if named
                .insert(name.to_ascii_lowercase(), value.into_owned())
                .is_some()
            {
                return Err(Self::invalid("JSON-RPC endpoint names a header twice"));
            }
        }
        HeaderMap::try_from(&named)
            .map_err(|_| Self::invalid("JSON-RPC endpoint header is invalid"))
    }

    /// The URL, with its `header.` parameters checked.
    fn check_url(text: &str) -> Result<Url, Error> {
        let url =
            Url::parse(text).map_err(|_| Self::invalid("JSON-RPC endpoint URL is invalid"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Self::invalid(
                "JSON-RPC endpoint scheme must be http or https",
            ));
        }
        if url.host_str().is_none() {
            return Err(Self::invalid("JSON-RPC endpoint must name a host"));
        }
        if url.fragment().is_some() {
            return Err(Self::invalid("JSON-RPC endpoint must not carry a fragment"));
        }
        Self::headers(&url)?;
        Ok(url)
    }

    fn invalid(message: &'static str) -> Error {
        Error::new(ErrorKind::InvalidConfiguration, message)
    }
}

/// The URL alone, or the object naming its bounds.
impl<'de> Deserialize<'de> for Endpoint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(Form)
    }
}

/// The two forms a deployment document writes one node in.
// linter:disable rust/empty-struct -- serde's visitor seam: the endpoint it builds is its state.
struct Form;

impl Form {
    const FIELDS: [&'static str; 2] = ["url", "bounds"];
}

impl<'de> de::Visitor<'de> for Form {
    type Value = Endpoint;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a node URL, or an object with `url` and `bounds`")
    }

    fn visit_str<E: de::Error>(self, url: &str) -> Result<Endpoint, E> {
        Ok(Endpoint::new(url))
    }

    fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<Endpoint, M::Error> {
        let mut url = None;
        let mut bounds = None;
        while let Some(key) = map.next_key::<String>()? {
            let field = Self::FIELDS
                .iter()
                .find(|name| **name == key)
                .ok_or_else(|| de::Error::unknown_field(&key, &Self::FIELDS))?;
            let written = match *field {
                "url" => url.replace(map.next_value()?).is_some(),
                _ => bounds.replace(map.next_value()?).is_some(),
            };
            if written {
                return Err(de::Error::duplicate_field(field));
            }
        }
        Ok(Endpoint {
            url: url.ok_or_else(|| de::Error::missing_field("url"))?,
            bounds: bounds.unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Endpoint;

    #[test]
    fn header_parameters_become_headers_and_stay_redacted() {
        let endpoint = Endpoint::new("https://node.invalid/v1?apikey=A&header.x-api-key=B");
        let url = url::Url::parse(&endpoint.url).unwrap();
        let headers = Endpoint::headers(&url).unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers["x-api-key"], "B");
        assert!(endpoint.connect().is_ok());
        assert_eq!(
            endpoint.redacted(),
            "https://node.invalid/v1?apikey=***&header.x-api-key=***"
        );
        for bad in [
            "https://node.invalid/?header.x-key=1&header.X-Key=2",
            "https://node.invalid/?header.bad%20name=1",
            "https://node.invalid/?header.x-key=%0A",
        ] {
            assert!(Endpoint::new(bad).validate().is_err(), "{bad}");
        }
    }
}
