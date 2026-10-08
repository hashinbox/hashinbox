//! What the routes run on: the application state `main` builds once and
//! `api::router` hands every handler. A plain struct of fields; the behavior
//! belongs to the models and the SDK values it holds.

use std::sync::Arc;

use tokio::sync::watch;

use crate::{
    api::networks::{LiveNetwork, Verifier},
    asset::Wallets,
    models::Signers,
};

/// The wallet factories and the keys this process signs with, the one
/// database pool the models' rows live in (indexing's tables too, which the
/// balance and history reads query), the repository's wallet registry, the
/// index with each network's pending blocks beside it, every network's
/// indexing health, and what the network routes maintain.
///
/// Cheap to clone; the router shares one behind an `Arc`.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) wallets: Arc<Wallets>,
    pub(crate) units: Arc<base::Units>,
    /// Every key-holding wallet's signer, by wallet id.
    pub(crate) signers: Arc<Signers>,
    /// The one SQLite pool: the `rpc_endpoints` rows, and indexing's rows the
    /// balance and history reads query.
    pub(crate) pool: sqlx::SqlitePool,
    /// The one repository's wallets: every wallet with its label and key,
    /// observed or archived, the rows the index scans.
    pub(crate) registry: Arc<dyn indexing::WalletRegistry>,
    pub(crate) index: Arc<indexing::Indexers>,
    /// Every network's blocks above its confirmed line, which complete the
    /// stored balance and history reads.
    pub(crate) pending: Arc<[(base::Network, Arc<dyn indexing::Pending>)]>,
    pub(crate) health: watch::Receiver<indexing::SyncHealth>,
    /// Every configured network in configuration order, served or not; the
    /// source of the health report's RPC counters, which follow a replacement.
    pub(crate) networks: Arc<[Arc<LiveNetwork>]>,
    /// The chain's own check of a candidate node list, built where the chains
    /// are named.
    pub(crate) verify: Verifier,
    /// The supervisor's control, which runs a network's online rescan.
    pub(crate) control: indexing::Control,
    /// The one repository's network purge, the rescan's second step.
    pub(crate) purging: Arc<dyn indexing::Purging>,
}
