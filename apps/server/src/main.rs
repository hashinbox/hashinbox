//! Payment server process: reads one configuration document, has each
//! registered chain parse its own section (`chains`, the only module that
//! names a chain), composes every chain's RPC client, indexer, and wallet
//! factories over one storage backend, and serves the public HTTP API
//! (`api`, which names no chain, no configuration reader and no storage
//! backend) while indexing catches up, with the browser page (`ui`) beside
//! it. Everything after composition goes through `wallets::Wallets` and
//! `indexing::Indexing`.

mod api;
mod chains;
mod config;
mod lock;
mod operator;
mod registry;
mod runtime;
mod settings;
mod storage;
mod ui;

use std::{env, future::IntoFuture};

use config::{AnyError, Config};
use tokio::sync::{oneshot, watch};

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
    let face = ui::Settings::from_params(ui::SettingsParams {
        brand: config.ui.brand.clone(),
        api_base: Config::api_base()?,
    });
    let bind = config.bind;

    let runtime = runtime::Runtime::new(config).await?;
    let router = api::live_router(runtime.application(), &server)?.merge(face.routes()?);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let mut synchronization = tokio::spawn(runtime.clone().run(shutdown_rx));
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let (stop_http, stop_http_rx) = oneshot::channel();
    let serving = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = stop_http_rx.await;
        })
        .into_future();
    tokio::pin!(serving);
    let (mut result, http_done, runtime_done) = tokio::select! {
        result = &mut serving => (result.map_err(Into::into), true, false),
        signal = tokio::signal::ctrl_c() => (signal.map_err(Into::into), false, false),
        _ = &mut synchronization => {
            (Err("network runtime stopped unexpectedly".into()), false, true)
        },
    };
    runtime.stop_admission();
    if !http_done {
        let _ = stop_http.send(());
        if let Err(error) = serving.await {
            result = result.and(Err(error.into()));
        }
    }
    shutdown.send_replace(true);
    if !runtime_done && let Err(error) = synchronization.await {
        result = result.and(Err(error.into()));
    }
    result
}
