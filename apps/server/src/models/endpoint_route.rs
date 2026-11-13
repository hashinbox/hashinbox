//! Every network's routed methods, `network_endpoint_route` rows: a method
//! spread round robin over some of the network's endpoints (`rpc::Router`);
//! every other method goes to the cluster of all of them.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    endpoint::Endpoint,
    schema::{network_endpoint, network_endpoint_route},
};
use base::{Chain, Network};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

/// One method and the endpoints taking turns serving it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = RpcRoute)]
#[serde(deny_unknown_fields)]
pub(crate) struct Route {
    /// The JSON-RPC method, exactly as sent, e.g. `getBlock`.
    pub(crate) method: String,
    /// The network's endpoints taking turns, by id.
    pub(crate) endpoint_ids: Vec<i64>,
}

/// `endpoints` with `routes`, as `payments` connects them: positions in
/// `endpoints` in place of ids; an id not among them is left out.
///
/// # Errors
///
/// A route left with no endpoint.
pub(crate) fn nodes(endpoints: &[Endpoint], routes: &[Route]) -> anyhow::Result<payments::Nodes> {
    let mut nodes = payments::Nodes::new(super::endpoint::urls(endpoints));
    for route in routes {
        let positions = route
            .endpoint_ids
            .iter()
            .filter_map(|id| endpoints.iter().position(|endpoint| endpoint.id == *id));
        nodes = nodes.route(route.method.clone(), positions)?;
    }
    Ok(nodes)
}

/// A network's routes, checked against its endpoints.
#[derive(Clone, Debug)]
pub(crate) struct Routes(Vec<Route>);

impl Routes {
    /// Every route checked: a non-empty method, routed once, over one or
    /// more of `endpoints`, each named once.
    ///
    /// # Errors
    ///
    /// A message naming the first route that does not check.
    pub(crate) fn new(routes: Vec<Route>, endpoints: &[Endpoint]) -> Result<Self, String> {
        let mut methods = BTreeSet::new();
        for route in &routes {
            let method = &route.method;
            if method.trim().is_empty() {
                return Err("a route's method is empty".to_owned());
            }
            if !methods.insert(method) {
                return Err(format!("{method} is routed twice"));
            }
            if route.endpoint_ids.is_empty() {
                return Err(format!("{method} is routed to no node"));
            }
            let ids: BTreeSet<_> = route.endpoint_ids.iter().collect();
            if ids.len() != route.endpoint_ids.len() {
                return Err(format!("{method} names a node twice"));
            }
            if let Some(id) = ids
                .iter()
                .find(|id| !endpoints.iter().any(|e| e.id == ***id))
            {
                return Err(format!("{method}: no node {id} in this network"));
            }
        }
        Ok(Self(routes))
    }
}

/// Every network's stored routes.
pub(crate) struct NetworkRoutes(BTreeMap<Network, Vec<Route>>);

impl NetworkRoutes {
    /// `network`'s routes, by method; none when it has none.
    pub(crate) fn routes(&self, network: &Network) -> Vec<Route> {
        self.0.get(network).cloned().unwrap_or_default()
    }
}

/// The `network_endpoint_route` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// Makes `routes` the stored ones of `network`.
    ///
    /// # Errors
    ///
    /// When a write fails.
    pub(crate) fn replace(
        connection: &mut SqliteConnection,
        network: &Network,
        routes: &Routes,
    ) -> anyhow::Result<()> {
        let ids = network_endpoint::table
            .filter(network_endpoint::chain.eq(network.chain.as_str()))
            .filter(network_endpoint::network.eq(&network.name))
            .select(network_endpoint::id);
        diesel::delete(network_endpoint_route::table)
            .filter(network_endpoint_route::endpoint_id.eq_any(ids))
            .execute(connection)?;
        for route in &routes.0 {
            for id in &route.endpoint_ids {
                diesel::insert_into(network_endpoint_route::table)
                    .values((
                        network_endpoint_route::endpoint_id.eq(id),
                        network_endpoint_route::method.eq(&route.method),
                    ))
                    .execute(connection)?;
            }
        }
        Ok(())
    }

    /// The stored routes of every network, by method, each one's endpoints
    /// in failover order.
    ///
    /// # Errors
    ///
    /// The read.
    pub(crate) fn find_all(connection: &mut SqliteConnection) -> anyhow::Result<NetworkRoutes> {
        let rows: Vec<(String, String, String, i64)> = network_endpoint_route::table
            .inner_join(network_endpoint::table)
            .select((
                network_endpoint::chain,
                network_endpoint::network,
                network_endpoint_route::method,
                network_endpoint::id,
            ))
            .order((
                network_endpoint::chain,
                network_endpoint::network,
                network_endpoint_route::method,
                network_endpoint::position,
                network_endpoint::id,
            ))
            .load(connection)?;
        let mut networks: BTreeMap<Network, Vec<Route>> = BTreeMap::new();
        for (chain, name, method, id) in rows {
            let routes = networks
                .entry(Network::new(Chain::new(&chain), name))
                .or_default();
            match routes.last_mut() {
                Some(route) if route.method == method => route.endpoint_ids.push(id),
                _ => routes.push(Route {
                    method,
                    endpoint_ids: vec![id],
                }),
            }
        }
        Ok(NetworkRoutes(networks))
    }
}
