//! Payment server process: reads one configuration document, has each
//! registered chain parse its own section (`chains`, the only module that
//! names a chain), composes every chain's RPC client, indexer, and wallet
//! factories once over one storage backend, and serves the
//! public HTTP API (`api`, which names no chain, no configuration reader and
//! no storage backend) while indexing catches up, with the browser page
//! (`ui`) beside it. The durable entities and their tables are `models`.
//! Nothing is recomposed while the process runs: each network's nodes are
//! stored in the database and composed behind a `json_rpc::Swap`, so a
//! verified replacement repoints them live, and one network can be rescanned
//! online; both are routes (`api`) over handles composed here.

mod api;
#[cfg(test)]
mod app;
mod asset;
mod chains;
mod config;
mod models;
mod operator;
mod state;
mod storage;
mod ui;

use std::{
    collections::BTreeMap, ffi::OsString, future::IntoFuture, path::PathBuf, sync::Arc,
    time::Duration,
};

use api::networks::{LiveNetwork, Rejected, Verifier};
use asset::AssetTag;
use base::Network;
use config::{AnyError, Config};
use indexing::Driver;
use models::{RpcEndpoints, Signers};
use payments::config::{ChainNodes, Chains, ErrorKind};
use state::AppState;
use storage::Repository;
use tokio::sync::{oneshot, watch};
use wallets::{Policy, Submissions};

/// How long a candidate node has to prove which network it serves.
const VERIFICATION: Duration = Duration::from_secs(30);

/// Every way the binary may be invoked; every refusal of an argument prints it.
pub(crate) const USAGE: &str = "usage: payment-server serve <config.toml> \
                                [--rescan <chain>/<network>] \
                                | payment-server migrate <config.toml> | payment-server --openapi";

