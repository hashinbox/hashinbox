use std::{fmt, sync::Arc};

use json_rpc::{Endpoint, Failover, Secrets};
use serde::Deserialize;

use crate::{Error, ErrorKind, Repetition};

/// The composed transport a chain's RPC client runs on.
///
/// Whether it retries and how many nodes it has are data, not a Rust type, so
/// every chain holds the same handle whatever its deployment configured.
pub type Transport = Arc<dyn json_rpc::Client>;

/// One chain's RPC settings: the nodes, and how much the transport repeats.
///
/// Each [`json_rpc::Endpoint`] describes one node on its own — its provider's
/// URL, its headers and its bounds — so two failover nodes can carry two
/// different API keys (`a retired design note (023)` section 3.4). A node written as a bare
/// string is that URL with the default bounds and no added header. A `${NAME}`
/// anywhere in a URL or a header value stands for a value the process supplies
/// through [`json_rpc::Secrets`]; no secret is ever written in a configuration
/// file.
///
/// The fields are public: a deployment that reads a document deserializes
/// this, and one that has its URLs in hand writes
/// `Rpc { repeat: Repetition::Off,..Rpc::endpoint(url) }`. `Debug` is written
/// rather than derived, for the reason `json_rpc::Endpoint`'s is: a URL may
/// carry a credential inline, and a value that prints itself ends up in a log.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rpc {
    /// Every node of the failover list, in the order they are tried.
    pub endpoints: Vec<Endpoint>,
    /// How much one call is repeated over that list.
    #[serde(default)]
    pub repeat: Repetition,
}

impl Rpc {
    /// A failover list, with the transport's default repetition.
    #[must_use]
    pub fn new(endpoints: Vec<Endpoint>) -> Self {
        Self {
            endpoints,
            repeat: Repetition::default(),
        }
    }

    /// The common case: one node at `url`, with the transport's default
    /// repetition, bounds and no added header.
    #[must_use]
    pub fn endpoint(url: impl Into<String>) -> Self {
        Self::new(vec![Endpoint::new(url)])
    }

    /// Rejects an unusable endpoint list before anything is opened; the
    /// message names no URL.
    ///
    /// A value built in code reaches exactly the checks a parsed one does:
    /// nothing here runs inside a `Deserialize` implementation.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Endpoints`] when the list is empty or a URL is not a
    /// usable JSON-RPC endpoint, and [`ErrorKind::Repeat`] when the
    /// repetition bounds contradict each other.
    pub fn validate(&self) -> Result<(), Error> {
        if self.endpoints.is_empty() {
            return Err(Error::new(
                ErrorKind::Endpoints,
                "invalid database or RPC configuration",
            ));
        }
        for endpoint in &self.endpoints {
            endpoint.validate()?;
        }
        self.repeat.validate()
    }

    /// How many nodes this chain may reach.
    #[must_use]
    pub const fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    /// The composed transport: one `Http` per endpoint, ordered failover over
    /// them, and the configured repetition around that.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Endpoints`] when a node is unusable or one of its
    /// `${NAME}` references has no value in `secrets`.
    pub fn transport(&self, secrets: &dyn Secrets) -> Result<Transport, Error> {
        self.monitored(secrets).map(|(transport, _)| transport)
    }

    /// Builds the transport and exposes each endpoint's statistics in configured order.
    /// Handles contain no endpoint URL or credentials.
    ///
    /// # Errors
    /// The same endpoint and secret validation failures as [`Self::transport`].
    pub fn monitored(
        &self,
        secrets: &dyn Secrets,
    ) -> Result<(Transport, Vec<json_rpc::Statistics>), Error> {
        let mut clients = Vec::with_capacity(self.endpoints.len());
        let mut statistics = Vec::with_capacity(self.endpoints.len());
        for endpoint in &self.endpoints {
            let client = endpoint.connect(secrets)?;
            statistics.push(client.statistics());
            clients.push(client);
        }
        Ok((self.repeat.wrap(Failover::new(clients)?), statistics))
    }
}

