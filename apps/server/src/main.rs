//! Payment server process: reads one configuration document, opens the one
//! SQLite file, builds `payments::Payments` over its stores with every chain
//! `chains` registers (the only module that names a chain), runs each
//! network's indexer and the send queue, and serves the public HTTP API
//! (`api`, which names no chain and no storage backend) beside the browser
//! page (`ui`). The tables and their statements are `models`; the stores and
//! reads on them are `storage`; the send queue is `workers`.
//!
//! A network's stored `rpc_endpoints` row replaces its section's
//! `endpoints` before the chain reads it, so a replacement made through the
//! API survives a restart.

mod api;

mod chains;
mod config;
mod models;
mod state;
mod storage;
mod ui;
mod workers;

#[cfg(test)]
#[path = "../tests/mod.rs"]
mod tests;

use std::{path::PathBuf, sync::Arc};

use config::{AnyError, Config};
use payments::{Document, Payments};
use serde_json::Value;
use state::AppState;

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

    let storage = config.storage.open()?;
    let document = stored_endpoints(config.payments.clone(), &storage).await?;
    let stores = Arc::new(storage.clone());
    let payments = chains::register(Payments::builder())
        .with_config(document)
        .with_storage(base::Storage {
            blocks: stores.clone(),
            indexes: stores.clone(),
            inputs: stores.clone(),
            transactions: stores.clone(),
            wallets: stores.clone(),
        })
        .build()
        .await?;
    let payments = Arc::new(payments);
    let indexers = payments.run();
    // Sends are durable jobs, one lane per sender, run by one runner.
    let runner = tasks::Runner::new(stores).executor(
        workers::send::KIND,
        Arc::new(workers::send::Sends::new(payments.clone())),
    );
    let jobs = tokio::spawn(async move { runner.run().await });
    let state = Arc::new(AppState { payments, storage });
    let router = api::router(state, &server)?.merge(face.routes()?);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    // Handlers in flight drain before `serve` returns; jobs left running
    // resume on the next start.
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    jobs.abort();
    indexers.stop();
    indexers.join().await?;
    Ok(served?)
}

/// `document` with each section's `endpoints` replaced by its network's
/// stored row, matched on the section's `chain` and `network` text.
async fn stored_endpoints(
    mut document: Document,
    storage: &storage::Storage,
) -> anyhow::Result<Document> {
    let stored = storage.endpoints().await?.documents();
    for section in &mut document.networks {
        let (Some(Value::String(chain)), Some(Value::String(name))) =
            (section.get("chain"), section.get("network"))
        else {
            continue;
        };
        let network = base::Network::new(base::Chain::new(chain), name);
        if let Some(nodes) = stored.get(&network) {
            section.insert("endpoints".to_owned(), Value::Array(nodes.clone()));
        }
    }
    Ok(document)
}
