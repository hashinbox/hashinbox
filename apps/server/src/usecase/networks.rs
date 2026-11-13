//! Networks and their nodes. [`Networks`]: every stored network with its
//! nodes and routes, read in one go; registered into the container once at
//! start, and loaded into a network's transport after every edit. Then what
//! is asked of them: every network with its live tip; a network's nodes and
//! routes, and changing them (each new node asked first, the change stored,
//! then in use); its kept blocks and transactions; and the search for its
//! block at a time.

use std::{collections::BTreeMap, time::Duration};

use base::{Container, InfoClient, NetworkConfig, ServiceFactory};
use tasks::Worker as _;
use tokio::sync::mpsc;

use super::{Error, Transport, workers::index};
use crate::{
    db::{Db, Page},
    models::{
        Block, Endpoint, Network, Route, Routes, RpcEndpoints, block, endpoint, endpoint_route,
        network, task,
        transaction::{self, Transaction},
    },
};

/// Longest [`overview`] waits for one network's node to tell its tip.
const LIVE: Duration = Duration::from_secs(2);

/// Every stored network: its config, nodes and routes, as the database holds
/// them.
pub(crate) struct Networks(Vec<Network>);

impl Networks {
    /// The networks with their assets, nodes, routes and index: one read.
    ///
    /// # Errors
    ///
    /// The read.
    pub(crate) async fn read(db: &Db) -> anyhow::Result<Self> {
        Ok(Self(db.read(network::db::find_all).await?))
    }

    /// Every indexed network of `F::CHAIN`: a transport read by `I`
    /// (`Transport::new::<I>`), loaded with its nodes and routes, registered
    /// with `F`. Main only, once per chain.
    pub(crate) fn register<I: InfoClient + 'static, F: ServiceFactory>(
        &self,
        container: &Container<Transport>,
    ) -> &Self {
        let indexed = self.0.iter().filter(|network| network.index.is_some());
        for network in indexed.filter(|network| network.key().chain == F::CHAIN) {
            let config = NetworkConfig::from(network);
            let transport = Transport::new::<I>(&config);
            // Every stored URL was checked when stored; one that no longer
            // builds is left out, and the API shows it without state.
            let _ = transport.load(&network.endpoints, &network.routes);
            container.register::<F>(config, transport);
        }
        self
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
        let Some(stored) = self.0.iter().find(|stored| stored.key() == *network) else {
            return transport.load(&[], &[]);
        };
        transport.load(&stored.endpoints, &stored.routes)
    }
}

/// A network's nodes as stored, in failover order, each with what the
/// running server knows of it.
pub(crate) struct Nodes {
    pub(crate) network: base::Network,
    pub(crate) endpoints: Vec<Endpoint>,
}

/// One step of a [`search_tip`].
pub(crate) enum Search {
    /// The block searched for is at or after `low`, before `high`.
    Probe(base::BlockProbe),
    /// The last block at or before the time.
    Found(base::BlockStamp),
    /// Why there is none: blocks cannot be found by time, the time is
    /// before the first block, or the node failed.
    Failed(String),
}

/// Every stored network with why its index job last failed, its nodes'
/// states, and its node's tip asked now: every network at once, each for at
/// most [`LIVE`]; one that fails keeps the stored tip and says why.
///
/// # Errors
///
/// The read.
pub(crate) async fn overview(
    container: &Container<Transport>,
    db: &Db,
) -> Result<Vec<Network>, Error> {
    let (mut networks, mut reasons) = db
        .read(|connection| {
            let networks = network::db::find_all(connection)?;
            let reasons = task::db::find_last_reasons(connection, index::KIND)?;
            Ok((networks, reasons))
        })
        .await?;
    for network in &mut networks {
        let target = index::Target {
            chain: network.chain.clone(),
            network: network.network.clone(),
        };
        let topic = index::Index::topic(&target).map(|topic| topic.to_string());
        network.index_job_reason = topic.and_then(|topic| reasons.remove(&topic));
    }
    let live = networks.into_iter().map(|network| live(container, network));
    Ok(futures_util::future::join_all(live).await)
}

/// `network` with its nodes' states, and its node's tip asked now or why
/// not.
async fn live(container: &Container<Transport>, mut network: Network) -> Network {
    let key = network.key();
    if let Ok(transport) = container.transport(&key) {
        transport.clients().states(&mut network.endpoints).await;
    }
    match tip(container, &key).await {
        Ok(tip) => network.tip_live = Some(tip),
        Err(error) => network.tip_error = Some(error),
    }
    network
}

/// The newest block of `network`'s node, asked now for at most [`LIVE`];
/// why not otherwise.
async fn tip(container: &Container<Transport>, network: &base::Network) -> Result<u64, String> {
    let service = container
        .service(network)
        .map_err(|error| format!("{error:#}"))?;
    let Some(dated) = &service.dated else {
        return Err("this network cannot read its tip".to_owned());
    };
    let Ok(tips) = tokio::time::timeout(LIVE, dated.tips()).await else {
        return Err("the node did not answer in time".to_owned());
    };
    tips.map(|tips| tips.produced)
        .map_err(|error| format!("{error:#}"))
}