/// The command line.
#[derive(clap::Parser)]
#[command(name = "payment-server", args_conflicts_with_subcommands = true)]
struct Cli {
    /// Print the OpenAPI document instead of serving, so a typed client can
    /// be generated without a node, a database, or a bound port.
    #[arg(long)]
    openapi: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Run the server, or one offline operator action.
    Serve {
        /// The configuration document.
        config: PathBuf,
        /// `--rescan <chain>/<network>`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        operation: Vec<OsString>,
    },
    /// Open the configured database file, applying the schema to an empty one.
    Migrate {
        /// The configuration document.
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    let cli = <Cli as clap::Parser>::parse();
    let (path, arguments) = match cli.command {
        _ if cli.openapi => {
            println!("{}", serde_json::to_string_pretty(&api::openapi())?);
            return Ok(());
        }
        None => return Err(USAGE.into()),
        Some(Command::Migrate { config }) => {
            Config::read(config).await?.storage.open().await?;
            return Ok(());
        }
        Some(Command::Serve { config, operation }) => (config, operation),
    };
    // Before the configuration is read, so a typo never reaches a node or a store.
    let operation = operator::Operation::parse(arguments.into_iter())?;
    let config = Config::read(path).await?;
    // Offline: no chain connects, no driver runs, no listener binds.
    if let Some(operation) = operation {
        return operation.run(&config).await;
    }
    let server = config.server()?;
    let face = ui::Settings::from_params(ui::SettingsParams {
        brand: config.ui.brand.clone(),
        api_base: Config::api_base()?,
    });

    // Owns every background send task a chain hands over, so the shutdown
    // sequence below can refuse new ones and wait for the running ones.
    let submissions = Arc::new(Submissions::new());
    let registrar: Arc<dyn wallets::SubmissionRegistrar> = submissions.clone();
    // The database is every network's node source: the file's nodes seed it
    // for a network it has none of. Every chain is composed over a swappable
    // transport of those nodes; a chain whose node fails verification is
    // skipped and named, and the others serve.
    let store = config.storage.open().await?;
    let (pool, repository) = (store.pool(), store.repository());
    let (chains, endpoints) = with_stored_endpoints(config.chains()?, &pool).await?;
    for notice in chains.notices() {
        println!("{notice}");
    }
    let startup = chains
        .compose_available(
            || std::future::ready(Ok::<_, AnyError>(store)),
            |store: &storage::Store| Ok::<_, AnyError>(store.repository()),
            &registrar,
        )
        .await?;
    for error in &startup.skipped {
        eprintln!("skipped, its node failed verification: {error}");
    }
    let composition = startup.composition;
    // Every sending asset's sends wait in one in-memory queue, bounded in
    // flight per node and retried only when nothing was accepted; its pauses
    // are Tokio's, the one runtime this process names.
    let mut wallets = composition.wallets;
    wallets.queue(&Policy {
        pause: Arc::new(|delay| Box::pin(tokio::time::sleep(delay))),
        ..Policy::default()
    });
    let wallets = Arc::new(wallets);
    // Before serving: every observed, keyed wallet of a served network is
    // rebuilt as a signer. The index restores its watches from the same rows.
    let registry: Arc<dyn indexing::WalletRegistry> = Arc::new(repository.clone());
    let signers = Signers::load(registry.as_ref(), &wallets, composition.index.networks()).await?;
    let networks = live(&startup.networks, endpoints);

    let (shutdown, shutdown_rx) = watch::channel(false);
    let driver = Driver::new(composition.index.clone());
    let control = driver.control();
    let state = AppState {
        wallets: wallets.clone(),
        units: Arc::new(composition.units),
        signers: Arc::new(signers),
        pool,
        registry,
        index: composition.index,
        pending: composition.pending.into(),
        health: driver.health(),
        networks,
        verify: verifier(startup.networks),
        control: control.clone(),
        index_store: Arc::new(repository.clone()),
        repository,
    };
    let mut synchronization = Some(tokio::spawn(driver.run(shutdown_rx)));
    let router = api::router(state, &server)?.merge(face.routes()?);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let (stop_http, stop_http_rx) = oneshot::channel();
    let serving = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = stop_http_rx.await;
        })
        .into_future();
    tokio::pin!(serving);
    let (mut result, http_done) = tokio::select! {
        result = &mut serving => (result.map_err(Into::into), true),
        completed = synchronization.as_mut().expect("synchronization task") => {
            synchronization = None;
            (match completed {
                Ok(()) => Err("index synchronization stopped unexpectedly".into()),
                Err(error) => Err(error.into()),
            }, false)
        }
        signal = tokio::signal::ctrl_c() => (signal.map_err(Into::into), false),
    };
    // The shutdown order, as consecutive statements, so a failure in one
    // stage cannot skip the stages that drain the work in flight.
    //
    // 1. Stop reporting ready, so no new work is admitted.
    control.reset_health();
    // 2. Ask HTTP to stop accepting.
    if !http_done {
        let _ = stop_http.send(());
    }
    // 3. Close the send queues, so a handler still running is refused a new
    //    send; sends already admitted are still dispatched and answered.
    wallets.close();
    // 4. Close submission registration, so no chain hands over more work.
    submissions.close();
    // 5. Drain the handlers already in flight, and with them every queued
    //    send they admitted.
    if !http_done && let Err(error) = serving.await {
        result = result.and(Err(error.into()));
    }
    // 6. Drain the submissions they registered, while indexing can still
    //    reconcile an ambiguous one.
    if let Err(error) = submissions.drain().await {
        result = result.and(Err(error.into()));
    }
    // 7. Only then stop indexing, and 8. join it.
    if let Some(task) = synchronization.take() {
        let _ = shutdown.send(true);
        if let Err(error) = task.await {
            result = result.and(Err(error.into()));
        }
    }
    result
}

/// `chains` with each network's stored nodes in place of its file's, the
/// file's nodes stored first for a network the database has none of; and
/// every network's nodes now.
///
/// # Errors
///
/// A stored row or a section's nodes that no longer check, or the write.
// linter:disable rust/single-use-free-function -- The live-maintenance test starts through it too.
async fn with_stored_endpoints(
    chains: Chains<AssetTag, Repository>,
    pool: &sqlx::SqlitePool,
) -> Result<
    (
        Chains<AssetTag, Repository>,
        BTreeMap<Network, RpcEndpoints>,
    ),
    AnyError,
> {
    let stored = RpcEndpoints::find_all(pool).await?;
    let documents = stored
        .iter()
        .map(|(network, endpoints)| (network.clone(), endpoints.documents()))
        .collect();
    let chains = chains.with_endpoints(&documents)?;
    let mut endpoints = BTreeMap::new();
    for (network, documents) in chains.endpoints() {
        let configured = RpcEndpoints::new(documents.to_vec())
            .map_err(|reason| format!("{}/{}: {reason}", network.chain, network.name))?;
        RpcEndpoints::insert_missing(pool, network, &configured).await?;
        endpoints.insert(network.clone(), configured);
    }
    Ok((chains, endpoints))
}

