//! Payment server process: reads one configuration document, opens the one
//! SQLite file, builds `payments::Payments` over its stores with every chain
//! registered here (the only module that names a chain crate), runs each
//! network's indexer and the send queue, and serves the public HTTP API
//! (`api`, which names no chain and no storage backend) beside the browser
//! page (`ui`). The tables and their statements are `models`; the stores and
//! reads on them are `storage`; the send queue is `workers`.
//!
//! A network's stored `rpc_endpoints` row replaces its section's
//! `endpoints` before the chain reads it, so a change made through the API
//! survives a restart; a section with no row seeds it on the first start.

mod api;
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
    // Each chain's `Config` reads its own section of the payments document.
    let payments = Payments::builder()
        .with_chain::<chain_bitcoin::Config>("bitcoin")
        .with_chain::<chain_ethereum::Config>("ethereum")
        .with_chain::<chain_solana::Config>("solana")
        .with_config(document)
        .with_storage(base::Storage {
            blocks: stores.clone(),
            indexes: stores.clone(),
            inputs: stores.clone(),
            wallets: stores.clone(),
        })
        .build()
        .await?;
    let payments = Arc::new(payments);
    // Every network's index job steps its indexer; at the tip it queues a
    // triggers job (locking the whole network), whose sends are broadcast
    // jobs (one topic per sender).
    let index = workers::indexer::Index::new(payments.clone(), storage.clone());
    index.start().await?;
    // Every network's expire job frees what pending transactions held.
    let expire = workers::broadcast::Expire::new(payments.clone(), storage.clone());
    expire.start().await?;
    let executor = workers::executor::Executor::new(storage.clone())
        .add(index, tasks::Retry::default())
        .add(expire, tasks::Retry::default())
        .add(
            workers::broadcast::Broadcast::new(payments.clone(), storage.clone()),
            tasks::Retry::default(),
        )
        .add(
            workers::trigger::Round::new(
                payments.clone(),
                storage.clone(),
                logging::Logger::default(),
            ),
            tasks::Retry::default(),
        );
    let jobs = tokio::spawn(executor.run());
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
    Ok(served?)
}

/// `document` with each section's `endpoints` replaced by its network's
/// stored row, matched on the section's `chain` and `network` text; a section
/// with no stored row has its `endpoints` stored, so the stored rows are the
/// one list every later change edits.
async fn stored_endpoints(
    mut document: Document,
    storage: &storage::Storage,
) -> anyhow::Result<Document> {
    let stored = storage
        .read(models::endpoint::db::find_all)
        .await?
        .documents();
    let mut seeds = Vec::new();
    for section in &mut document.networks {
        let (Some(Value::String(chain)), Some(Value::String(name))) =
            (section.get("chain"), section.get("network"))
        else {
            continue;
        };
        let network = base::Network::new(base::Chain::new(chain), name);
        if let Some(nodes) = stored.get(&network) {
            section.insert("endpoints".to_owned(), Value::Array(nodes.clone()));
        } else if let Some(Value::Array(nodes)) = section.get("endpoints") {
            let nodes = models::RpcEndpoints::new(nodes.clone()).map_err(anyhow::Error::msg)?;
            seeds.push((network, nodes));
        }
    }
    storage
        .write(move |connection| {
            for (network, nodes) in &seeds {
                models::endpoint::db::upsert(connection, network, nodes)?;
            }
            Ok(())
        })
        .await?;
    Ok(document)
}