impl fmt::Debug for Rpc {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Rpc")
            .field("endpoints", &self.endpoints.len())
            .field("repeat", &self.repeat)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{Value, json};

    use super::*;
    use crate::{Repetition, RepetitionPolicy};

    fn secrets<const N: usize>(values: [(&str, &str); N]) -> BTreeMap<String, String> {
        values
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect()
    }

    fn parse(value: Value) -> Result<Rpc, Error> {
        let config: Rpc = serde_json::from_value(value)
            .map_err(|error| Error::new(ErrorKind::Endpoints, error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// The crate derives no `Serialize` — an endpoint must not be able to
    /// print itself — so the parity test spells the document out by hand.
    fn document(rpc: &Rpc) -> Value {
        let repeat = match &rpc.repeat {
            Repetition::Off => json!("off"),
            Repetition::Policy(policy) => json!({
                "attempts": policy.attempts.get(),
                "backoff_ms": u64::try_from(policy.backoff.as_millis()).unwrap_or(u64::MAX),
                "max_backoff_ms": u64::try_from(policy.max_backoff.as_millis()).unwrap_or(u64::MAX),
            }),
        };
        let endpoints = rpc
            .endpoints
            .iter()
            .map(|endpoint| Value::String(endpoint.url.clone()))
            .collect::<Vec<_>>();
        json!({"endpoints": endpoints, "repeat": repeat})
    }

    /// The defect this shape fixes: one shared header map could not give two
    /// failover nodes two different API keys — and a provider that puts its
    /// key in the URL is now an ordinary endpoint rather than a rejected one.
    #[test]
    fn every_endpoint_carries_its_own_credential() {
        let config = parse(json!({"endpoints": [
            "https://primary.invalid/v3/${PRIMARY_KEY}?apikey=${PRIMARY_KEY}",
            {"url": "https://backup.invalid/v1",
             "headers": {"X-API-Key": "${BACKUP_KEY}"},
             "bounds": {"timeout_ms": 5000}},
        ]}))
        .expect("both nodes are supported");
        assert_eq!(config.endpoint_count(), 2);
        assert_eq!(config.endpoints[1].bounds.timeout_ms, 5_000);
        let values = secrets([("PRIMARY_KEY", "one"), ("BACKUP_KEY", "two")]);
        config.transport(&values).expect("the transport composes");

        let error = config
            .transport(&secrets([("PRIMARY_KEY", "one")]))
            .err()
            .expect("the second endpoint's variable is unset");
        let message = error.to_string();
        assert_eq!(
            message,
            "a configured endpoint's environment variable is unavailable"
        );
        assert!(!message.contains("BACKUP_KEY"));
        assert!(!message.contains("backup.invalid"));
    }

    #[test]
    fn an_unusable_endpoint_is_rejected_without_echoing_it() {
        for (endpoint, expected) in [
            (
                json!("ws://node.invalid"),
                "JSON-RPC endpoint scheme must be http or https",
            ),
            (
                json!("http://node.invalid/?apikey=s3cret#section"),
                "JSON-RPC endpoint must not carry a fragment",
            ),
            (
                json!({"url": "http://node.invalid/", "headers": {"bad header": "s3cret"}}),
                "JSON-RPC header name is invalid",
            ),
            (
                json!({"url": "http://node.invalid/", "bounds": {"timeout_ms": 0}}),
                "JSON-RPC endpoint `timeout_ms` must be a positive integer",
            ),
        ] {
            let error =
                parse(json!({"endpoints": [endpoint]})).expect_err("a bad node fails validation");
            assert_eq!(error.to_string(), expected);
            assert_eq!(error.kind, ErrorKind::Endpoints);
            assert!(!error.to_string().contains("s3cret"));
        }
        assert_eq!(
            parse(json!({"endpoints": []}))
                .expect_err("a chain needs a node")
                .to_string(),
            "invalid database or RPC configuration"
        );
    }

    /// A provider's own query string is the provider's: it is neither claimed
    /// by this crate nor dropped on the way to the node.
    #[test]
    fn a_providers_query_string_is_kept_rather_than_read_as_configuration() {
        let config = parse(json!({"endpoints": [
            "https://node.invalid/v3/project?apikey=k3y&auth=token",
        ]}))
        .expect("a provider URL is an ordinary endpoint");
        assert_eq!(
            config.endpoints[0].url,
            "https://node.invalid/v3/project?apikey=k3y&auth=token"
        );
        assert_eq!(config.endpoints[0].bounds, json_rpc::Bounds::default());
        assert!(config.endpoints[0].headers.is_empty());
    }

    #[test]
    fn the_rpc_wire_contract_rejects_what_belongs_to_one_node() {
        for (raw, expected) in [
            (
                r#"{"endpoints":["a"],"endpoints":["b"]}"#,
                "duplicate field `endpoints`",
            ),
            (
                r#"{"endpoints":["a"],"endpoint":"b"}"#,
                "unknown field `endpoint`",
            ),
            (
                r#"{"endpoints":["a"],"headers":[]}"#,
                "unknown field `headers`",
            ),
            (
                r#"{"endpoints":["a"],"timeout_seconds":1}"#,
                "unknown field `timeout_seconds`",
            ),
            (
                r#"{"endpoints":[{"url":"http://a","timeout_ms":1}]}"#,
                "unknown field `timeout_ms`",
            ),
            (r#"{"endpoints":[{"headers":{}}]}"#, "missing field `url`"),
            (r#"{"endpoints":null}"#, "invalid type: null"),
            (r#"{"endpoints":"http://a"}"#, "invalid type: string"),
        ] {
            serde_json::from_str::<Value>(raw).expect("syntactically valid raw JSON");
            let error = serde_json::from_str::<Rpc>(raw).expect_err("strict RPC schema");
            assert!(error.to_string().starts_with(expected), "{error}");
        }
    }

    /// The point of the types being the API: a value assembled in Rust and the
    /// same value read from a document are indistinguishable, defaults
    /// included, and reach the identical `validate` — no check hides inside a
    /// `Deserialize` implementation.
    #[test]
    fn a_value_built_in_code_equals_the_parsed_one_and_validates_identically() {
        let url = "http://127.0.0.1:8545";
        let built = Rpc::endpoint(url);
        let parsed: Rpc = serde_json::from_value(json!({"endpoints": [url]})).expect("one node");
        assert_eq!(built, parsed, "an absent `repeat` is the constructor's own");
        assert_eq!(built.repeat, Repetition::Policy(RepetitionPolicy::new()));

        let explicit = Rpc {
            repeat: Repetition::Off,
            ..Rpc::new(vec![
                Endpoint::new(url),
                Endpoint::new("http://127.0.0.1:8546"),
            ])
        };
        let written: Rpc = serde_json::from_value(json!({
            "endpoints": [url, "http://127.0.0.1:8546"], "repeat": "off"
        }))
        .expect("two nodes, no repetition");
        assert_eq!(explicit, written);
        assert_eq!(explicit.endpoint_count(), 2);

        for broken in [
            Rpc::new(Vec::new()),
            Rpc::endpoint("ws://node.invalid"),
            Rpc {
                repeat: Repetition::Policy(RepetitionPolicy {
                    backoff: std::time::Duration::from_secs(2),
                    max_backoff: std::time::Duration::from_millis(250),
                    ..RepetitionPolicy::new()
                }),
                ..Rpc::endpoint(url)
            },
        ] {
            let coded = broken.validate().expect_err("an invalid value in code");
            let read: Rpc = serde_json::from_value(document(&broken)).expect("the same document");
            let read = read.validate().expect_err("and the same failure");
            assert_eq!((coded.kind, coded.message), (read.kind, read.message));
        }
    }
}
