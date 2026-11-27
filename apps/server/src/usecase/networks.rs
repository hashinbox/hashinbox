//! Networks and their nodes. [`Networks`]: every stored network with its
//! nodes and routes, read in one go; registered into the container once at
//! start by its chain ([`Chains`]), a created one at once, and loaded into a
//! network's transport after every edit. Then what
//! is asked of them: every network with its live tip; a network's nodes and
//! routes, and changing them (each new node asked first, the change stored,
//! then in use); and its kept blocks and transactions.

use std::collections::BTreeMap;

use base::{Container, InfoClient, NetworkConfig, NodeInfo, ServiceFactory};
use tasks::Worker;

use super::{
    Open, Served as _, Transport, now,
    tasks::Queue,
    workers::index::{Index, Target},
};
use crate::{
    db::{Db, Page},
    error::Error,
    models::{
        Block, EndpointFailure, EndpointFailures, Endpoints, Network, Route, Routes, RpcEndpoints,
        block, endpoint, endpoint_failure, endpoint_route, network,
        transaction::{self, Transaction},
    },
};

/// Every stored network: its config, nodes and routes, as the database holds
/// them.
pub(crate) struct Networks {
    networks: Vec<Network>,
    /// Where they were read, and where the transports [`Self::register`]
    /// builds turn off and count their stored nodes.
    db: Db,
}

impl Networks {
    /// The networks of `db` with their assets, nodes, routes and index: one
    /// read.
    ///
    /// # Errors
    ///
    /// The read.
    pub(crate) async fn new(db: Db) -> anyhow::Result<Self> {
        let networks = db.read(network::db::find_all).await?;
        Ok(Self { networks, db })
    }

    /// Every indexed network of a chain in `chains`, registered by it
    /// ([`Chains`]); one of another chain is left out. Main only, once.
    pub(crate) fn register(&self, chains: &Chains, container: &Container<Transport>) {
        for network in self.networks.iter().filter(|network| network.enabled) {
            if let Some(mount) = chains.mount(&network.chain) {
                (mount.register)(container, network, self.db.clone(), mount.open);
            }
        }
    }

    /// `network`'s stored nodes and routes into its transport
    /// (`Transport::load`). No chain types: the transport kept its reader.
    ///
    /// # Errors
    ///
    /// A network that is not registered, or what the transport refuses.
    pub(crate) fn load(
        &self,
        network: &base::Network,
        container: &Container<Transport>,
    ) -> anyhow::Result<()> {
        let transport = container.transport(network)?;
        let Some(stored) = self.networks.iter().find(|stored| stored.key() == *network) else {
            return transport.load(&Endpoints::default(), &[]);
        };
        transport.load(&stored.endpoints, &stored.routes)
    }
}

/// Every chain the server runs, by name: how a network of it is checked and
/// registered, without naming its types (main names them, once).
#[derive(Default)]
pub(crate) struct Chains(Vec<Mount>);

/// One chain of [`Chains`].
struct Mount {
    chain: base::Chain,
    /// Its config read as the chain reads it (`F::new`), asking no node.
    check: fn(&NetworkConfig) -> anyhow::Result<()>,
    /// A transport read by its `I` (`Transport::new::<I>`), its stored
    /// nodes turned off and counted in the database, loaded with its nodes
    /// and routes, registered with its `F`.
    register: fn(&Container<Transport>, &Network, Db, Open),
    /// Each node's transport.
    open: Open,
}

impl Chains {
    /// `F::CHAIN` added: its nodes JSON-RPC over HTTP, read by `I`, its
    /// networks built by `F`.
    #[must_use]
    pub(crate) fn with<I: InfoClient + 'static, F: ServiceFactory>(self) -> Self {
        self.over::<I, F>(super::nodes::http)
    }

    /// [`Self::with`], each node's transport `open`'s.
    #[must_use]
    pub(crate) fn over<I: InfoClient + 'static, F: ServiceFactory>(mut self, open: Open) -> Self {
        self.0.push(Mount {
            chain: F::CHAIN,
            check: check::<F>,
            register: register::<I, F>,
            open,
        });
        self
    }

    /// The chains' names, in the order added.
    pub(crate) fn names(&self) -> Vec<String> {
        self.0.iter().map(|mount| mount.chain.to_string()).collect()
    }

    /// The chain named `chain`; none: not run here.
    fn mount(&self, chain: &str) -> Option<&Mount> {
        self.0.iter().find(|mount| mount.chain.as_str() == chain)
    }
}

