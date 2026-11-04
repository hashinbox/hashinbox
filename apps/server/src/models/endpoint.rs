//! Every network's RPC endpoints, in the server's `network_endpoint` table
//! of the one database file (seeded by `seed.sql`).

use std::{collections::BTreeMap, fmt};

use anyhow::Context as _;
use base::{Chain, Network};
use diesel::{prelude::*, sqlite::Sqlite, upsert::excluded};
use serde::Serialize;
use serde_json::Value;

use super::schema::network_endpoint;

/// The most endpoints one network may list.
const MOST: usize = 16;

/// One configured RPC endpoint: the document a configuration file's
/// `rpc.endpoints` holds — a URL, or an object with the URL and bounds —
/// checked as `rpc::Endpoint` checks it.

#[derive(Clone)]
pub(crate) struct Endpoint {
    document: Value,
    /// The node the document describes.
    endpoint: rpc::Endpoint,
    /// The URL's scheme, host and port; `unresolved` when it does not parse.
    origin: String,
}

impl Endpoint {
    /// Checks `document` without contacting a node.
    ///
    /// # Errors
    ///
    /// One that is neither a URL nor an endpoint object, or one
    /// `rpc::Endpoint::validate` refuses.
    pub(crate) fn new(document: Value) -> Result<Self, String> {
        let endpoint: rpc::Endpoint = serde_json::from_value(document.clone())
            .map_err(|_| "an RPC node is a URL or an object of url and bounds")?;
        endpoint.validate().map_err(|error| error.to_string())?;
        let origin = url::Url::parse(&endpoint.url).map_or_else(
            |_| "unresolved".to_owned(),
            |url| url.origin().ascii_serialization(),
        );
        Ok(Self {
            document,
            endpoint,
            origin,
        })
    }

    /// Its URL, the URL's parts ([`rpc::Endpoint::parts`]) and its bounds;
    /// `None` when its URL does not parse.
    fn view(&self) -> Option<(String, rpc::Parts, rpc::Bounds)> {
        Some((
            self.endpoint.url.clone(),
            self.endpoint.parts()?,
            self.endpoint.bounds,
        ))
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.origin)
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("Endpoint")
            .field(&self.origin)
            .finish()
    }
}

/// One query parameter of a node's URL.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct RpcParameter {
    /// The parameter's name, or for a `header.<Name>` one, the header's.
    name: String,
    value: String,
    /// Whether every call sends it as a header instead of in the URL.
    header: bool,
}

/// One RPC node: its URL, parsed.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct RpcNode {
    /// Its position in failover order; what removing it takes.
    index: usize,
    /// The URL as stored.
    url: String,
    /// Scheme, host and port.
    origin: String,
    scheme: String,
    host: String,
    /// The port, the scheme's default when unwritten; always present.
    #[schema(required)]
    port: Option<u16>,
    path: String,
    /// The user of `user:password`, sent as HTTP Basic; empty when none.
    username: String,
    /// Always present.
    #[schema(required)]
    password: Option<String>,
    /// Its query parameters, in order.
    parameters: Vec<RpcParameter>,
    /// Longest one call may take, in milliseconds.
    timeout_ms: u64,
    /// Largest request body, in bytes.
    max_request_bytes: u32,
    /// Largest response body, in bytes.
    max_response_bytes: u32,
}

impl RpcNode {
    fn new(index: usize, (url, parts, bounds): (String, rpc::Parts, rpc::Bounds)) -> Self {
        Self {
            index,
            url,
            origin: parts.origin,
            scheme: parts.scheme,
            host: parts.host,
            port: parts.port,
            path: parts.path,
            username: parts.username,
            password: parts.password,
            parameters: parts
                .parameters
                .into_iter()
                .map(|parameter| RpcParameter {
                    name: parameter.name,
                    value: parameter.value,
                    header: parameter.header,
                })
                .collect(),
            timeout_ms: bounds.timeout_ms,
            max_request_bytes: bounds.max_request_bytes,
            max_response_bytes: bounds.max_response_bytes,
        }
    }
}

/// One row of `network_endpoint`: a network's endpoints in failover order, one
/// to [`MOST`] of them. The store's operations are this row's.
#[derive(Clone)]
pub(crate) struct RpcEndpoints(Vec<Endpoint>);

