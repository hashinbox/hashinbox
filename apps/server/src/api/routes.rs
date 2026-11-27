//! One module per resource, each answering its own `routes()`, merged here
//! into one router. Authentication is the router's one middleware
//! (`api::middleware`); a handler takes `Session` or `Caller`, or neither.

mod assets;
mod health;
mod jobs;
mod keys;
mod logs;
mod markets;
mod network_block_transactions;
mod network_blocks;
mod network_endpoint_routes;
mod network_endpoints;
mod network_transactions;
mod networks;
mod openapi;
mod recovery;
mod session;
mod settings;
mod stats;
mod triggers;
mod user;
mod users;
mod version;
mod wallet_index_legs;
mod wallet_indexes;
mod wallet_tags;
mod wallet_transactions;
mod wallets;

use std::sync::Arc;

use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

use crate::api::{AppState, Contract};

/// Every route.
pub(super) fn all() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::with_openapi(Contract::openapi())
        .merge(assets::routes())
        .merge(wallets::routes())
        .merge(wallet_indexes::routes())
        .merge(wallet_index_legs::routes())
        .merge(wallet_transactions::routes())
        .merge(jobs::routes())
        .merge(logs::routes())
        .merge(markets::routes())
        .merge(networks::routes())
        .merge(network_blocks::routes())
        .merge(network_endpoints::routes())
        .merge(network_endpoint_routes::routes())
        .merge(network_block_transactions::routes())
        .merge(network_transactions::routes())
        .merge(stats::routes())
        .merge(triggers::routes())
        .merge(wallet_tags::routes())
        .merge(session::routes())
        .merge(user::routes())
        .merge(users::routes())
        .merge(keys::routes())
        .merge(recovery::routes())
        .merge(settings::routes())
        .merge(health::routes())
        .merge(openapi::routes())
        .merge(version::routes())
}
