use std::{fmt, sync::Arc};

use json_rpc::{Endpoint, Failover};
use serde::Deserialize;

use crate::config::{Error, ErrorKind, Repetition};

/// The composed transport a chain's RPC client runs on.
///
/// Whether it retries and how many nodes it has are data, not a Rust type, so
/// every chain holds the same handle whatever its deployment configured.
pub type Transport = Arc<dyn json_rpc::Client>;

/// One chain's RPC settings: the nodes, and how much the transport repeats.
///
/// Each [`json_rpc::Endpoint`] is one node's URL, carrying its own
/// credentials (userinfo, query keys), and its bounds. A node written as a
/// bare string is that URL with the default bounds.
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
    /// repetition and bounds.
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
    /// [`ErrorKind::Endpoints`] when a node is unusable.
    pub fn transport(&self) -> Result<Transport, Error> {
        self.monitored().map(|(transport, _)| transport)
    }

    /// Builds the transport and exposes each endpoint's statistics in configured order.
    /// Handles contain no endpoint URL or credentials.
    ///
    /// # Errors
    /// The same endpoint validation failures as [`Self::transport`].
    pub fn monitored(&self) -> Result<(Transport, Vec<json_rpc::Statistics>), Error> {
        self.logged(&logging::Logger::default())
    }

    /// Builds each node with an explicitly injected, network-scoped logger.
    /// Node IDs are endpoint ordinals within this immutable configuration.
    ///
    /// # Errors
    /// The same configuration failures as [`Self::transport`].
    pub fn logged(
        &self,
        logger: &logging::Logger,
    ) -> Result<(Transport, Vec<json_rpc::Statistics>), Error> {
        let mut clients = Vec::with_capacity(self.endpoints.len());
        let mut statistics = Vec::with_capacity(self.endpoints.len());
        for (ordinal, endpoint) in self.endpoints.iter().enumerate() {
            let client = endpoint.connect()?.with_logger(
                logger
                    .with("component", "rpc")
                    .with("node_id", ordinal.to_string()),
            );
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
