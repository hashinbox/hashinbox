//! Every network's RPC endpoints, in the server's `network_endpoint` table
//! of the one database file (seeded by `seed.sql`).

use std::collections::BTreeMap;

use anyhow::Context as _;
use base::{Chain, Network};
use diesel::{prelude::*, sqlite::Sqlite, upsert::excluded};
use serde::Serialize;

use super::schema::network_endpoint;

/// The most endpoints one network may list.
const MOST: usize = 16;

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
#[derive(Clone, Debug)]
pub(crate) struct RpcEndpoints(Vec<rpc::Endpoint>);

impl RpcEndpoints {
    /// Every endpoint checked, in order; asks no node.
    ///
    /// # Errors
    ///
    /// A message naming the position of the first unusable endpoint, or the
    /// list's size when it is empty or longer than [`MOST`].
    pub(crate) fn new(endpoints: Vec<rpc::Endpoint>) -> Result<Self, String> {
        if endpoints.is_empty() || endpoints.len() > MOST {
            return Err(format!("a network lists between 1 and {MOST} RPC nodes"));
        }
        for (position, endpoint) in endpoints.iter().enumerate() {
            endpoint
                .validate()
                .map_err(|error| format!("RPC node {position}: {error}"))?;
        }
        Ok(Self(endpoints))
    }

    /// Each one's shown form, in failover order; one whose URL does not
    /// parse is left out.
    pub(crate) fn shown(&self) -> Vec<RpcNode> {
        let views = self.0.iter().filter_map(|endpoint| {
            Some((endpoint.url.clone(), endpoint.parts()?, endpoint.bounds))
        });
        views
            .enumerate()
            .map(|(index, view)| RpcNode::new(index, view))
            .collect()
    }

    /// The endpoints, in failover order.
    pub(crate) fn nodes(&self) -> &[rpc::Endpoint] {
        &self.0
    }
}

/// Every network's stored endpoints.
pub(crate) struct NetworkEndpoints(BTreeMap<Network, RpcEndpoints>);

impl NetworkEndpoints {
    /// Every network's list.
    pub(crate) fn rows(&self) -> &BTreeMap<Network, RpcEndpoints> {
        &self.0
    }

    /// `network`'s endpoints, in failover order; none when it has no row.
    pub(crate) fn nodes(&self, network: &Network) -> Vec<rpc::Endpoint> {
        self.0
            .get(network)
            .map(|endpoints| endpoints.0.clone())
            .unwrap_or_default()
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

        /// The network and its endpoints; a refusal names the network.
        fn try_from(row: Row) -> Result<Self, String> {
            let endpoints = serde_json::from_str::<Vec<rpc::Endpoint>>(&row.nodes)
                .map_err(|_| "stored RPC nodes are not a JSON list of nodes".to_owned())
                .and_then(RpcEndpoints::new)
                .map_err(|reason| format!("{}/{}: {reason}", row.chain, row.network))?;
            Ok((Network::new(Chain::new(&row.chain), row.network), endpoints))
        }
    }

    /// Makes `endpoints` the stored ones of `network`.
    ///
    /// # Errors
    ///
    /// When the endpoints do not encode or the write fails.
    pub(crate) fn upsert(
        connection: &mut SqliteConnection,
        network: &Network,
        endpoints: &RpcEndpoints,
    ) -> anyhow::Result<()> {
        let nodes = serde_json::to_string(&endpoints.0).context("RPC nodes did not encode")?;
        diesel::insert_into(network_endpoint::table)
            .values((
                network_endpoint::chain.eq(network.chain.as_str()),
                network_endpoint::network.eq(&network.name),
                network_endpoint::nodes.eq(nodes),
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
    /// The read, or a row that no longer checks.
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
