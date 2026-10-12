//! What the routes run on: the application state `main` builds once and
//! `api::router` hands every handler. A plain struct of fields; the behavior
//! belongs to the models and the SDK values it holds.

use std::sync::Arc;

use payments::Payments;

use crate::asset::AssetTag;

/// The payments container (with the keys this process signs with), the one
/// repository (whose balance, history and wallet reads the routes call), and
/// the one database pool the models' rows live in.
pub(crate) struct AppState {
    /// Every chain composed and driven: wallet factories, index, pending
    /// blocks and the networks' operational handle.
    pub(crate) payments: Payments<AssetTag>,
    /// The one repository: balance and history reads, and every wallet row.
    pub(crate) repo: Arc<indexing::sqlite::Repository>,
    /// The one SQLite pool: the `rpc_endpoints` rows.
    pub(crate) pool: sqlx::SqlitePool,
}
