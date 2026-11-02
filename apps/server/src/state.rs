//! What the routes run on: the application state `main` builds once and
//! `api::router` hands every handler. A plain struct of fields; the behavior
//! belongs to the models and the SDK values it holds.

use std::sync::Arc;

use payments::Payments;

/// The payments container, the one SQLite file every store lives in, and
/// the explorer links.
pub(crate) struct AppState {
    /// Every configured network: services, wallets, broadcast, stats.
    pub(crate) payments: Arc<Payments>,
    /// The one SQLite file: the SDK stores, the reads, the job queue and
    /// the `rpc_endpoints` rows.
    pub(crate) storage: crate::storage::Storage,
    /// Block explorer links of the networks that have one configured.
    pub(crate) explorers: crate::models::Explorers,
}