impl RpcEndpoints {
    /// Every document checked, in order.
    ///
    /// # Errors
    ///
    /// A message naming the position of the first unusable endpoint and never
    /// its contents, or the list's size when it is empty or longer than
    /// [`MOST`].
    pub(crate) fn new(documents: Vec<Value>) -> Result<Self, String> {
        if documents.is_empty() || documents.len() > MOST {
            return Err(format!("a network lists between 1 and {MOST} RPC nodes"));
        }
        let endpoints = documents
            .into_iter()
            .enumerate()
            .map(|(position, document)| {
                Endpoint::new(document).map_err(|reason| format!("RPC node {position}: {reason}"))
            });
        endpoints.collect::<Result<_, _>>().map(Self)
    }

    /// The documents, in failover order, for storage and for the section a
    /// chain parses them in.
    pub(crate) fn documents(&self) -> Vec<Value> {
        self.0
            .iter()
            .map(|endpoint| endpoint.document.clone())
            .collect()
    }

    /// Each one's shown form, in failover order; one whose URL does not
    /// parse is left out.
    pub(crate) fn shown(&self) -> Vec<RpcNode> {
        let views = self.0.iter().filter_map(Endpoint::view).enumerate();
        views
            .map(|(index, view)| RpcNode::new(index, view))
            .collect()
    }

    /// The nodes the documents describe, in failover order, for a transport.
    pub(crate) fn nodes(&self) -> Vec<rpc::Endpoint> {
        self.0
            .iter()
            .map(|endpoint| endpoint.endpoint.clone())
            .collect()
    }
}

impl fmt::Debug for RpcEndpoints {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_list().entries(&self.0).finish()
    }
}

/// Every network's stored endpoints.
pub(crate) struct NetworkEndpoints(BTreeMap<Network, RpcEndpoints>);

impl NetworkEndpoints {
    /// Every network's list.
    pub(crate) fn rows(&self) -> &BTreeMap<Network, RpcEndpoints> {
        &self.0
    }

    /// Every network's documents, in failover order, for composition.
    pub(crate) fn documents(&self) -> BTreeMap<Network, Vec<Value>> {
        self.0
            .iter()
            .map(|(network, endpoints)| (network.clone(), endpoints.documents()))
            .collect()
    }
}

/// The `network_endpoint` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// One `network_endpoint` row as the file holds it, before it is checked.
    #[derive(Queryable, Selectable)]
    #[diesel(table_name = network_endpoint, check_for_backend(Sqlite))]
    struct Row {
        chain: String,
        network: String,
        nodes: String,
    }

    impl TryFrom<Row> for (Network, RpcEndpoints) {
        type Error = String;

        /// The network and its endpoints; a refusal names the network and never
        /// quotes the stored documents.
        fn try_from(row: Row) -> Result<Self, String> {
            let endpoints = serde_json::from_str::<Vec<Value>>(&row.nodes)
                .map_err(|_| "stored RPC nodes are not a JSON list".to_owned())
                .and_then(RpcEndpoints::new)
                .map_err(|reason| format!("{}/{}: {reason}", row.chain, row.network))?;
            Ok((Network::new(Chain::new(&row.chain), row.network), endpoints))
        }
    }

    /// Makes `endpoints` the stored ones of `network`.
    ///
    /// # Errors
    ///
    /// When the documents do not encode or the write fails.
    pub(crate) fn upsert(
        connection: &mut SqliteConnection,
        network: &Network,
        endpoints: &RpcEndpoints,
    ) -> anyhow::Result<()> {
        let documents =
            serde_json::to_string(&endpoints.documents()).context("RPC nodes did not encode")?;
        diesel::insert_into(network_endpoint::table)
            .values((
                network_endpoint::chain.eq(network.chain.as_str()),
                network_endpoint::network.eq(&network.name),
                network_endpoint::nodes.eq(documents),
            ))
            .on_conflict((network_endpoint::chain, network_endpoint::network))
            .do_update()
            .set(network_endpoint::nodes.eq(excluded(network_endpoint::nodes)))
            .execute(connection)?;
        Ok(())
    }

    /// The stored endpoints of every network, one row each.
    ///
    /// # Errors
    ///
    /// The read, or a row that no longer checks; the refusal never quotes it.
    pub(crate) fn find_all(connection: &mut SqliteConnection) -> anyhow::Result<NetworkEndpoints> {
        let rows = network_endpoint::table
            .select(Row::as_select())
            .load(connection)?;
        rows.into_iter()
            .map(|row| row.try_into().map_err(anyhow::Error::msg))
            .collect::<anyhow::Result<_>>()
            .map(NetworkEndpoints)
    }
}
