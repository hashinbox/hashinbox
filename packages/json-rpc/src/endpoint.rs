use std::{fmt, time::Duration};

use serde::{Deserialize, Deserializer, de};
use url::Url;

use crate::{Error, ErrorKind, Http};

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
/// parameters (API keys). It reaches the node unaltered.
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
        let url = Self::check_url(&self.url)?;
        self.bounds.validate()?;
        Http::build(url.as_str(), self.redacted(), self.bounds)
    }

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
