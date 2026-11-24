//! Every network's routed methods, `network_endpoint_route` rows: a method
//! spread round robin over some of the network's endpoints; every other
//! method goes to all of them, highest tip first (`usecase::Transport`).

use std::collections::BTreeSet;

use super::{Endpoints, schema::network_endpoint_route};
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
    pub(crate) fn new(routes: Vec<Route>, endpoints: &Endpoints) -> Result<Self, String> {
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

/// The `network_endpoint_route` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// Makes `routes` the stored ones of the network whose nodes are
    /// `endpoint_ids`.
    ///
    /// # Errors
    ///
    /// When a write fails.
    pub(crate) fn replace(
        connection: &mut SqliteConnection,
        endpoint_ids: &[i64],
        routes: &Routes,
    ) -> anyhow::Result<()> {
        diesel::delete(network_endpoint_route::table)
            .filter(network_endpoint_route::endpoint_id.eq_any(endpoint_ids))
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
}
