use std::{collections::BTreeMap, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{Error, ErrorKind};

/// The query parameter prefix naming a static header.
const HEADER: &str = "header.";

/// The query parameter prefix of this client's own settings for the node,
/// never sent to it; each layer of [`crate::node`] reads its own params.
const SETTING: &str = "rpc.";

/// A node URL, parsed: what reaches the node, and every query parameter.
///
/// ```
/// let url = rpc::Url::parse("https://node.invalid/v1?key=K&rpc.max_batch=1").unwrap();
/// assert_eq!(url.node.as_str(), "https://node.invalid/v1?key=K");
/// assert_eq!(url.params.get_int::<u32>("rpc.max_batch").unwrap(), Some(1));
/// assert_eq!(url.params.namespace("rpc").get_int::<u32>("max_batch").unwrap(), Some(1));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    /// The URL as sent: without its `header.` and `rpc.` parameters.
    pub node: url::Url,
    /// Every query parameter, by name.
    pub params: Params,
}

impl Url {
    /// `text` parsed; an `http` or `https` URL naming a host, with no
    /// fragment, naming no parameter twice.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] otherwise.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let mut node =
            url::Url::parse(text).map_err(|_| invalid("JSON-RPC endpoint URL is invalid"))?;
        if !matches!(node.scheme(), "http" | "https") {
            return Err(invalid("JSON-RPC endpoint scheme must be http or https"));
        }
        if node.host_str().is_none() {
            return Err(invalid("JSON-RPC endpoint must name a host"));
        }
        if node.fragment().is_some() {
            return Err(invalid("JSON-RPC endpoint must not carry a fragment"));
        }
        let mut params = Params::default();
        for (key, value) in node.query_pairs() {
            if params
                .values
                .insert(key.to_string(), value.into_owned())
                .is_some()
            {
                return Err(invalid(format!("JSON-RPC endpoint names `{key}` twice")));
            }
        }
        let kept: Vec<(String, String)> = node
            .query_pairs()
            .filter(|(key, _)| !key.starts_with(HEADER) && !key.starts_with(SETTING))
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        if kept.is_empty() {
            node.set_query(None);
        } else {
            node.query_pairs_mut().clear().extend_pairs(kept);
        }
        Ok(Self { node, params })
    }
}

/// Query parameters by name, or those under one prefix ([`Self::namespace`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Params {
    /// What [`Self::namespace`] stripped, so errors name the parameter.
    prefix: String,
    values: BTreeMap<String, String>,
}

impl Params {
    /// The value of `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// The value of `name` as a number; `None` when absent.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when it is not a `T`.
    pub fn get_int<T: FromStr>(&self, name: &str) -> Result<Option<T>, Error> {
        let Some(value) = self.get(name) else {
            return Ok(None);
        };
        let parsed = value.parse().map_err(|_| {
            invalid(format!(
                "JSON-RPC endpoint `{}{name}` must be a positive integer",
                self.prefix
            ))
        })?;
        Ok(Some(parsed))
    }

    /// The parameters named `<prefix>.<name>`, by `<name>`.
    #[must_use]
    pub fn namespace(&self, prefix: &str) -> Self {
        let prefix = format!("{}{prefix}.", self.prefix);
        let values = self
            .iter()
            .filter_map(|(name, value)| {
                let name = format!("{}{name}", self.prefix);
                Some((name.strip_prefix(&prefix)?.to_owned(), value.to_owned()))
            })
            .collect();
        Self { prefix, values }
    }

    /// Every parameter: name and value, by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Refuses a parameter whose name is not in `known`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] naming the first unknown one.
    pub fn only(&self, known: &[&str]) -> Result<(), Error> {
        let Some((name, _)) = self.iter().find(|(name, _)| !known.contains(name)) else {
            return Ok(());
        };
        Err(invalid(format!(
            "JSON-RPC endpoint names an unknown setting `{}{name}`",
            self.prefix
        )))
    }
}

