//! The server's business over its models: what more than one caller needs
//! (a worker and an endpoint, two endpoints), with nothing HTTP-shaped in it.
//! `sdk` puts `base`'s store traits on the models, `tasks` is the job queue,
//! `workers` are the background jobs with their executor, and the rest
//! combine several models into one read.

pub(crate) mod chain;
pub(crate) mod history;
pub(crate) mod sdk;
pub(crate) mod stats;
pub(crate) mod tasks;
pub(crate) mod trigger;
pub(crate) mod wallets;
pub(crate) mod workers;
