//! Payment server process: reads one configuration document, has each
//! registered chain parse its own section (`chains`, the only module that
//! names a chain), composes every chain's RPC client, indexer, and wallet
//! factories over one storage backend, and serves the public HTTP API
//! (`api`, which names no chain, no configuration reader and no storage
//! backend) after each index is ready. Everything after composition goes
//! through `wallets::Wallets` and `indexing::Indexing`.

mod api;
mod chains;
mod config;
mod operator;
mod readiness;
mod registry;
mod storage;

use std::{env, future::IntoFuture, sync::Arc};

use api::{Payments, Signers};
use config::{AnyError, Config};
use indexing::Supervisor;
use indexing::SyncReport;
use tokio::sync::{oneshot, watch};
use wallets::Submissions;

/// Printed instead of serving, so a typed client can be generated without a
/// node, a database, or a bound port.
const OPENAPI_FLAG: &str = "--openapi";

/// Every way the binary may be invoked; every refusal of an argument prints it.
pub(crate) const USAGE: &str = "usage: payment-server <config.toml> \
                                [--rescan <chain>/<network> | --cancel-backfill <chain>/<network>] \
                                | payment-server --openapi";

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    let mut arguments = env::args_os().skip(1);
    let path = arguments.next().ok_or(USAGE)?;
    if path == *OPENAPI_FLAG {
        println!("{}", serde_json::to_string_pretty(&api::openapi())?);
        return Ok(());
    }
    // Before the configuration is read, so a typo never reaches a node or a store.
    let operation = operator::Operation::parse(arguments)?;
    let config = Config::read(path).await?;
    // Offline: no chain connects, no supervisor runs, no listener binds.
    if let Some(operation) = operation {
        return operation.run(&config).await;
    }
    let server = config.server()?;
    let passphrase = config.passphrase()?;
    let bind = config.bind;

    // Owns every background send task a chain hands over, so the shutdown
    // sequence below can refuse new ones and wait for the running ones.
    let submissions = Arc::new(Submissions::new());
    // Every configured chain identity is verified before storage is opened.
    let chains = config.chains()?;
    // The one safety number an operator chooses is Bitcoin's confirmation
    // depth, so every chain states what its section commits this deployment
    // to before anything connects. A library crate must not write to standard
    // output, so composition reports the configured safety policy.
    for notice in chains.notices() {
        println!("{notice}");
    }
    let registrar: Arc<dyn wallets::SubmissionRegistrar> = submissions.clone();
    let (
        wallets::Composition {
            index: indexer,
            history,
            balance,
            schedules,
            wallets,
            units,
        },
        rpc,
    ) = chains
        .compose_with_statistics(
            &config::Environment,
            || config.storage.open(),
            |store: &storage::Store| Ok(store.repository()),
            &registrar,
        )
        .await?;
    let wallets = Arc::new(wallets);
    // Unlock stored keys before serving. Restore only IDs with durable watches;
    // a key whose watch never committed remains an unpublished orphan.
    let registry = registry::Registry::open(&config.storage.resolved_path()?, passphrase).await?;
    let signers = Arc::new(Signers::new());
    registry.restore(&wallets, &indexer, &signers).await?;
    for configured in config.wallets {
        configured
            .import(&wallets, &*indexer, &signers, &registry)
            .await?;
    }
    let (shutdown, shutdown_rx) = watch::channel(false);
    let (report, mut settling) = SyncReport::watching(indexer.networks());
    let health = settling.receiver();
    // The routes watch addresses on, and read checkpoints from, the same
    // composed index the supervisor drives. Neither query they serve goes
    // through it: each is its own handle, routed per network by the composition.
    let routes_index = indexer.clone();
    let monitor = Arc::new(readiness::Health::new(Arc::clone(&report)));
    let supervisor = Supervisor::new(indexer, schedules, monitor);
    let mut synchronization = Some(tokio::spawn(supervisor.run(shutdown_rx)));

    // Every network must have settled — reached its tip once, or been
    // quarantined — before the listener binds. A quarantined chain does not
    // hold the gate: it cannot heal by itself, and the other chains work.
    // Nor does history still owed below a checkpoint, which the wallets
    // imported above can ask for: settling follows forward indexing, and the
    // owed range is reported on `GET /healthz` instead of holding the
    // listener unbound for a range a pruned node can never serve.
    let startup_result: Option<Result<(), AnyError>> = tokio::select! {
        settled = settling.settled() => {
            settled.is_none().then(|| Err("index readiness channel closed".into()))
        }
        result = synchronization.as_mut().expect("synchronization task") => {
            synchronization = None;
            Some(match result {
                Ok(()) => Err("index synchronization stopped during startup".into()),
                Err(error) => Err(error.into()),
            })
        }
        signal = tokio::signal::ctrl_c() => Some(signal.map_err(Into::into)),
    };
    if let Some(result) = startup_result {
        report.reset();
        submissions.drain().await?;
        if let Some(task) = synchronization.take() {
            let _ = shutdown.send(true);
            task.await?;
        }
        return result;
    }
    let payments = Payments {
        wallets,
        units: Arc::new(units),
        signers,
        registry,
        index: routes_index,
        history,
        balance,
        health,
        rpc,
    };
    let router = api::router(payments, &server)?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
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
    // The shutdown order, as seven consecutive statements. It was a state
    // machine whose `advance` returned an error applied with `?`, which made a
    // violated order skip every remaining stage — leaving exactly the tasks the
    // sequence existed to drain un-awaited. Straight-line code cannot skip.
    //
    // 1. Stop reporting ready, so no new work is admitted.
    report.reset();
    // 2. Ask HTTP to stop accepting.
    if !http_done {
        let _ = stop_http.send(());
    }
    // 3. Close submission registration, so no chain hands over more work.
    submissions.close();
    // 4. Drain the handlers already in flight.
    if !http_done && let Err(error) = serving.await {
        result = result.and(Err(error.into()));
    }
    // 5. Drain the submissions they registered, while indexing can still
    //    reconcile an ambiguous one.
    if let Err(error) = submissions.drain().await {
        result = result.and(Err(error.into()));
    }
    // 6. Only then stop indexing, and 7. join it.
    if let Some(task) = synchronization.take() {
        let _ = shutdown.send(true);
        if let Err(error) = task.await {
            result = result.and(Err(error.into()));
        }
    }
    result
}
