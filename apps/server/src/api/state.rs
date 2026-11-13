//! What the routes run on: the application state `main` builds once and
//! [`super::router`] hands every handler. A plain struct of fields; the behavior
//! belongs to the models and the SDK values it holds.

use std::sync::Arc;

/// The container, the one SQLite file and the job queue on it.
pub(crate) struct AppState {
    /// Every registered network: services and transports, wallets,
    /// broadcast, stats.
    pub(crate) container: Arc<base::Container<crate::usecase::Transport>>,
    /// The one SQLite file every model reads and writes.
    pub(crate) db: crate::db::Db,
    /// The job queue: sends are enqueued, jobs read.
    pub(crate) queue: crate::usecase::tasks::Queue,
    /// Every action a trigger may call: a trigger is checked against them.
    pub(crate) actions: Arc<trigger::actions::Registry<tasks::Job>>,
}