/// One node: its URL. Serializes as the URL.
///
/// The URL carries everything, like a database URI: scheme, host, port, path,
/// `user:password` userinfo (sent as HTTP Basic authorization) and query
/// parameters (API keys). A query parameter named `header.<Name>` is not sent
/// in the URL: every call carries it as the static header `<Name>` instead
/// (`?header.x-api-key=KEY`). One named `rpc.<setting>` is not sent either:
/// it sets how this client talks to the node ([`crate::HttpParams`],
/// [`crate::SplitParams`]). The rest reaches
/// the node unaltered. [`crate::node`] builds the client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Endpoint {
    /// The node's URL: `http` or `https`, a host, and no fragment.
    pub url: String,
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.url)
    }
}

impl Endpoint {
    /// One node at `url`.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }

    /// The URL parsed ([`Url::parse`]); its params are read by their owners.
    ///
    /// # Errors
    ///
    /// As [`Url::parse`].
    pub fn parse(&self) -> Result<Url, Error> {
        Url::parse(&self.url)
    }

    /// The node's identity: `host[:port]/path` and the query sent to it,
    /// without scheme, userinfo, `header.*` or `rpc.*` params. Providers put
    /// the network or key in the path, so different paths are different
    /// nodes. A URL that does not parse is its own id.
    #[must_use]
    pub fn id(&self) -> String {
        let Ok(url) = self.parse() else {
            return self.url.clone();
        };
        let node = url.node;
        let host = node.host_str().unwrap_or_default();
        let port = node
            .port()
            .map(|port| format!(":{port}"))
            .unwrap_or_default();
        let query = node
            .query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default();
        format!("{host}{port}{}{query}", node.path())
    }

    /// Rejects an unusable node: builds its client ([`crate::node`]), which
    /// does no I/O, so every param is read by the layer that owns it.
    ///
    /// # Errors
    ///
    /// As [`crate::node`].
    pub fn validate(&self) -> Result<(), Error> {
        crate::node(self, logging::Logger::default()).map(drop)
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::transport(ErrorKind::InvalidConfiguration, message)
}

#[cfg(test)]
mod tests {
    use super::Endpoint;

    #[test]
    fn the_id_is_host_port_path_and_the_query_sent() {
        let id = |url: &str| Endpoint::new(url).id();
        assert_eq!(
            id(
                "https://user:pass@node.invalid:8545/v1/KEY?apikey=A&header.x-key=B&rpc.max_batch=1"
            ),
            "node.invalid:8545/v1/KEY?apikey=A"
        );
        assert_eq!(id("http://node.invalid"), "node.invalid/");
        assert_eq!(id("https://node.invalid:443/a"), "node.invalid/a");
        assert_ne!(id("https://node.invalid/a"), id("https://node.invalid/b"));
        assert_eq!(
            id("http://node.invalid/?rpc.timeout_ms=1"),
            id("https://node.invalid/")
        );

        let endpoint = Endpoint::new("http://node.invalid/");
        let node = crate::Retry::new(crate::node(&endpoint, Default::default()).unwrap());
        assert_eq!(crate::Client::endpoint(&node), Some(&endpoint));
    }

    #[test]
    fn header_parameters_become_headers() {
        let endpoint = Endpoint::new("https://node.invalid/v1?apikey=A&header.x-api-key=B");
        let url = endpoint.parse().unwrap();
        let headers = crate::HttpParams::try_from(&url).unwrap().headers;
        assert_eq!(headers.len(), 1);
        assert_eq!(headers["x-api-key"], "B");
        assert_eq!(url.node.as_str(), "https://node.invalid/v1?apikey=A");
        for bad in [
            "https://node.invalid/?header.x-key=1&header.X-Key=2",
            "https://node.invalid/?header.bad%20name=1",
            "https://node.invalid/?header.x-key=%0A",
        ] {
            assert!(Endpoint::new(bad).validate().is_err(), "{bad}");
        }
    }
}
