//! Every network's RPC nodes, one `network_endpoint` row each, in failover
//! order (seeded by `seed.sql`), changed by verified edits. A node with its
//! counts and state is read with its network (`network::db`).

use super::{EndpointFailure, EndpointStats, schema::network_endpoint};
use base::Network;
use diesel::{prelude::*, upsert::excluded};
use serde::Serialize;

/// One stored node of a network, with its counts and state.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = RpcNode)]
pub(crate) struct Endpoint {
    /// Its id: what a route and removal name.
    pub(crate) id: i64,
    /// Its URL as stored.
    #[schema(value_type = String)]
    pub(crate) url: rpc::Endpoint,
    /// Its failover position, lowest first.
    pub(crate) position: i64,
    /// Its calls.
    pub(crate) stats: EndpointStats,
    /// Its newest record while that is not an enable: what disabled it, its
    /// `message` why. Always present, null: enabled, the transport uses it.
    #[schema(required)]
    pub(crate) disabled: Option<EndpointFailure>,
}

collection! {
    /// A network's nodes, in failover order.
    #[derive(Clone, Debug, Default, Serialize)]
    Endpoints(Endpoint)
}

impl Endpoints {
    /// How many are enabled; 0: no call can go out.
    pub(crate) fn enabled(&self) -> usize {
        self.iter().filter(|node| node.disabled.is_none()).count()
    }

    /// Their ids, in order.
    pub(crate) fn ids(&self) -> Vec<i64> {
        self.iter().map(|node| node.id).collect()
    }

    /// Their URLs, in order.
    pub(crate) fn urls(&self) -> Vec<rpc::Endpoint> {
        self.iter().map(|node| node.url.clone()).collect()
    }
}

/// The most endpoints one network may list.
const MOST: usize = 16;

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

/// The `network_endpoint` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

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
}