/// `config` read by `F`.
fn check<F: ServiceFactory>(config: &NetworkConfig) -> anyhow::Result<()> {
    F::new(config).map(drop)
}

/// `network` registered into `container`, as [`Mount::register`] says.
fn register<I: InfoClient + 'static, F: ServiceFactory>(
    container: &Container<Transport>,
    network: &Network,
    db: Db,
    open: Open,
) {
    let config = NetworkConfig::from(network);
    let transport = Transport::new::<I>(&config, db, open);
    // Every stored URL was checked when stored; one that no longer
    // builds is left out, and the API shows it without state.
    let _ = transport.load(&network.endpoints, &network.routes);
    container.register::<F>(config, transport);
}

/// Stores `network` (its row and assets, no node yet) and registers it as
/// a start does, in use at once: its nodes are added by the endpoint
/// routes. Answers it as stored.
///
/// # Errors
///
/// Invalid for a network name that is not a slug, a chain not run here or
/// a network its chain refuses (no chain id, wrong decimals, ...); already
/// exists for a stored chain and name; the store.
pub(crate) async fn create(
    container: &Container<Transport>,
    db: &Db,
    chains: &Chains,
    network: Network,
) -> Result<Network, Error> {
    let slug = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    let name = &network.network;
    if name.is_empty() || name.len() > 64 || !name.chars().all(slug) || name.starts_with('-') {
        return Err(Error::invalid_argument(
            "a network name is 1 to 64 of a-z, 0-9 and -, not starting with -",
        ));
    }
    let Some(mount) = chains.mount(&network.chain) else {
        let known = chains.names().join(", ");
        return Err(Error::invalid_argument(format!(
            "unknown chain {}; this server runs {known}",
            network.chain
        )));
    };
    (mount.check)(&NetworkConfig::from(&network))
        .map_err(|error| Error::invalid_argument(format!("{error:#}")))?;
    let kept = network.clone();
    if !db
        .write(move |connection| network::db::insert(connection, &kept))
        .await?
    {
        return Err(Error::already_exists("the network exists already"));
    }
    let key = network.key();
    let stored = stored_network(db, &key).await?.unwrap_or(network);
    (mount.register)(container, &stored, db.clone(), mount.open);
    Ok(stored)
}

/// What the node at `endpoint` reports (its chain id and tip), asked once
/// through `network`'s reader; nothing is stored.
///
/// # Errors
///
/// Not found for an unconfigured network; invalid for a malformed URL, a
/// node that fails or one serving another chain (its chain id said).
pub(crate) async fn test_endpoint(
    container: &Container<Transport>,
    network: &base::Network,
    endpoint: &rpc::Endpoint,
) -> Result<NodeInfo, Error> {
    let transport = container.transport(network)?;
    transport
        .verify(endpoint)
        .await
        .map_err(|error| Error::invalid_argument(format!("{error:#}")))
}

/// A network's nodes as stored, in failover order, each with its state.
pub(crate) struct Nodes {
    pub(crate) network: base::Network,
    pub(crate) endpoints: Endpoints,
}

/// Every stored network, each with the highest tip its nodes last told
/// (kept in memory, `Transport::known_tip`; `None` before the first, or for
/// a network not running). Asks no node.
///
/// # Errors
///
/// The read.
pub(crate) async fn overview(
    container: &Container<Transport>,
    db: &Db,
) -> Result<Vec<(Network, Option<u64>)>, Error> {
    let networks = db.read(network::db::find_all).await?;
    let known = networks.into_iter().map(|network| async move {
        let tip = match container.transport(&network.key()) {
            Ok(transport) => transport.known_tip().await,
            Err(_) => None,
        };
        (network, tip)
    });
    Ok(futures_util::future::join_all(known).await)
}

