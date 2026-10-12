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
mod state;
mod storage;
mod ui;

use std::{path::PathBuf, sync::Arc};

use config::{AnyError, Config};
use models::NetworkEndpoints;
use state::AppState;
use wallets::Policy;

/// Every way the binary may be invoked; every refusal of an argument prints it.
const USAGE: &str = "usage: payment-server serve <config.toml> \
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
    /// Run the server.
    Serve {
        /// The configuration document.
        config: PathBuf,
    },
    /// Execute the schema on the configured (new) database file.
    Migrate {
        /// The configuration document.
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    let cli = <Cli as clap::Parser>::parse();
    let path = match cli.command {
        _ if cli.openapi => {
            println!("{}", serde_json::to_string_pretty(&api::openapi())?);
            return Ok(());
        }
        None => return Err(USAGE.into()),
        Some(Command::Migrate { config }) => {
            Config::read(config).await?.storage.migrate().await?;
            return Ok(());
        }
        Some(Command::Serve { config }) => config,
    };
    let config = Config::read(path).await?;
    let server = config.server()?;
    let face = ui::Settings::from_params(ui::SettingsParams {
        brand: config.ui.brand.clone(),
        api_base: Config::api_base()?,
    });

    // The database is every network's node source; the file's nodes are the
    // fallback for a network it holds none of.
    let store = config.storage.open().await?;
    let (pool, repo) = (store.pool(), Arc::new(store.indexing()));
    let builder = config.payments()?;
    let endpoints = NetworkEndpoints::find_all(&pool)
        .await?
        .or_configured(builder.endpoints()?)?;
    let mut payments = builder
        .with_endpoints(endpoints.documents())
        .with_store(repo.clone())
        .with_output_store(repo.clone())
        .with_wallet_registry(Arc::new(store.wallets()))
        .start()
        .await?;
    for notice in &payments.notices {
        println!("{notice}");
    }
    for error in &payments.skipped {
        eprintln!("skipped, its node failed verification: {error}");
    }
    // Every sending asset's sends wait in one in-memory queue; its pauses are
    // Tokio's, the one runtime this process names.
    payments.wallets.queue(&Policy {
        pause: Arc::new(|delay| Box::pin(tokio::time::sleep(delay))),
        ..Policy::default()
    });
    let state = Arc::new(AppState {
        payments,
        repo,
        pool,
    });
    let router = api::router(state.clone(), &server)?.merge(face.routes()?);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    // Stop reporting ready before HTTP stops accepting; the handlers in
    // flight, and the sends they admitted, drain before `serve` returns.
    let networks = state.payments.networks.clone();
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            networks.reset_health();
        })
        .await;
    let state = Arc::try_unwrap(state).map_err(|_| "a request still holds the server state")?;
    state.payments.shutdown().await?;
    Ok(served?)
}
