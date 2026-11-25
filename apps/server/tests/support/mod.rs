//! The server suite's harness: [`App`] runs the real server (its container,
//! routes and workers) over a mock chain's nodes, [`Api`] calls its routes, [`until`] waits for
//! the workers, and `scenarios!` emits one test per case, chain and
//! topology. Nothing here opens a socket. Besides, temp databases (`db`)
//! and the scripted network the older tests run over (`scripted`).

mod api;
pub(crate) mod db;
pub(crate) mod scripted;

use std::{future::Future, sync::Arc, time::Duration};

use base::{
    Container, NetworkConfig,
    testing::{TestChain, Topology},
};
use diesel::{
    RunQueryDsl as _,
    connection::SimpleConnection as _,
    sql_types::{BigInt, Bool, Integer, Nullable, Text},
};

use crate::{api::AppState, usecase, usecase::workers};
use tasks::Executor;

pub(crate) use api::{Api, Wallet};

/// How often the job loop looks for due jobs.
const TICK: Duration = Duration::from_millis(5);
/// The longest [`until`] waits: a safety net, never a case's clock. The
/// workers run on their own timers (an index retry waits a second longer
/// after each failure, a sent payment is checked every chain's
/// `Tracking::check`), which a busy machine stretches.
const PATIENCE: Duration = Duration::from_secs(30);

/// A test chain's reader and factory, as `main` registers its chain.
pub(crate) trait Mounted: TestChain {
    /// How a node is read.
    type Info: base::InfoClient + 'static;
    /// How a network is built.
    type Factory: base::ServiceFactory;
}

impl Mounted for chain_ethereum::testing::Ethereum {
    type Info = chain_ethereum::Info;
    type Factory = chain_ethereum::Factory;
}

/// The server over a mock chain, on its own SQLite file.
pub(crate) struct App {
    pub(crate) api: Api,
    db: crate::db::Db,
    /// The job queues; dropping them stops them.
    _jobs: tasks::Running,
    _directory: tempfile::TempDir,
}

impl App {
    /// [`Self::boot`] with `chain`'s network alone.
    pub(crate) async fn start<T: Mounted>(chain: &T) -> Self {
        Self::boot::<T>(vec![(chain.network(), chain.nodes())]).await
    }

    /// Seeds a new file as `migrate` does, with `networks` as the only ones,
    /// each one's node `i` stored at `http://{name}-{i}.invalid/`; registers
    /// each over a transport of its nodes, as `main` does over stored ones,
    /// and runs every worker.
    pub(crate) async fn boot<T: Mounted>(
        networks: Vec<(NetworkConfig, Vec<Arc<dyn rpc::Client>>)>,
    ) -> Self {
        let (db, directory) = db::fresh().await;
        clear(&db).await;
        let container = Container::new(usecase::sdk::Stores::storage(
            db.clone(),
            Default::default(),
        ));
        for (network, nodes) in networks {
            let urls: Vec<String> = (0..nodes.len())
                .map(|node| format!("http://{}-{node}.invalid/", network.network.name))
                .collect();
            seed(&db, network.clone(), urls).await;
            let transport = usecase::Transport::from_clients::<T::Info>(&network, nodes, None);
            container.register::<T::Factory>(network, transport);
        }
        let state = Arc::new(AppState {
            container: Arc::new(container),
            queue: usecase::tasks::queue(db.clone()),
            actions: Arc::new(usecase::workers::trigger::actions::registry()),
            logs: crate::usecase::logs::feed(),
            db: db.clone(),
            candles: Default::default(),
        });
        let (container, queue, actions) = (&state.container, &state.queue, &state.actions);
        let jobs = Executor::new(queue.clone(), TICK)
            .add(workers::index::Index::new(
                container.clone(),
                db.clone(),
                queue.clone(),
            ))
            .add(workers::prune::Prune::new(db.clone(), container.clone()))
            .add(workers::broadcast::Broadcast::new(
                container.clone(),
                db.clone(),
            ))
            .add(workers::trigger::Round::new(
                container.clone(),
                db.clone(),
                queue.clone(),
                actions.clone(),
            ))
            .run();
        let config = http_server::Config::new(
            "127.0.0.1:0".parse().expect("an address"),
            http_server::TransportSecurity::PlaintextLoopback,
            None,
            http_server::RequestLimits::default(),
        );
        let router = crate::api::router(state, &config).expect("a router");
        Self {
            api: Api::new(router),
            db,
            _jobs: jobs,
            _directory: directory,
        }
    }
}

