//! What the routes run on: the application state `main` builds once and
//! [`super::router`] hands every handler. A plain struct of fields; the behavior
//! belongs to the models and the SDK values it holds.

use std::sync::Arc;

use payments::Payments;

/// The payments container, the one SQLite file, the job queue on it, and
/// the explorer links.
pub(crate) struct AppState {
    /// Every configured network: services, wallets, broadcast, stats.
    pub(crate) payments: Arc<Payments>,
    /// The one SQLite file every model reads and writes.
    pub(crate) db: crate::db::Db,
    /// The job queue: sends are enqueued, jobs read.
    pub(crate) queue: crate::usecase::tasks::Queue,
    /// Block explorer links of the networks that have one configured.
    pub(crate) explorers: crate::models::Explorers,
}
