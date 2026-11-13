//! Every network's RPC nodes, one `network_endpoint` row each, in failover
//! order (seeded by `seed.sql`).

use std::collections::BTreeMap;

use super::schema::network_endpoint;
use base::{Chain, Network};
use diesel::{prelude::*, sqlite::Sqlite, upsert::excluded};
use serde::Serialize;

/// The most endpoints one network may list.
const MOST: usize = 16;

/// One stored node of a network.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = RpcNode)]
pub(crate) struct Endpoint {
    /// Its id: what a route and removal name.
    pub(crate) id: i64,
    /// Its URL as stored.
    #[schema(value_type = String)]
    pub(crate) url: rpc::Endpoint,
    /// What the running server knows of it; always present, null where it
    /// is not running (its network is not, or its URL did not build).
    #[schema(required)]
    pub(crate) state: Option<NodeState>,
}

/// What the running server knows of one node: its last tip, its failure
/// streak, and its calls since it was loaded. Nothing asks the node.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[schema(as = RpcNodeState)]
pub(crate) struct NodeState {
    /// Its newest block (slot) when last asked, kept for the network's block
    /// time; always present, null before it told one.
    #[schema(required)]
    pub(crate) tip: Option<u64>,
    /// Retryable failures in a row (a rate limit counts as 3); from 3 it is
    /// tried after every other node until it answers, or 10 block times (at
    /// least 30 s) pass.
    pub(crate) failures: u32,
    /// Calls that reached it and finished.
    pub(crate) calls: u64,
    /// Of those, the ones that failed.
    pub(crate) failed_calls: u64,
    /// The last failure's kind or code, never a message of the node's;
    /// always present, null: none.
    #[schema(required)]
    pub(crate) last_failure: Option<String>,
}

/// The URLs of `endpoints`, in their order.
pub(crate) fn urls(endpoints: &[Endpoint]) -> Vec<rpc::Endpoint> {
    endpoints
        .iter()
        .map(|endpoint| endpoint.url.clone())
        .collect()
}

/// A network's node URLs in failover order: one to [`MOST`], each once.
#[derive(Clone, Debug)]
pub(crate) struct RpcEndpoints(Vec<rpc::Endpoint>);

impl RpcEndpoints {
    /// Every endpoint checked, in order; asks no node.
    ///
    /// # Errors
    ///
    /// A message naming the position of the first unusable or repeated
    /// endpoint, or the list's size when it is empty or longer than [`MOST`].
    pub(crate) fn new(endpoints: Vec<rpc::Endpoint>) -> Result<Self, String> {
        if endpoints.is_empty() || endpoints.len() > MOST {
            return Err(format!("a network lists between 1 and {MOST} RPC nodes"));
        }
        for (position, endpoint) in endpoints.iter().enumerate() {
            endpoint
                .validate()
                .map_err(|error| format!("RPC node {position}: {error}"))?;
            if endpoints[..position].iter().any(|e| e.url == endpoint.url) {
                return Err(format!("RPC node {position} is listed twice"));
            }
        }
        Ok(Self(endpoints))
    }

    /// The endpoints, in failover order.
    pub(crate) fn nodes(&self) -> &[rpc::Endpoint] {
        &self.0
    }
}

/// Every network's stored endpoints.
pub(crate) struct NetworkEndpoints(BTreeMap<Network, Vec<Endpoint>>);

impl NetworkEndpoints {
    /// `network`'s endpoints, in failover order; none when it has no row.
    pub(crate) fn nodes(&self, network: &Network) -> Vec<Endpoint> {
        self.0.get(network).cloned().unwrap_or_default()
    }
}

/// The `network_endpoint` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// One `network_endpoint` row as the file holds it.
    #[derive(Queryable, Selectable)]
    #[diesel(table_name = network_endpoint, check_for_backend(Sqlite))]
    struct Row {
        id: i64,
        chain: String,
        network: String,
        url: String,
    }

    /// Makes `endpoints` the stored ones of `network`, in their order: a
    /// URL already stored keeps its id (and routes), one no longer listed is
    /// deleted with its routes.
    ///
    /// # Errors
    ///
    /// When a write fails.
    pub(crate) fn replace(
        connection: &mut SqliteConnection,
        network: &Network,
        endpoints: &RpcEndpoints,
    ) -> anyhow::Result<()> {
        let of_network = network_endpoint::chain
            .eq(network.chain.as_str())
            .and(network_endpoint::network.eq(&network.name));
        let urls: Vec<&str> = endpoints.0.iter().map(|e| e.url.as_str()).collect();
        diesel::delete(network_endpoint::table)
            .filter(of_network)
            .filter(network_endpoint::url.ne_all(&urls))
            .execute(connection)?;
        for (position, url) in urls.iter().enumerate() {
            diesel::insert_into(network_endpoint::table)
                .values((
                    network_endpoint::chain.eq(network.chain.as_str()),
                    network_endpoint::network.eq(&network.name),
                    network_endpoint::url.eq(url),
                    network_endpoint::position.eq(i64::try_from(position)?),
                ))
                .on_conflict((
                    network_endpoint::chain,
                    network_endpoint::network,
                    network_endpoint::url,
                ))
                .do_update()
                .set(network_endpoint::position.eq(excluded(network_endpoint::position)))
                .execute(connection)?;
        }
        Ok(())
    }

    /// The stored endpoints of every network, in failover order.
    ///
    /// # Errors
    ///
    /// The read.
    pub(crate) fn find_all(connection: &mut SqliteConnection) -> anyhow::Result<NetworkEndpoints> {
        let rows = network_endpoint::table
            .select(Row::as_select())
            .order((
                network_endpoint::chain,
                network_endpoint::network,
                network_endpoint::position,
                network_endpoint::id,
            ))
            .load(connection)?;
        let mut networks: BTreeMap<Network, Vec<Endpoint>> = BTreeMap::new();
        for row in rows {
            let network = Network::new(Chain::new(&row.chain), row.network);
            networks.entry(network).or_default().push(Endpoint {
                id: row.id,
                url: rpc::Endpoint::new(row.url),
                state: None,
            });
        }
        Ok(NetworkEndpoints(networks))
    }
}
