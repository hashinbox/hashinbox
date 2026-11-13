//! Payment server process: reads its command line (`config`), opens the
//! one SQLite file, builds `payments::Payments` over its stores from the
//! `network` rows that have a `network_index` row (each `enabled` network
//! gets one at start), with every chain registered here (the only module
//! that names a chain crate), runs each of those networks' indexer and the
//! send queue, and serves the public HTTP API (`api`, which names no chain
//! and no storage backend) beside the browser page (`ui`). The pool is `db`;
//! the tables and their statements are `models`; the stores, the queue and
//! the reads on them are `usecase`; the background jobs are `workers`.

mod api;
mod config;
mod db;
mod models;
mod ui;
mod usecase;

#[cfg(test)]
#[path = "../tests/mod.rs"]
mod tests;

use std::{fmt::Write as _, sync::Arc, time::Duration};

use api::AppState;
use config::{AnyError, Cli, Command};
use logging::{Level, Logger, kv};
use payments::Payments;
use tasks::Executor;
use usecase::{
    tasks::Queue,
    workers::{broadcast, expire, index, prune, trigger},
};

/// How often due jobs are looked for without a write.
const TICK: Duration = Duration::from_secs(1);

/// A network's nodes as one client, when the caller has them (tests map
/// their endpoint URLs to mock nodes); `None`: the chain connects its own.
pub(crate) type Connect = dyn Fn(
        base::NetworkConfig,
    ) -> base::BoxFuture<'static, anyhow::Result<Option<Arc<dyn rpc::Client>>>>
    + Send
    + Sync;

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

    let db = config.storage.open()?;
    let logger = Logger::new(Arc::new(Stderr));
    let (state, executor) = boot(db, &|_| Box::pin(async { Ok(None) }), TICK, logger).await?;
    let jobs = executor.run();
    let router = api::router(state, &server)?.merge(face.routes()?);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    // Handlers in flight drain before `serve` returns; jobs left running
    // resume on the next start.
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    jobs.stop().await;
    Ok(served?)
}

/// Everything `serve` runs, over `db`: the payments container of every
/// network with a `network_index` row (each `enabled` network gets one),
/// its nodes from `connect` or the chain's own, every recurring job, and
/// the job queues (not yet running) looking for due jobs every `tick`,
/// a trigger round's refusals going to `logger`.
///
/// # Errors
///
/// A store failure. A network that does not build, or whose node does not
/// answer, only fails its own jobs, with why.
pub(crate) async fn boot(
    db: db::Db,
    connect: &Connect,
    tick: Duration,
    logger: Logger,
) -> anyhow::Result<(Arc<AppState>, Executor<Queue>)> {
    let queue = usecase::tasks::queue(db.clone());
    let stores = Arc::new(usecase::sdk::Stores::new(db.clone()));
    // Every network with a `network_index` row, each built by its chain.
    db.write(models::network_index::db::create_enabled).await?;
    let networks = db.read(models::network::db::find_all).await?;
    let mut builder = Payments::builder()
        .with_chain::<chain_bitcoin::Config>(chain_bitcoin::CHAIN_NAME)
        .with_chain::<chain_ethereum::Config>(chain_ethereum::CHAIN_NAME)
        .with_chain::<chain_solana::Config>(chain_solana::CHAIN_NAME);
    for network in networks.iter().filter(|network| network.index.is_some()) {
        let config = base::NetworkConfig::from(network);
        if let Some(client) = connect(config.clone()).await? {
            builder = builder.with_client(config.network.clone(), client);
        }
        let nodes = models::endpoint_route::nodes(&network.endpoints, &network.routes)?;
        builder = builder
            .with_nodes(config.network.clone(), nodes)
            .with_network_config(config);
    }
    let payments = builder
        .with_storage(base::Storage {
            blocks: stores.clone(),
            indexes: stores.clone(),
            inputs: stores.clone(),
            wallets: stores.clone(),
            holdings: stores.clone(),
        })
        .build()
        .await?;
    let payments = Arc::new(payments);
    // Each recurring worker seeds its own jobs when its queue starts
    // (`Worker::before`): every network's index job steps its indexer and at
    // the tip queues a triggers job (locking the whole network), whose sends
    // are broadcast jobs (one topic per sender); every network's expire job
    // frees what pending transactions held; one prune job deletes old
    // succeeded jobs. A trigger acts through `actions`, which the API
    // checks a trigger against too.
    let actions = Arc::new(trigger::actions::registry());
    let executor = Executor::new(queue.clone(), tick)
        .add(index::Index::new(
            payments.clone(),
            db.clone(),
            queue.clone(),
        ))
        .add(expire::Expire::new(db.clone()))
        .add(prune::Prune::new(db.clone()))
        .add(broadcast::Broadcast::new(payments.clone(), db.clone()))
        .add(trigger::Round::new(
            payments.clone(),
            db.clone(),
            queue.clone(),
            actions.clone(),
            logger,
        ));
    let state = Arc::new(AppState {
        payments,
        db,
        queue,
        actions,
    });
    Ok((state, executor))
}

/// Events at `info` and above, one line each on stderr: level, event,
/// message, then each field as `key=value`.
struct Stderr;

impl logging::Log for Stderr {
    fn enabled(&self, metadata: &logging::Metadata<'_>) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &logging::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let mut line = format!("{} {} {}", record.level(), record.target(), record.args());
        let _ = record.key_values().visit(&mut Fields(&mut line));
        eprintln!("{line}");
    }

    fn flush(&self) {}
}

/// Appends each field to a line.
struct Fields<'a>(&'a mut String);

impl<'kvs> kv::VisitSource<'kvs> for Fields<'_> {
    fn visit_pair(&mut self, key: kv::Key<'kvs>, value: kv::Value<'kvs>) -> Result<(), kv::Error> {
        let _ = write!(self.0, " {key}={value}");
        Ok(())
    }
}