/// `network`'s stored nodes, each with its state.
///
/// # Errors
///
/// Not found for an unconfigured network; the read.
pub(crate) async fn endpoints(
    container: &Container<Transport>,
    db: &Db,
    network: &base::Network,
) -> Result<Nodes, Error> {
    container.service(network)?;
    Ok(Nodes {
        network: network.clone(),
        endpoints: stored(db, network).await?,
    })
}

/// `network` as stored; none: not stored.
async fn stored_network(db: &Db, network: &base::Network) -> Result<Option<Network>, Error> {
    let network = network.clone();
    Ok(db
        .read(move |connection| network::db::find(connection, &network))
        .await?)
}

/// `network`'s stored nodes, in failover order; none for a network not
/// stored.
async fn stored(db: &Db, network: &base::Network) -> Result<Endpoints, Error> {
    let network = stored_network(db, network).await?;
    Ok(network.map(|network| network.endpoints).unwrap_or_default())
}

/// Appends `added` after `network`'s nodes: each asked first, then the list
/// stored and in use.
///
/// # Errors
///
/// Not found for an unconfigured network; invalid for no node, a list above
/// 16 nodes, or a node that fails or serves another chain; the store.
pub(crate) async fn add_endpoints(
    container: &Container<Transport>,
    db: &Db,
    queue: &Queue,
    network: &base::Network,
    added: Vec<rpc::Endpoint>,
) -> Result<Nodes, Error> {
    container.service(network)?;
    if added.is_empty() {
        return Err(Error::invalid_argument("add 1 to 16 nodes at once"));
    }
    let mut nodes = stored(db, network).await?.urls();
    nodes.extend(added.clone());
    let nodes = RpcEndpoints::new(nodes).map_err(Error::invalid_argument)?;
    verify(container, network, &added).await?;
    store(db, network, nodes).await?;
    connected(container, db, queue, network).await
}

/// Makes `nodes` `network`'s list: each asked first, then stored and in use.
/// A URL already stored keeps its id, routes and state; one left out goes
/// with its routes.
///
/// # Errors
///
/// Not found for an unconfigured network; invalid for a malformed list or a
/// node that fails or serves another chain; the store.
pub(crate) async fn replace_endpoints(
    container: &Container<Transport>,
    db: &Db,
    queue: &Queue,
    network: &base::Network,
    nodes: Vec<rpc::Endpoint>,
) -> Result<Nodes, Error> {
    container.service(network)?;
    let nodes = RpcEndpoints::new(nodes).map_err(Error::invalid_argument)?;
    verify(container, network, nodes.nodes()).await?;
    store(db, network, nodes).await?;
    connected(container, db, queue, network).await
}