/// `network`'s stored nodes, each with its node's state.
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
    let mut endpoints = db.read(endpoint::db::find_all).await?.nodes(network);
    let transport = container.transport(network)?;
    transport.clients().states(&mut endpoints).await;
    Ok(Nodes {
        network: network.clone(),
        endpoints,
    })
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
    network: &base::Network,
    added: Vec<rpc::Endpoint>,
) -> Result<Nodes, Error> {
    container.service(network)?;
    if added.is_empty() {
        return Err(Error::invalid("add 1 to 16 nodes at once"));
    }
    let mut nodes = endpoint::urls(&db.read(endpoint::db::find_all).await?.nodes(network));
    nodes.extend(added.clone());
    let nodes = RpcEndpoints::new(nodes).map_err(Error::invalid)?;
    verify(container, network, &added).await?;
    store(db, network, nodes).await?;
    connected(container, db, network).await
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
    network: &base::Network,
    nodes: Vec<rpc::Endpoint>,
) -> Result<Nodes, Error> {
    container.service(network)?;
    let nodes = RpcEndpoints::new(nodes).map_err(Error::invalid)?;
    verify(container, network, nodes.nodes()).await?;
    store(db, network, nodes).await?;
    connected(container, db, network).await
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
    added: Vec<(base::Network, rpc::Endpoint)>,
) -> Result<Vec<Nodes>, Error> {
    let stored = db.read(endpoint::db::find_all).await?;
    let mut lists: BTreeMap<base::Network, Vec<rpc::Endpoint>> = BTreeMap::new();
    for (network, endpoint) in &added {
        container.service(network)?;
        lists
            .entry(network.clone())
            .or_insert_with(|| endpoint::urls(&stored.nodes(network)))
            .push(endpoint.clone());
    }
    let lists = lists
        .into_iter()
        .map(|(network, nodes)| Ok((network, RpcEndpoints::new(nodes).map_err(Error::invalid)?)))
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
        networks.push(connected(container, db, &network).await?);
    }
    Ok(networks)
}

/// Removes node `id` of `network` and its routes; the rest stored and in
/// use.
///
/// # Errors
///
/// Not found for an unconfigured network or no such node; invalid for the
/// network's last node; the store.
pub(crate) async fn remove_endpoint(
    container: &Container<Transport>,
    db: &Db,
    network: &base::Network,
    id: i64,
) -> Result<Nodes, Error> {
    container.service(network)?;
    let mut nodes = db.read(endpoint::db::find_all).await?.nodes(network);
    let Some(position) = nodes.iter().position(|node| node.id == id) else {
        return Err(Error::not_found("no such node"));
    };
    nodes.remove(position);
    let nodes = RpcEndpoints::new(endpoint::urls(&nodes))
        .map_err(|_| Error::invalid("a network keeps at least one node"))?;
    store(db, network, nodes).await?;
    connected(container, db, network).await
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
            .map_err(|error| Error::invalid(format!("node {index}: {error:#}")))?;
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

/// `network`'s stored nodes, each with its node's state, after loading them
/// and its routes into its transport.
async fn connected(
    container: &Container<Transport>,
    db: &Db,
    network: &base::Network,
) -> Result<Nodes, Error> {
    Networks::read(db).await?.load(network, container)?;
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
    Ok(db.read(endpoint_route::db::find_all).await?.routes(network))
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
    let endpoints = db.read(endpoint::db::find_all).await?.nodes(network);
    let routes = Routes::new(routes, &endpoints).map_err(Error::invalid)?;
    let key = network.clone();
    // Stored before connected: a restart never forgets a change in use.
    db.write(move |connection| endpoint_route::db::replace(connection, &key, &routes))
        .await?;
    Networks::read(db).await?.load(network, container)?;
    Ok(db.read(endpoint_route::db::find_all).await?.routes(network))
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

/// Searches `network`'s node for its last block at or before `time` (Unix
/// seconds), on a task of its own: each step as the range narrows, then the
/// block or why not, on the receiver, which then closes. A caller gone
/// mid-search only stops hearing it.
///
/// # Errors
///
/// Not found for an unconfigured network.
pub(crate) fn search_tip(
    container: &Container<Transport>,
    network: &base::Network,
    time: u64,
) -> Result<mpsc::UnboundedReceiver<Search>, Error> {
    let dated = container.service(network)?.dated.clone();
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let probe = |probe| {
            let _ = sender.send(Search::Probe(probe));
        };
        let found = match &dated {
            None => Err(anyhow::anyhow!("this network cannot find blocks by time")),
            Some(dated) => dated.search(time, &probe).await,
        };
        let last = match found {
            Ok(Some(stamp)) => Search::Found(stamp),
            Ok(None) => Search::Failed("the time is before the network's first block".to_owned()),
            Err(error) => Search::Failed(format!("{error:#}")),
        };
        let _ = sender.send(last);
    });
    Ok(receiver)
}
