//! A network's stored nodes and routes, connected while the server runs.

use crate::{
    db::Db,
    models::{Endpoint, Route, endpoint, endpoint_route},
};

/// Connects `network` to its stored nodes and routes; returns both. Asks no
/// node.
///
/// # Errors
///
/// The read, or what `Payments::connect` refuses.
pub(crate) async fn connect(
    db: &Db,
    payments: &payments::Payments,
    network: &base::Network,
) -> anyhow::Result<(Vec<Endpoint>, Vec<Route>)> {
    let key = network.clone();
    let (endpoints, routes) = db
        .read(move |connection| {
            let endpoints = endpoint::db::find_all(connection)?.nodes(&key);
            let routes = endpoint_route::db::find_all(connection)?.routes(&key);
            Ok((endpoints, routes))
        })
        .await?;
    payments.connect(network, &endpoint_route::nodes(&endpoints, &routes)?)?;
    Ok((endpoints, routes))
}