/// Appends each of `added` after its network's nodes: each asked first,
/// every changed list stored in one write (all or none), then in use; the
/// changed networks in order.
///
/// # Errors
///
/// As [`add_endpoints`], for any of the networks.
pub(crate) async fn add_endpoints_each(
    container: &Container<Transport>,
    db: &Db,
    queue: &Queue,
    added: Vec<(base::Network, rpc::Endpoint)>,
) -> Result<Vec<Nodes>, Error> {
    let mut lists: BTreeMap<base::Network, Vec<rpc::Endpoint>> = BTreeMap::new();
    for (network, endpoint) in &added {
        container.service(network)?;
        if !lists.contains_key(network) {
            lists.insert(network.clone(), stored(db, network).await?.urls());
        }
        lists
            .entry(network.clone())
            .or_default()
            .push(endpoint.clone());
    }
    let lists = lists
        .into_iter()
        .map(|(network, nodes)| {
            Ok((
                network,
                RpcEndpoints::new(nodes).map_err(Error::invalid_argument)?,
            ))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    for (network, endpoint) in &added {
        verify(container, network, std::slice::from_ref(endpoint)).await?;
    }
    let kept = lists.clone();
    // Stored before connected: a restart never forgets a change in use.
    db.write(move |connection| {
        for (network, nodes) in &kept {
            endpoint::db::replace(connection, network, nodes)?;
        }
        Ok(())
    })
    .await?;
    let mut networks = Vec::new();
    for (network, _) in lists {
        networks.push(connected(container, db, queue, &network).await?);
    }
    Ok(networks)
}

/// Removes node `id` of `network` and its routes; the rest stored and in
/// use.
///
/// # Errors
///
/// Not found for an unconfigured network or no such node; failed precondition for the
/// network's last node; the store.
pub(crate) async fn remove_endpoint(
    container: &Container<Transport>,
    db: &Db,
    queue: &Queue,
    network: &base::Network,
    id: i64,
) -> Result<Nodes, Error> {
    container.service(network)?;
    let nodes = stored(db, network).await?;
    if !nodes.iter().any(|node| node.id == id) {
        return Err(Error::not_found("no such node"));
    }
    let kept = nodes.iter().filter(|node| node.id != id);
    let nodes = RpcEndpoints::new(kept.map(|node| node.url.clone()).collect())
        .map_err(|_| Error::failed_precondition("a network keeps at least one node"))?;
    store(db, network, nodes).await?;
    connected(container, db, queue, network).await
}

/// Enables (`enabled`) or disables node `id` of `network` by hand
/// (`reason`: why; none: `by hand`): an `enabled` or a `manual` record
/// stored, logged as `endpoint.enabled` or `endpoint.disabled`, then its
/// transport reloaded: a disabled node gets no call and its routes skip it.
///
/// # Errors
///
/// Not found for an unconfigured network or no such node; the store.
pub(crate) async fn set_endpoint_state(
    container: &Container<Transport>,
    db: &Db,
    queue: &Queue,
    network: &base::Network,
    id: i64,
    enabled: bool,
    reason: Option<String>,
) -> Result<Nodes, Error> {
    container.service(network)?;
    if !stored(db, network).await?.iter().any(|node| node.id == id) {
        return Err(Error::not_found("no such node"));
    }
    // Stored before connected: a restart never forgets a change in use.
    let reason = reason.unwrap_or_else(|| "by hand".to_owned());
    let record = if enabled {
        EndpointFailure::enabled(id, now(), reason.clone())
    } else {
        EndpointFailure::manual(id, now(), reason.clone())
    };
    db.write(move |connection| Ok(endpoint_failure::db::insert(connection, &record)?))
        .await?;
    if enabled {
        tracing::info!(target: crate::events::ENDPOINT_ENABLED, node = id, reason);
    } else {
        tracing::info!(target: crate::events::ENDPOINT_DISABLED, node = id, kind = "manual", reason);
    }
    connected(container, db, queue, network).await
}

/// A page of node `id` of `network`'s records (what disabled or enabled
/// it), newest first.
///
/// # Errors
///
/// Not found for an unconfigured network or no such node; the read.
pub(crate) async fn endpoint_failures_page(
    container: &Container<Transport>,
    db: &Db,
    network: &base::Network,
    id: i64,
    page: Page<i64>,
) -> Result<EndpointFailures, Error> {
    container.service(network)?;
    if !stored(db, network).await?.iter().any(|node| node.id == id) {
        return Err(Error::not_found("no such node"));
    }
    let page = move |connection: &mut _| Ok(endpoint_failure::db::find_page(connection, id, page)?);
    Ok(db.read(page).await?)
}

/// Asks each of `endpoints` which chain it serves and its tip, once, before
/// anything is stored (`Transport::verify`): a node that fails or serves
/// another chain is refused with why.
async fn verify(
    container: &Container<Transport>,
    network: &base::Network,
    endpoints: &[rpc::Endpoint],
) -> Result<(), Error> {
    let transport = container.transport(network)?;
    for (index, endpoint) in endpoints.iter().enumerate() {
        transport
            .verify(endpoint)
            .await
            .map_err(|error| Error::invalid_argument(format!("node {index}: {error:#}")))?;
    }
    Ok(())
}

/// Stores `nodes` as `network`'s list.
async fn store(db: &Db, network: &base::Network, nodes: RpcEndpoints) -> Result<(), Error> {
    let network = network.clone();
    // Stored before connected: a restart never forgets a change in use.
    db.write(move |connection| endpoint::db::replace(connection, &network, &nodes))
        .await?;
    Ok(())
}

/// `network`'s stored nodes, each with its state, after loading them and
/// its routes into its transport.
async fn connected(
    container: &Container<Transport>,
    db: &Db,
    queue: &Queue,
    network: &base::Network,
) -> Result<Nodes, Error> {
    Networks::new(db.clone()).await?.load(network, container)?;
    // A network with a node again indexes again: its index job stopped
    // (`Ack::Idle`) when its last node went. Unique: a running one stays.
    if container.has_node(network) {
        <Index as Worker>::enqueue(queue, &Target::from(network))
            .await
            .map_err(|error| Error::unavailable(error.to_string()))?;
    }
    endpoints(container, db, network).await
}

/// `network`'s routed methods, each over its nodes in failover order.
///
/// # Errors
///
/// Not found for an unconfigured network; the read.
pub(crate) async fn routes(
    container: &Container<Transport>,
    db: &Db,
    network: &base::Network,
) -> Result<Vec<Route>, Error> {
    container.service(network)?;
    let network = stored_network(db, network).await?;
    Ok(network.map(|network| network.routes).unwrap_or_default())
}

/// Makes `routes` `network`'s routed methods: checked against its nodes,
/// stored, then in use. No node is asked.
///
/// # Errors
///
/// Not found for an unconfigured network; invalid for an empty method, a
/// method routed twice, or a node that is missing, repeated or of another
/// network; the store.
pub(crate) async fn replace_routes(
    container: &Container<Transport>,
    db: &Db,
    network: &base::Network,
    routes: Vec<Route>,
) -> Result<Vec<Route>, Error> {
    container.service(network)?;
    let endpoints = stored(db, network).await?;
    let routes = Routes::new(routes, &endpoints).map_err(Error::invalid_argument)?;
    let ids = endpoints.ids();
    // Stored before connected: a restart never forgets a change in use.
    db.write(move |connection| endpoint_route::db::replace(connection, &ids, &routes))
        .await?;
    let networks = Networks::new(db.clone()).await?;
    networks.load(network, container)?;
    let stored = networks
        .networks
        .iter()
        .find(|stored| stored.key() == *network);
    Ok(stored
        .map(|stored| stored.routes.clone())
        .unwrap_or_default())
}

/// A page of `network`'s live kept blocks, latest first.
///
/// # Errors
///
/// Not found for an unconfigured network; the read.
pub(crate) async fn blocks_page(
    container: &Container<Transport>,
    db: &Db,
    network: base::Network,
    page: Page<i64>,
) -> Result<Vec<Block>, Error> {
    container.service(&network)?;
    Ok(db
        .read(move |connection| Ok(block::db::find_page(connection, &network, page)?))
        .await?)
}

/// The live block of `network` kept at `position`.
///
/// # Errors
///
/// Not found when none is kept there; the read.
pub(crate) async fn block(db: &Db, network: base::Network, position: i64) -> Result<Block, Error> {
    db.read(move |connection| Ok(block::db::find_by_position(connection, &network, position)?))
        .await?
        .ok_or_else(|| Error::not_found("no live block is kept at this position"))
}

/// A page of the kept transactions of `network`'s live block at
/// `position`, in block order.
///
/// # Errors
///
/// Not found when no live block is kept there; the read.
pub(crate) async fn block_transactions_page(
    db: &Db,
    network: base::Network,
    position: i64,
    page: Page<transaction::Key>,
) -> Result<Vec<Transaction>, Error> {
    db.read(move |connection| {
        let Some(block) = block::db::find_by_position(connection, &network, position)? else {
            return Ok(None);
        };
        Ok(Some(transaction::db::find_by_block_page(
            connection, &block.id, page,
        )?))
    })
    .await?
    .ok_or_else(|| Error::not_found("no live block is kept at this position"))
}

/// A page of `network`'s kept transactions: only the pending ones
/// (`pending`), oldest sent first; else pending and mined in live blocks,
/// newest first.
///
/// # Errors
///
/// Not found for an unconfigured network; the read.
pub(crate) async fn transactions_page(
    container: &Container<Transport>,
    db: &Db,
    network: base::Network,
    pending: bool,
    page: Page<transaction::Key>,
) -> Result<Vec<Transaction>, Error> {
    container.service(&network)?;
    Ok(db
        .read(move |connection| {
            if pending {
                return Ok(transaction::db::find_pending_page(
                    connection, &network, page,
                )?);
            }
            Ok(transaction::db::find_page(connection, &network, page)?)
        })
        .await?)
}
