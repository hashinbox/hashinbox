use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    time::Duration,
};

use jsonrpsee::http_client::{HeaderMap, HeaderValue};
use serde::{Deserialize, Deserializer, de};
use url::Url;

use crate::{Error, ErrorKind, Http};

/// The values an [`Endpoint`]'s `${NAME}` references stand for.
///
/// This crate reads no environment of its own: `linter.toml` allows
/// `std::env::var` only in the composition root, and the workspace's
/// `unsafe_code = "forbid"` makes `std::env::set_var` — and therefore an
/// environment-reading crate's own tests — impossible here. The caller supplies
/// the values, exactly as `apps/server/src/storage.rs` supplies a database URL.
pub trait Secrets: Send + Sync {
    /// The value of `name`, or `None` when it is not set.
    fn value(&self, name: &str) -> Option<String>;
}

/// A prepared map of values, for tests and for callers that hold them already.
impl Secrets for BTreeMap<String, String> {
    fn value(&self, name: &str) -> Option<String> {
        self.get(name).cloned()
    }
}

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

/// One node: the URL its provider publishes, the headers every call to it
/// carries, and the [`Bounds`] one call runs under.
///
/// The URL is the provider's and reaches the node unaltered — path and query
/// included, so `https://host/v3/<key>`, `?apikey=<key>` and `?auth=<token>`
/// are all ordinary endpoints. This crate claims no parameter of its own; what
/// the deployment chooses is a named field beside the URL, never a word
/// smuggled into it.
///
/// A value never holds a secret. It holds a `${NAME}` reference
/// (`[A-Z_][A-Z0-9_]*`) that [`Self::connect`] looks up — anywhere in the URL
/// and in any header value. A value resolved into the URL may hold only
/// unreserved characters and `+=`: one carrying a delimiter is rejected,
/// because it would either reach the node percent-encoded or silently repoint
/// the URL. A header value has no such restriction.
///
/// A deployment document writes either the URL alone or the full object:
///
/// ```json
/// ["https://node.invalid/v3/${KEY}",
///  {"url": "https://node.invalid/",
///   "headers": {"X-API-Key": "${KEY}"},
///   "bounds": {"timeout_ms": 20000}}]
/// ```
///
/// `Debug` shows header names and bounds, never the URL or a value.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The provider's URL: `http` or `https`, a host, and no fragment.
    pub url: String,
    /// Headers every call carries, by name.
    pub headers: BTreeMap<String, String>,
    /// What one call may take and carry.
    pub bounds: Bounds,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Endpoint")
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("bounds", &self.bounds)
            .finish()
    }
}

