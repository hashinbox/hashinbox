//! The server's business over its models, with nothing transport-shaped in
//! it: what any caller (the HTTP API, a worker, another transport) asks.
//! One file per resource (`networks`, `wallets`, `triggers`, `jobs`,
//! `stats`): functions over the container and the database (and the queue
//! or the actions where they need them) that take typed inputs and answer
//! models or plain results, or an [`Error`] by kind. Beside them, the ports:
//! `transport` is a network's nodes and the selection over them (each
//! stored node built with `decorators`: guarded, which turns one that
//! cannot recover off through its `nodes::NodeSwitch`, and counted), `logs` keeps the
//! events (`DbLayer`) and reads them back, `sdk` puts
//! `base`'s store traits on the models, `tasks` is the job queue, `trigger`
//! the trigger package's store, and `workers` are the background jobs with
//! their executor.

pub(crate) mod blocks;
mod decorators;
mod error;
pub(crate) mod indexes;
pub(crate) mod jobs;
pub(crate) mod logs;
pub(crate) mod networks;
mod nodes;
pub(crate) mod sdk;
pub(crate) mod stats;
pub(crate) mod tasks;
mod transport;
pub(crate) mod trigger;
pub(crate) mod triggers;
pub(crate) mod wallets;
pub(crate) mod workers;

pub(crate) use error::{Error, Kind};
pub(crate) use networks::Networks;
pub(crate) use transport::{NO_ENABLED_ENDPOINT, Served, Transport};

/// `network` as events name it: `chain/name`.
pub(crate) fn network_label(network: &base::Network) -> String {
    format!("{}/{}", network.chain, network.name)
}

/// The span a network's work runs in: every event inside names it
/// (`network=chain/name`).
pub(crate) fn network_span(network: &base::Network) -> tracing::Span {
    tracing::info_span!("network", network = %network_label(network))
}

/// Now, Unix ms.
pub(crate) fn now() -> i64 {
    i64::try_from(::tasks::now()).unwrap_or(i64::MAX)
}
