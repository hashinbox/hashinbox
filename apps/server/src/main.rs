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