/// Every configured network as the routes maintain it, in configuration
/// order, with its nodes and, when composed, its transport.
fn live(
    networks: &[ChainNodes<AssetTag, Repository>],
    mut endpoints: BTreeMap<Network, RpcEndpoints>,
) -> Arc<[Arc<LiveNetwork>]> {
    networks
        .iter()
        .filter_map(|chain| {
            let nodes = endpoints.remove(chain.network())?;
            let transport = chain.transport().cloned();
            Some(Arc::new(LiveNetwork::new(
                chain.network().clone(),
                transport,
                nodes,
            )))
        })
        .collect()
}

/// A candidate node list checked by its network's own chain over the
/// transport it builds, within [`VERIFICATION`].
// linter:disable rust/single-use-free-function -- The live-maintenance test routes through it too.
fn verifier(networks: Vec<ChainNodes<AssetTag, Repository>>) -> Verifier {
    let chains: Arc<BTreeMap<Network, ChainNodes<AssetTag, Repository>>> = Arc::new(
        networks
            .into_iter()
            .map(|chain| (chain.network().clone(), chain))
            .collect(),
    );
    Arc::new(move |network, endpoints: RpcEndpoints| {
        let chains = chains.clone();
        Box::pin(async move {
            let chain = chains
                .get(&network)
                .ok_or_else(|| Rejected::Invalid("network is not configured".into()))?;
            let check = chain.verify(endpoints.nodes());
            let Ok(verified) = tokio::time::timeout(VERIFICATION, check).await else {
                return Err(Rejected::Unverified(format!(
                    "the node did not prove its network within {} seconds",
                    VERIFICATION.as_secs()
                )));
            };
            verified.map_err(Rejected::from)
        })
    })
}

/// A chain's refusal of a candidate node list: a node that did not prove its
/// network is unverified; anything else makes the list invalid.
impl From<payments::config::Error> for Rejected {
    fn from(error: payments::config::Error) -> Self {
        if error.kind == ErrorKind::Unverified {
            return Self::Unverified(error.to_string());
        }
        Self::Invalid(error.to_string())
    }
}