impl Endpoint {
    /// One node at `url`, with no added header and the default bounds.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: BTreeMap::new(),
            bounds: Bounds::default(),
        }
    }

    /// The same node, with `value` sent under `name` on every call.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// The same node, under `bounds`.
    #[must_use]
    pub const fn with_bounds(mut self, bounds: Bounds) -> Self {
        self.bounds = bounds;
        self
    }

    /// Rejects an unusable node before any value is looked up.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when the URL is not an `http` or
    /// `https` URL naming a host, when it carries a fragment, when a header
    /// name is invalid or collides with another as HTTP compares them, when a
    /// `${NAME}` reference is malformed, or when a bound is zero. No message
    /// echoes a URL or a value.
    pub fn validate(&self) -> Result<(), Error> {
        Self::check_references(&self.url)?;
        Self::check_url(&Self::probe(&self.url))?;
        let mut seen = BTreeSet::new();
        for (name, value) in &self.headers {
            let name = Self::header(name)?;
            if !seen.insert(name.as_str().to_owned()) {
                return Err(Self::invalid(
                    "JSON-RPC endpoint header names collide case-insensitively",
                ));
            }
            Self::check_references(value)?;
        }
        self.bounds.validate()
    }

    /// The client for this node, with every `${NAME}` looked up in `secrets`.
    ///
    /// # Errors
    ///
    /// As [`Self::validate`], plus [`ErrorKind::InvalidConfiguration`] when a
    /// named value is unset (the name is not echoed), when a header value is
    /// invalid, or when a resolved value would have to be percent-encoded.
    pub fn connect(&self, secrets: &dyn Secrets) -> Result<Http, Error> {
        self.validate()?;
        let (text, values) = Self::substitute(&self.url, secrets)?;
        if !values.iter().all(|value| value.bytes().all(Self::is_safe)) {
            return Err(Self::invalid(
                "JSON-RPC endpoint value must not require percent-encoding",
            ));
        }
        let url = Self::check_url(&text)?;
        let mut headers = HeaderMap::new();
        for (name, value) in &self.headers {
            let name = Self::header(name)?;
            let (value, _) = Self::substitute(value, secrets)?;
            let value = HeaderValue::from_str(&value)
                .map_err(|_| Self::invalid("JSON-RPC header value is invalid"))?;
            headers.insert(name, value);
        }
        Http::build(url.as_str(), headers, self.bounds)
    }

    /// HTTP's own name, which is lowercase however the deployment wrote it.
    fn header(name: &str) -> Result<http_types::HeaderName, Error> {
        name.parse::<http_types::HeaderName>()
            .map_err(|_| Self::invalid("JSON-RPC header name is invalid"))
    }

    /// The shape every endpoint URL must have, resolved or not.
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

    /// The URL as it will look once resolved, so its shape is checked before
    /// any value is known. A reference stands for one opaque word.
    fn probe(url: &str) -> String {
        Self::rewrite(url, |_| Some("reference".to_owned())).unwrap_or_else(|_| url.to_owned())
    }

    /// Replaces every `${NAME}` and reports the values that were put in, so
    /// the caller can check that each one survived.
    fn substitute(text: &str, secrets: &dyn Secrets) -> Result<(String, Vec<String>), Error> {
        let mut values = Vec::new();
        let resolved = Self::rewrite(text, |name| {
            let value = secrets.value(name)?;
            values.push(value.clone());
            Some(value)
        })?;
        Ok((resolved, values))
    }

    /// Walks `${NAME}` references; `value` returning `None` is an unset name.
    fn rewrite(text: &str, mut value: impl FnMut(&str) -> Option<String>) -> Result<String, Error> {
        let mut resolved = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("${") {
            resolved.push_str(&rest[..start]);
            let tail = &rest[start + 2..];
            let Some(end) = tail.find('}') else {
                return Err(Self::malformed());
            };
            let Some(found) = value(&tail[..end]) else {
                return Err(Self::invalid(
                    "a configured endpoint's environment variable is unavailable",
                ));
            };
            resolved.push_str(&found);
            rest = &tail[end + 1..];
        }
        resolved.push_str(rest);
        Ok(resolved)
    }

    fn check_references(text: &str) -> Result<(), Error> {
        Self::rewrite(text, |name| {
            Self::is_name(name).then(|| "reference".to_owned())
        })
        .map(|_| ())
        .map_err(|_| Self::malformed())
    }

    /// A value put into a URL may not carry a delimiter: one that would be
    /// percent-encoded reaches the node as the encoded text, and one that
    /// would not — `@`, `/`, `?` — silently repoints the URL.
    const fn is_safe(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'=')
    }

    fn is_name(name: &str) -> bool {
        let mut bytes = name.bytes();
        bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_uppercase() || byte == b'_')
            && bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    }

    fn invalid(message: &'static str) -> Error {
        Error::new(ErrorKind::InvalidConfiguration, message)
    }

    fn malformed() -> Error {
        Self::invalid("JSON-RPC endpoint environment reference is malformed")
    }
}

/// The URL alone, or the object naming its headers and bounds.
impl<'de> Deserialize<'de> for Endpoint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(Form)
    }
}

/// The two forms a deployment document writes one node in.
// linter:disable rust/empty-struct -- serde's visitor seam: the endpoint it builds is its state.
struct Form;

impl Form {
    const FIELDS: [&'static str; 3] = ["url", "headers", "bounds"];
}

impl<'de> de::Visitor<'de> for Form {
    type Value = Endpoint;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a node URL, or an object with `url`, `headers` and `bounds`")
    }

    fn visit_str<E: de::Error>(self, url: &str) -> Result<Endpoint, E> {
        Ok(Endpoint::new(url))
    }

    fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<Endpoint, M::Error> {
        let mut url = None;
        let mut headers = None;
        let mut bounds = None;
        while let Some(key) = map.next_key::<String>()? {
            let field = Self::FIELDS
                .iter()
                .find(|name| **name == key)
                .ok_or_else(|| de::Error::unknown_field(&key, &Self::FIELDS))?;
            let written = match *field {
                "url" => url.replace(map.next_value()?).is_some(),
                "headers" => headers.replace(map.next_value()?).is_some(),
                _ => bounds.replace(map.next_value()?).is_some(),
            };
            if written {
                return Err(de::Error::duplicate_field(field));
            }
        }
        Ok(Endpoint {
            url: url.ok_or_else(|| de::Error::missing_field("url"))?,
            headers: headers.unwrap_or_default(),
            bounds: bounds.unwrap_or_default(),
        })
    }
}