impl App {
    /// Runs `sql` on the server's file: a fault planted under the API
    /// (a SQLite trigger refusing a write).
    pub(crate) async fn sql(&self, sql: &'static str) {
        self.db
            .write(move |connection| Ok(connection.batch_execute(sql)?))
            .await
            .expect("the statement runs");
    }
}

/// Deletes the seeded networks.
async fn clear(db: &crate::db::Db) {
    db.write(|connection| {
        connection.batch_execute(
            "DELETE FROM network_endpoint; DELETE FROM network_asset; \
             DELETE FROM network_index; DELETE FROM network;",
        )?;
        Ok(())
    })
    .await
    .expect("the seeded networks are deleted");
}

/// Adds `network`, reached at `urls`.
async fn seed(db: &crate::db::Db, network: NetworkConfig, urls: Vec<String>) {
    db.write(move |connection| {
        let (chain, name) = (network.network.chain.to_string(), network.network.name);
        diesel::sql_query(
            "INSERT INTO network (chain, network, chain_id, testnet, enabled, block_time_ms, \
             confirmations, settings) VALUES (?, ?, ?, ?, 1, ?, ?, ?)",
        )
        .bind::<Text, _>(&chain)
        .bind::<Text, _>(&name)
        .bind::<Nullable<Text>, _>(&network.chain_id)
        .bind::<Bool, _>(network.testnet)
        .bind::<BigInt, _>(i64::try_from(network.block_time.as_millis())?)
        .bind::<BigInt, _>(i64::try_from(network.confirmations)?)
        .bind::<Text, _>(network.settings.to_string())
        .execute(connection)?;
        for asset in &network.assets {
            let kind = match asset.kind {
                base::AssetKind::Native => "native",
                base::AssetKind::Contract => "contract",
            };
            diesel::sql_query(
                "INSERT INTO network_asset (chain, network, asset, kind, contract, ticker, \
                 decimals) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind::<Text, _>(&chain)
            .bind::<Text, _>(&name)
            .bind::<Text, _>(&asset.asset)
            .bind::<Text, _>(kind)
            .bind::<Nullable<Text>, _>(&asset.contract)
            .bind::<Text, _>(&asset.ticker)
            .bind::<Integer, _>(i32::try_from(asset.decimals)?)
            .execute(connection)?;
        }
        for (position, url) in urls.iter().enumerate() {
            diesel::sql_query(
                "INSERT INTO network_endpoint (chain, network, url, position) VALUES (?, ?, ?, ?)",
            )
            .bind::<Text, _>(&chain)
            .bind::<Text, _>(&name)
            .bind::<Text, _>(url)
            .bind::<BigInt, _>(i64::try_from(position)?)
            .execute(connection)?;
        }
        Ok(())
    })
    .await
    .expect("the test network is seeded");
}

/// Polls `probe` until it answers `Ok`, at most [`PATIENCE`].
///
/// # Panics
///
/// When it never does: with `what` and the last thing it saw.
pub(crate) async fn until<T, F, Fut>(what: &str, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let seen = match probe().await {
            Ok(value) => return value,
            Err(seen) => seen,
        };
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}; last saw {seen}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// `single` or `cluster`, as a test name says it.
pub(crate) fn topology(name: &str) -> Topology {
    match name {
        "cluster" => Topology::Cluster,
        _ => Topology::Single,
    }
}

/// One `#[tokio::test]` per case, chain and topology, named
/// `suite::<case>::<chain>::<topology>`: a fresh chain (`prepare`d), a booted
/// [`App`], the case, and its wall time on stderr. A chain short of what
/// the case needs prints `skip` and passes. Attributes before a topology
/// (`#[ignore = "finding: …"]`) go on its test.
macro_rules! scenarios {
    ($($case:ident: $($chain:ident => $Chain:ty [$($(#[$attr:meta])* $topology:ident),+]),+;)+) => {
        mod suite {$(mod $case {$(mod $chain {$(
            $(#[$attr])*
            #[tokio::test(flavor = "multi_thread")]
            async fn $topology() {
                use base::testing::{Caps, TestChain as _, Topology};
                use crate::tests::{cases::$case as case, support};
                let topology = support::topology(stringify!($topology));
                let needs = if topology == Topology::Cluster {
                    case::NEEDS.with(Caps::CLUSTER)
                } else {
                    case::NEEDS
                };
                if !<$Chain>::CAPS.contains(needs) {
                    eprintln!("skip {}", module_path!());
                    return;
                }
                let started = std::time::Instant::now();
                let chain = <$Chain>::start(topology.nodes());
                case::prepare(&chain);
                let app = support::App::start(&chain).await;
                case::case(&app, &chain).await;
                eprintln!("{} took {:?}", module_path!(), started.elapsed());
            }
        )+})+})+}
    };
}

pub(crate) use scenarios;