/// The live maintenance of the composed networks, end to end: startup's plan
/// over a fixture chain and a real database file, the handles composed here,
/// and the network routes acting on them.
#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use axum::{body::Body, http::StatusCode};
    use base::{BoxFuture, ChainName};
    use http_body_util::BodyExt as _;
    use indexing::{
        Address, AssetId, Budget, Config, IndexStore as _, Indexer, Indexing, WalletRegistry as _,
        Watch, WatchStore as _, testing,
    };
    use json_rpc::{Client as _, Request, Statistics, Swap};
    use payments::config::ComposedChain;
    use payments::config::{Registry, Sections};
    use serde::Deserialize;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tower::ServiceExt as _;
    use wallets::SubmissionRegistrar;

    use super::*;

    /// Every scripted chain a fixture composed, by the `line` its section
    /// names: what a test reads its inspections from.
    static LINES: Mutex<BTreeMap<String, testing::Chain>> = Mutex::new(BTreeMap::new());

    /// A chain section in miniature: a network name, the scripted chain it
    /// indexes, and the shared `rpc` shell. Its node proves its network by
    /// naming it.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fixture {
        network: String,
        line: String,
        rpc: payments::config::Rpc,
    }

    impl payments::config::Chain<AssetTag, Repository> for Fixture {
        fn network(&self) -> Network {
            Network::new(ChainName::new("fixture"), self.network.as_str())
        }

        fn accept(&self) -> Result<Option<String>, payments::config::Error> {
            self.rpc.validate()?;
            Ok(None)
        }

        fn verify<'a>(
            &'a self,
            transport: &'a payments::config::Transport,
        ) -> BoxFuture<'a, Result<(), payments::config::Error>> {
            Box::pin(async move {
                let named = transport
                    .request(Request::new("network", json!([])))
                    .await
                    .map_err(payments::config::Error::refused)?
                    .map_err(|_| payments::config::Error::refused("the node refused the call"))?
                    .deserialize::<String>()
                    .map_err(payments::config::Error::refused)?;
                if named != self.network {
                    return Err(payments::config::Error::refused(format!(
                        "the node serves {named}, not {}",
                        self.network
                    )));
                }
                Ok(())
            })
        }

        fn connect<'a>(
            &'a self,
            _statistics: &'a mut Vec<Statistics>,
            _logger: logging::Logger,
            transport: Option<payments::config::Transport>,
        ) -> BoxFuture<
            'a,
            Result<
                Box<dyn payments::config::Connection<AssetTag, Repository>>,
                payments::config::Error,
            >,
        > {
            Box::pin(async move {
                let transport = match transport {
                    Some(transport) => transport,
                    None => self.rpc.transport()?,
                };
                self.verify(&transport).await?;
                let verified = Verified {
                    network: self.network(),
                    line: self.line.clone(),
                };
                Ok(Box::new(verified)
                    as Box<
                        dyn payments::config::Connection<AssetTag, Repository>,
                    >)
            })
        }
    }

    /// A verified fixture node: it composes a scripted chain at block 6 over
    /// the one repository, with one wallet asset tagged as its network.
    struct Verified {
        network: Network,
        line: String,
    }

    impl payments::config::Connection<AssetTag, Repository> for Verified {
        fn compose<'a>(
            self: Box<Self>,
            repository: Repository,
            _registrar: &'a Arc<dyn SubmissionRegistrar>,
        ) -> BoxFuture<'a, Result<ComposedChain<AssetTag>, payments::config::Error>> {
            Box::pin(async move {
                let line = testing::Chain::dense(self.network.clone(), 6);
                LINES
                    .lock()
                    .expect("the lines")
                    .insert(self.line.clone(), line.clone());
                let schedule = indexing::Schedule::every(Duration::from_millis(1));
                let config = Config::new(&self.network, Budget::new(100), schedule)?;
                let indexer = Arc::new(Indexer::new(line.clone(), line, repository, config));
                let tag: AssetTag = self.network.name.parse().expect("a tag");
                let factory = wallets::testing::Factory::new(self.network.clone());
                Ok(ComposedChain {
                    pending: indexer.clone(),
                    index: indexer as Arc<dyn Indexing>,
                    factories: vec![(tag, Box::new(factory) as Box<dyn wallets::WalletFactory>)],
                    units: std::iter::once((
                        (
                            self.network.clone(),
                            AssetId::native(self.network.chain.clone()),
                        ),
                        base::Unit::native(&COIN),
                    ))
                    .collect(),
                })
            })
        }
    }

    /// The native coin of every fixture network.
    const COIN: base::Asset = base::Asset::new(
        base::ChainNetwork::new(
            ChainName::from_static("Fixture"),
            "fixture",
            base::NetworkKind::Mainnet,
            "FIX",
        ),
        "Fixture Coin",
        "FIX",
        8,
    );

    fn registry() -> Registry<AssetTag, Repository> {
        let mut registry = Registry::new();
        registry.register::<Fixture>("one");
        registry.register::<Fixture>("two");
        registry
    }

    fn network(name: &str) -> Network {
        Network::new(ChainName::new("fixture"), name)
    }

    /// A local JSON-RPC node that answers every call with the network it
    /// claims, and counts them.
    struct Node {
        url: String,
        calls: Arc<AtomicUsize>,
    }

    impl Node {
        async fn serving(network: &'static str) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a local port");
            let url = format!("http://{}", listener.local_addr().expect("an address"));
            let calls = Arc::new(AtomicUsize::new(0));
            let counted = calls.clone();
            let answer = axum::routing::post(move |axum::Json(call): axum::Json<Value>| {
                counted.fetch_add(1, Ordering::SeqCst);
                let answer = json!({"jsonrpc": "2.0", "id": call["id"], "result": network});
                async move { axum::Json(answer) }
            });
            let router = axum::Router::new().fallback(answer);
            tokio::spawn(async move { axum::serve(listener, router).await });
            Self { url, calls }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    /// One database file, and the process that starts over it.
    struct Bench {
        directory: TempDir,
        path: std::path::PathBuf,
    }

    /// A started process: its routes, what its chains hold, and what indexes
    /// them.
    struct Started {
        state: AppState,
        router: axum::Router,
        /// What each composed network's chain was composed over.
        transports: BTreeMap<Network, Arc<Swap>>,
        repository: Repository,
        /// The key of this process's scripted chains in [`LINES`].
        prefix: String,
        stop: Option<watch::Sender<bool>>,
    }

    impl Bench {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("a directory");
            Self {
                path: directory.path().join("server.db"),
                directory,
            }
        }

        /// The configuration's chain sections: each named network on `urls`.
        fn sections(&self, sections: &[(&str, &[&str])]) -> Sections {
            sections
                .iter()
                .map(|(name, urls)| {
                    let line = format!("{}/{name}", self.directory.path().display());
                    let rpc = json!({"endpoints": urls, "repeat": "off"});
                    let section = json!({"network": name, "line": line, "rpc": rpc});
                    ((*name).to_owned(), section)
                })
                .collect()
        }

        /// Starts as `main` does over `sections`: stored nodes win, every
        /// network whose node proves itself is composed (a scripted chain at
        /// block 6 with one watch from block 3 and one wallet asset), and the
        /// driver runs when `supervised`.
        async fn start(&self, sections: &Sections, supervised: bool) -> Started {
            let storage: storage::Config =
                serde_json::from_value(json!({"path": self.path})).expect("storage");
            let store = storage.open().await.expect("the file");
            let (pool, repository) = (store.pool(), store.repository());
            let chains = registry().chains(sections).expect("accepted");
            for network in chains.networks() {
                // Registered before the indexer loads its watches, as a
                // restored deployment's are.
                repository
                    .add(vec![Self::watch(&network)])
                    .await
                    .expect("watched");
            }
            let (chains, endpoints) = with_stored_endpoints(chains, &pool).await.expect("seeded");
            let registrar: Arc<dyn SubmissionRegistrar> = Arc::new(wallets::Submissions::new());
            let startup = chains
                .compose_available(
                    || std::future::ready(Ok::<_, AnyError>(store)),
                    |store: &storage::Store| Ok::<_, AnyError>(store.repository()),
                    &registrar,
                )
                .await
                .expect("composed");
            let transports = startup
                .networks
                .iter()
                .filter_map(|chain| Some((chain.network().clone(), chain.transport()?.clone())))
                .collect();
            let composition = startup.composition;
            let mut wallets = composition.wallets;
            wallets.queue(&Policy::default());
            let wallets = Arc::new(wallets);
            let registry: Arc<dyn indexing::WalletRegistry> = Arc::new(repository.clone());
            let signers = Signers::load(registry.as_ref(), &wallets, composition.index.networks())
                .await
                .expect("loaded");
            let driver = Driver::new(composition.index.clone());
            let state = AppState {
                wallets,
                units: Arc::new(composition.units),
                signers: Arc::new(signers),
                pool,
                registry,
                index: composition.index,
                pending: composition.pending.into(),
                health: driver.health(),
                networks: live(&startup.networks, endpoints),
                verify: verifier(startup.networks),
                control: driver.control(),
                index_store: Arc::new(repository.clone()),
                repository: repository.clone(),
            };
            let stop = supervised.then(|| {
                let (stop, shutdown) = watch::channel(false);
                tokio::spawn(driver.run(shutdown));
                stop
            });
            let config = http_server::Config::new(
                "127.0.0.1:0".parse().expect("a bind address"),
                http_server::TransportSecurity::PlaintextLoopback,
                None,
                http_server::RequestLimits::default(),
            );
            Started {
                router: api::router(state.clone(), &config).expect("the router"),
                state,
                transports,
                repository,
                prefix: self.directory.path().display().to_string(),
                stop,
            }
        }

        /// The one address watched on `network`, from block 3.
        fn watch(network: &Network) -> Watch {
            Watch {
                id: format!("{}-watch", network.name),
                address: Address::new(network.clone(), "addr"),
                asset: AssetId::native(network.chain.clone()),
                start: indexing::BlockPosition(3),
            }
        }
    }

    impl Started {
        /// One request to the routes: the status and the JSON body.
        async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
            let request = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(
                    body.map(|body| body.to_string()).unwrap_or_default(),
                ))
                .expect("a request");
            let response = self
                .router
                .clone()
                .oneshot(request)
                .await
                .expect("a response");
            let status = response.status();
            let bytes = response
                .into_body()
                .collect()
                .await
                .expect("a body")
                .to_bytes();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            )
        }

        /// Replaces `network`'s nodes with `endpoints`.
        async fn replace(&self, name: &str, endpoints: Value) -> (StatusCode, Value) {
            let path = format!("/v1/networks/fixture/{name}/rpc");
            self.call("PUT", &path, Some(json!({"endpoints": endpoints})))
                .await
        }

        async fn rescan(&self, name: &str) -> (StatusCode, Value) {
            let path = format!("/v1/networks/fixture/{name}/rescans");
            self.call("POST", &path, None).await
        }

        /// Every configured network's listing.
        async fn listed(&self) -> Vec<Value> {
            let (status, body) = self.call("GET", "/v1/networks", None).await;
            assert_eq!(status, StatusCode::OK);
            body["networks"].as_array().expect("networks").clone()
        }

        async fn origins(&self, name: &str) -> Vec<String> {
            let listed = self.listed().await;
            let row = listed
                .iter()
                .find(|row| row["network"]["name"] == name)
                .expect("configured");
            row["nodes"]
                .as_array()
                .expect("nodes")
                .iter()
                .map(|node| node["origin"].as_str().expect("an origin").to_owned())
                .collect()
        }

        /// One call through what `network`'s composed chain holds.
        async fn call_through(&self, network: &Network) -> String {
            self.transports[network]
                .request(Request::new("network", json!([])))
                .await
                .expect("an answer")
                .expect("a result")
                .deserialize()
                .expect("a name")
        }

        /// `network`'s stored node documents.
        async fn stored(&self, network: &Network) -> Vec<Value> {
            let stored = RpcEndpoints::find_all(&self.state.pool)
                .await
                .expect("stored");
            stored
                .into_iter()
                .find(|(stored, _)| stored == network)
                .map(|(_, endpoints)| endpoints.documents())
                .unwrap_or_default()
        }

        /// The scripted chain this process composed for section `name`.
        fn line(&self, name: &str) -> testing::Chain {
            let lines = LINES.lock().expect("the lines");
            lines[&format!("{}/{name}", self.prefix)].clone()
        }

        /// Waits until `network` is committed through the line's tip with
        /// every watch inspected there.
        async fn settles(&self, network: &Network) {
            let line = Some(indexing::testing::block(6));
            let settled = async {
                loop {
                    let checkpoint = self.repository.checkpoint(network).await.expect("tip");
                    let watched = self.repository.all(network).await.expect("watches");
                    if checkpoint == line && watched.iter().all(|row| row.indexed_through == line) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            };
            tokio::time::timeout(Duration::from_secs(10), settled)
                .await
                .expect("the network reaches its tip");
        }
    }

    impl Drop for Started {
        fn drop(&mut self) {
            if let Some(stop) = self.stop.take() {
                stop.send_replace(true);
            }
        }
    }

    /// The file seeds the database once; after that the database names the
    /// nodes, whatever the file says on a later start.
    #[tokio::test]
    async fn the_file_seeds_the_nodes_once_and_the_database_wins_after() {
        let bench = Bench::new();
        let (seeded, edited) = (Node::serving("one").await, Node::serving("one").await);
        let first = bench
            .start(&bench.sections(&[("one", &[&seeded.url])]), false)
            .await;
        assert_eq!(first.origins("one").await, [seeded.url.as_str()]);
        drop(first);

        let restarted = bench
            .start(&bench.sections(&[("one", &[&edited.url])]), false)
            .await;
        assert_eq!(
            restarted.origins("one").await,
            [seeded.url.as_str()],
            "the stored nodes override the file's"
        );
        let calls = seeded.calls();
        assert_eq!(restarted.call_through(&network("one")).await, "one");
        assert_eq!(seeded.calls(), calls + 1);
        assert_eq!(edited.calls(), 0, "the file's new node is never reached");
    }

    /// A verified replacement serves the next call, is listed, and is what
    /// the next start connects to.
    #[tokio::test]
    async fn a_replaced_node_serves_later_calls_and_survives_a_restart() {
        let bench = Bench::new();
        let (old, new) = (Node::serving("one").await, Node::serving("one").await);
        let sections = bench.sections(&[("one", &[&old.url])]);
        let started = bench.start(&sections, false).await;
        let (status, summary) = started.replace("one", json!([new.url])).await;
        assert_eq!(status, StatusCode::OK, "{summary}");
        assert_eq!(summary["state"], "catching_up", "served, not yet reported");
        assert_eq!(started.origins("one").await, [new.url.as_str()]);
        let calls = (old.calls(), new.calls());
        assert_eq!(started.call_through(&network("one")).await, "one");
        assert_eq!((old.calls(), new.calls()), (calls.0, calls.1 + 1));
        drop(started);

        let restarted = bench.start(&sections, false).await;
        assert_eq!(restarted.origins("one").await, [new.url.as_str()]);
        let before = old.calls();
        restarted.call_through(&network("one")).await;
        assert_eq!(
            old.calls(),
            before,
            "the replaced node is not reached again"
        );
    }

    /// A candidate that serves another network, refuses the chain's own
    /// rules, or cannot be reached changes nothing, stored or live.
    #[tokio::test]
    async fn a_failing_candidate_keeps_the_working_node() {
        let bench = Bench::new();
        let (working, elsewhere) = (Node::serving("one").await, Node::serving("two").await);
        let started = bench
            .start(&bench.sections(&[("one", &[&working.url])]), false)
            .await;
        let seventeen = json!(vec![working.url.clone(); 17]);
        let mut statuses = Vec::new();
        for (name, endpoints) in [
            ("one", json!([elsewhere.url])),
            ("one", json!(["http://127.0.0.1:9"])),
            ("one", seventeen),
            ("one", json!(["ws://127.0.0.1:9"])),
            ("three", json!([working.url])),
        ] {
            statuses.push(started.replace(name, endpoints).await);
        }
        let codes: Vec<StatusCode> = statuses.iter().map(|(status, _)| *status).collect();
        assert_eq!(
            codes,
            [
                StatusCode::UNPROCESSABLE_ENTITY,
                StatusCode::UNPROCESSABLE_ENTITY,
                StatusCode::BAD_REQUEST,
                StatusCode::BAD_REQUEST,
                StatusCode::NOT_FOUND,
            ]
        );
        assert_eq!(
            statuses[0].1["message"],
            "the node failed verification and the current nodes stay in use: \
             one: the node serves two, not one"
        );
        assert_eq!(started.origins("one").await, [working.url.as_str()]);
        assert_eq!(started.call_through(&network("one")).await, "one");
        assert_eq!(started.stored(&network("one")).await, [json!(working.url)]);
    }

    /// Credentials sent with a node are stored and used, and appear in no
    /// listing, refusal or `Debug`.
    #[tokio::test]
    async fn credentials_never_leave_the_server() {
        let bench = Bench::new();
        let node = Node::serving("one").await;
        let started = bench
            .start(&bench.sections(&[("one", &[&node.url])]), false)
            .await;
        let port = node.url.trim_start_matches("http://127.0.0.1:");
        let secret = json!([{
            "url": format!("http://user:s3cret@127.0.0.1:{port}/v3/s3cret?apikey=s3cret"),
        }]);
        let (status, replaced) = started.replace("one", secret.clone()).await;
        assert_eq!(status, StatusCode::OK, "verified with its credentials");
        assert_eq!(started.origins("one").await, [node.url.as_str()]);
        assert_eq!(
            started.stored(&network("one")).await,
            secret.as_array().cloned().unwrap_or_default()
        );
        let (_, refused) = started
            .replace("one", json!(["http://user:s3cret@127.0.0.1:9/?k=s3cret"]))
            .await;
        let configured =
            LiveNetwork::find(&started.state.networks, &network("one")).expect("configured");
        for text in [
            refused.to_string(),
            replaced.to_string(),
            format!("{:?}", configured.current().0),
            started.origins("one").await.join(" "),
        ] {
            assert!(!text.contains("s3cret"), "{text}");
        }
    }

    /// A network whose node failed at startup is listed as unavailable and
    /// its nodes can still be replaced, for the next start.
    #[tokio::test]
    async fn an_unavailable_network_is_listed_and_editable() {
        let bench = Bench::new();
        let one = Node::serving("one").await;
        let started = bench
            .start(
                &bench.sections(&[("one", &[&one.url]), ("two", &["http://127.0.0.1:9"])]),
                false,
            )
            .await;
        let replacement = Node::serving("two").await;
        assert_eq!(started.listed().await[1]["state"], "unavailable");
        assert!(
            LiveNetwork::find(&started.state.networks, &network("two"))
                .expect("two")
                .current()
                .1
                .is_empty()
        );
        let (status, summary) = started.replace("two", json!([replacement.url])).await;
        assert_eq!(status, StatusCode::OK, "verified");
        assert_eq!(summary["state"], "unavailable");
        assert_eq!(summary["nodes"][0]["origin"], replacement.url);
        assert_eq!(started.rescan("two").await.0, StatusCode::CONFLICT);
        assert_eq!(
            started.stored(&network("two")).await,
            [json!(replacement.url)]
        );
    }

    /// The rescan purges one network and indexes it again from its watch's
    /// birthday, while the other network, every watch and every key stay.
    #[tokio::test]
    async fn a_rescan_purges_only_its_network_and_indexes_it_again() {
        let bench = Bench::new();
        let (one_node, two_node) = (Node::serving("one").await, Node::serving("two").await);
        let sections = bench.sections(&[("one", &[&one_node.url]), ("two", &[&two_node.url])]);
        let started = bench.start(&sections, true).await;
        let (one, two) = (network("one"), network("two"));
        started.settles(&one).await;
        started.settles(&two).await;
        started
            .state
            .registry
            .register(indexing::Registration {
                id: "saved-key".to_owned(),
                address: Address::new(one.clone(), "elsewhere"),
                asset: AssetId::native(one.chain.clone()),
                label: Some("kept".to_owned()),
                secret: Some(wallets::SecretBytes::new([7; 32])),
            })
            .await
            .expect("filed");
        let since = started.line("one").inspections().len();
        let untouched = started.line("two").inspections();
        let other = started.repository.all(&two).await.expect("watches");

        let (status, summary) = started.rescan("one").await;
        assert_eq!(status, StatusCode::ACCEPTED, "{summary}");
        assert_ne!(summary["state"], "rescanning", "admissions are open again");
        started.settles(&one).await;

        let again: Vec<u64> = started.line("one").inspections()[since..]
            .iter()
            .map(|(height, _)| height.0)
            .collect();
        assert_eq!(
            again,
            [2, 3, 4, 5, 6],
            "read again from the birthday's anchor"
        );
        assert_eq!(
            started.line("two").inspections(),
            untouched,
            "the other network read nothing"
        );
        assert_eq!(started.repository.all(&two).await.expect("watches"), other);
        let watched = started.repository.all(&one).await.expect("watches");
        assert_eq!(
            watched.iter().map(|row| &row.watch).collect::<Vec<_>>(),
            [&Bench::watch(&one)]
        );
        let kept = started.state.registry.wallets().await.expect("readable");
        let kept: Vec<_> = kept
            .iter()
            .map(|wallet| (wallet.id(), wallet.label(), wallet.secret().is_some()))
            .collect();
        assert_eq!(
            kept,
            [
                ("one-watch", None, false),
                ("saved-key", Some("kept"), true),
                ("two-watch", None, false)
            ],
            "the rescan keeps every wallet, with its label and key"
        );
    }

    /// One maintenance request per network: while a rescan waits for an
    /// admitted wallet registration to finish, a second rescan and a node
    /// change are refused, and new wallets and sends on that network are not
    /// admitted.
    #[tokio::test]
    async fn a_second_rescan_is_refused_while_one_runs() {
        let bench = Bench::new();
        let (node, other) = (Node::serving("one").await, Node::serving("two").await);
        let sections = bench.sections(&[("one", &[&node.url]), ("two", &[&other.url])]);
        let started = bench.start(&sections, true).await;
        let (one, two) = (network("one"), network("two"));
        started.settles(&one).await;
        let (tag_one, tag_two): (AssetTag, AssetTag) =
            ("one".parse().expect("a tag"), "two".parse().expect("a tag"));
        let registering = started.state.wallets.admit(&tag_one).expect("open");

        let first = started.rescan("one");
        tokio::pin!(first);
        let waiting = tokio::time::timeout(Duration::from_millis(50), &mut first).await;
        assert!(
            waiting.is_err(),
            "the rescan waits for the registration it admitted"
        );
        assert_eq!(started.rescan("one").await.0, StatusCode::CONFLICT);
        let change = started.replace("one", json!([node.url])).await;
        assert_eq!(change.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            started.state.wallets.admit(&tag_one).is_err(),
            "no new send or wallet"
        );
        assert!(started.state.wallets.paused(&one));
        drop(
            started
                .state
                .wallets
                .admit(&tag_two)
                .expect("the other network admits"),
        );
        assert_eq!(started.listed().await[0]["state"], "rescanning");
        assert_eq!(
            started.rescan("two").await.0,
            StatusCode::ACCEPTED,
            "another network rescans"
        );

        drop(registering);
        assert_eq!(
            first.await.0,
            StatusCode::ACCEPTED,
            "the first rescan finishes"
        );
        started.settles(&one).await;
        assert_eq!(
            started.rescan("one").await.0,
            StatusCode::ACCEPTED,
            "and may run again"
        );
        assert!(!started.state.wallets.paused(&two));
    }
}
