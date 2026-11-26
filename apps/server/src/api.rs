//! The HTTP transport: one module per resource, assembled into the router and
//! the OpenAPI document. A handler reads and checks its request, calls one
//! usecase (`crate::usecase`, where the logic is) and answers its result with
//! a status, or its [`crate::error::Error`]. A shape several
//! endpoints share is a model (`crate::models`); each endpoint's own request
//! and response bodies live in its resource file.

pub(crate) mod middleware;
mod routes;
mod state;

use std::sync::Arc;

use axum::{Extension, Router, middleware::from_fn_with_state};

use utoipa::OpenApi;

use axum::http::StatusCode;
use serde::Deserialize;
use utoipa::IntoParams;

use crate::{db::Page, error::Error};
pub(crate) use state::AppState;

/// The document root: title, version, description and servers. Title and
/// description are stated here and the version is this crate's own, so a
/// published contract never carries a dependency's metadata. The title is the
/// contract's published name, not the name of the crate that serves it.
// linter:disable rust/empty-struct -- Utoipa attaches the document root to a type, not a value.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "hashinbox-api",
        description = "The hashinbox HTTP routes and OpenAPI contract over the chain-neutral \
            wallet and index abstractions. An asset is its network's own asset id \
            (`native`, or a token); no chain or asset is named in this contract."
    ),
    servers((url = "/", description = "The hashinbox server's own origin")),
    tags(
        (name = "assets", description = "Configured assets"),
        (name = "wallets", description = "Wallets and balances"),
        (name = "transactions", description = "History and sends, one payment each"),
        (name = "networks", description = "Configured networks, their indexing and RPC nodes"),
        (name = "triggers", description = "Triggers, their runs, and the tags they select by"),
        (name = "jobs", description = "The job queue: indexing, trigger rounds, broadcasts, pruning"),
        (name = "logs", description = "The kept log entries: every one, a network's, a job's"),
        (name = "markets", description = "Market data sources, their price feeds and hourly candles"),
        (name = "stats", description = "What the store holds and its last day"),
        (name = "health", description = "Readiness and indexing state"),
        (name = "auth", description = "Sessions, the logged-in user, users and keys"),
        (name = "settings", description = "The instance's settings"),
        (name = "openapi", description = "This document")
    ),
    modifiers(&Bearer)
)]
struct Contract;

/// The `bearer` security scheme every protected route names: a session
/// JWT, or a key's secret (`his/…`).
// linter:disable rust/empty-struct -- Utoipa's modifiers are types, not values.
struct Bearer;

impl utoipa::Modify for Bearer {
    fn modify(&self, document: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
        let scheme = HttpBuilder::new()
            .scheme(HttpAuthScheme::Bearer)
            .bearer_format("JWT")
            .description(Some(
                "A session's JWT (`POST /v1/session`), or a key's secret `his/…`. A GET \
                 route also takes the JWT as `?token=`.",
            ))
            .build();
        let components = document.components.get_or_insert_with(Default::default);
        components.add_security_scheme("bearer", SecurityScheme::Http(scheme));
    }
}

/// The complete OpenAPI 3 document, without binding a listener.
///
/// `hashinbox --openapi` prints exactly this, and `docs/openapi.json`
/// holds the committed copy typed clients are generated from.
#[must_use]
pub(crate) fn openapi() -> utoipa::openapi::OpenApi {
    routes::all().split_for_parts().1
}

/// The complete API router: every route under `config`'s body limit
/// ([`http_server::protected_router`], given no token), and every route
/// behind one middleware (`middleware::authenticate`) checking the bearer
/// token against `state`'s users and keys. The OpenAPI contract is built
/// from the same handlers.
///
/// # Errors
///
/// Returns the [`http_server::ConfigError`] of an invalid `config` (for
/// example a non-loopback bind without TLS termination).
pub(crate) fn router(
    state: Arc<AppState>,
    config: &http_server::Config,
) -> Result<Router, http_server::ConfigError> {
    let (routes, contract) = routes::all().split_for_parts();
    let authenticate = from_fn_with_state(state.clone(), middleware::authenticate);
    let routes = routes.layer(authenticate).with_state(state);
    let routes = http_server::protected_router(routes, config)?;
    Ok(routes.layer(Extension(Arc::new(contract))))
}

/// 201 for what a create made, 200 for what it found already kept.
const fn create_status(created: bool) -> StatusCode {
    if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    }
}

/// The paging query of every list endpoint; its filters, if any, are a
/// query of their own.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct IndexParams {
    /// `next_cursor` of the previous page; absent for the first page.
    cursor: Option<String>,
    /// Page size, 1 to 500.
    #[param(minimum = 1, maximum = 500, default = 100)]
    limit: Option<i64>,
}

impl IndexParams {
    /// The page these ask for: `limit` (default 100) checked, `cursor` read
    /// by `parse`; a 400 named after the list's `name` otherwise.
    pub(crate) fn page<K>(
        &self,
        name: &str,
        parse: impl FnOnce(&str) -> Option<K>,
    ) -> Result<Page<K>, Error> {
        let limit = self.limit.unwrap_or(100);
        if !(1..=500).contains(&limit) {
            return Err(Error::invalid_argument(format!(
                "{name} limit must be between 1 and 500"
            )));
        }
        let after = self
            .cursor
            .as_deref()
            .map(|cursor| {
                parse(cursor)
                    .ok_or_else(|| Error::invalid_argument(format!("{name} cursor is invalid")))
            })
            .transpose()?;
        Ok(Page { after, limit })
    }
}
