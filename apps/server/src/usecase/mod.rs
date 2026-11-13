//! The server's business over its models, with nothing transport-shaped in
//! it: what any caller (the HTTP API, a worker, another transport) asks.
//! One file per resource (`networks`, `wallets`, `triggers`, `jobs`,
//! `stats`): functions over the container and the database (and the queue
//! or the actions where they need them) that take typed inputs and answer
//! models or plain results, or an [`Error`] by kind. Beside them, the ports:
//! `transport` is a network's nodes and the selection over them, `sdk` puts
//! `base`'s store traits on the models, `tasks` is the job queue, `trigger`
//! the trigger package's store, and `workers` are the background jobs with
//! their executor.

mod error;
pub(crate) mod jobs;
pub(crate) mod networks;
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
pub(crate) use transport::Transport;
