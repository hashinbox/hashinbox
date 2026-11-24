//! Payment server process: reads its command line (`config`), opens the
//! one SQLite file, builds the container (`base::Container`) of every
//! `enabled` `network` row, each network over its stored nodes (`usecase::Transport`)
//! and built by its chain, registered here (the only module that names a
//! chain crate); runs the workers (each network's indexer, the send queue)
//! and serves the public HTTP API (`api`, which names no chain and no
//! storage backend) beside the browser page (`ui`). The pool is `db`; the
//! tables and their statements are `models`; the stores, the queue, the
//! transports and the reads on them are `usecase`; the background jobs are
//! `workers`.

mod api;
mod config;
mod db;
mod models;
mod ui;
mod usecase;

#[cfg(test)]
#[path = "../tests/mod.rs"]
mod tests;

use std::{sync::Arc, time::Duration};

use api::AppState;
use base::Container;
use config::{AnyError, Cli, Command};
use tasks::Executor;
use usecase::workers;

/// How often due jobs are looked for without a write.
const TICK: Duration = Duration::from_secs(1);

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    let cli = <Cli as clap::Parser>::parse();
    let config = match cli.command {
        _ if cli.openapi => {
            println!("{}", serde_json::to_string_pretty(&api::openapi())?);
            return Ok(());
        }
        None => {
            return Err("usage: payment-server serve | migrate | --openapi (see --help)".into());
        }
        Some(Command::Migrate(storage)) => {
            storage.open()?.migrate().await?;
            return Ok(());
        }
        Some(Command::Serve(serve)) => serve,
    };
    let server = config.server()?;
    let face = ui::Settings::from_params(ui::SettingsParams {
        brand: config.brand.clone(),
        api_base: config::Serve::api_base()?,
    });

    // The one subscriber: stdout, and the `log` table and its live feed.
    let db = config.storage.open()?;
    let logs = usecase::logs::feed();
    tracing::subscriber::set_global_default(config.subscriber(&db, logs.clone())?)?;

    // Every `enabled` network, each over its stored nodes and built by its
    // chain; a node that cannot recover disabled in the database, every
    // node's calls counted there. A network that does not build, or whose
    // nodes do not answer, only fails its own jobs, with why.
    let container = Arc::new(Container::<usecase::Transport>::new(
        usecase::sdk::Stores::storage(db.clone()),
    ));
    usecase::Networks::new(db.clone())
        .await?
        .register::<chain_bitcoin::Info, chain_bitcoin::Factory>(&container)
        .register::<chain_ethereum::Info, chain_ethereum::Factory>(&container)
        .register::<chain_solana::Info, chain_solana::Factory>(&container);
    let state = Arc::new(AppState {
        container,
        queue: usecase::tasks::queue(db.clone()),
        actions: Arc::new(usecase::workers::trigger::actions::registry()),
        db,
        logs,
    });

    let (container, db, queue, actions) =
        (&state.container, &state.db, &state.queue, &state.actions);
    // Each recurring worker seeds its own jobs when the queue starts: the index job per
    // network (queueing trigger rounds at the tip, whose sends become broadcast jobs)
    // and one prune job.
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
    let networks = container.networks().len();
    let router = api::router(state, &server)?.merge(face.routes()?);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let bind = config.bind.to_string();
    tracing::info!(target: "app.start", networks, bind, "Server started");
    // Handlers in flight drain before `serve` returns; jobs left running
    // resume on the next start.
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    jobs.stop().await;
    tracing::info!(target: "app.stop", "Server stopped");
    Ok(served?)
}
