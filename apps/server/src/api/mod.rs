//! The HTTP transport: one module per resource, assembled into the router and
//! the OpenAPI document, and the error every endpoint answers with. A handler
//! reads and checks its request, calls one usecase (`crate::usecase`, where
//! the logic is) and answers its result with a status. A shape several
//! endpoints share is a model (`crate::models`); each endpoint's own request
//! and response bodies live in its resource file.

mod assets;
mod health;
mod jobs;
mod network_block_transactions;
mod network_blocks;
mod network_endpoint_routes;
mod network_endpoints;
mod network_transactions;
mod networks;
mod networks_tip;
mod openapi;
mod state;
mod stats;
mod triggers;
mod wallet_index_legs;
mod wallet_indexes;
mod wallet_tags;
mod wallet_transactions;
mod wallets;

use std::sync::Arc;

use axum::{Extension, Router};

use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

use axum::{
    Json,
    extract::rejection::{JsonRejection, QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::{db::Page, usecase};
pub(crate) use state::AppState;

/// The document root: title, version, description and servers. Title and
/// description are stated here and the version is this crate's own, so a
/// published contract never carries a dependency's metadata. The title is the
/// contract's published name, not the name of the crate that serves it.
// linter:disable rust/empty-struct -- Utoipa attaches the document root to a type, not a value.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "payment-api",
        description = "The payment HTTP routes and OpenAPI contract over the chain-neutral \
            wallet and index abstractions. An asset is its network's own asset id \
            (`native`, or a token); no chain or asset is named in this contract."
    ),
    servers((url = "/", description = "The payment server's own origin")),
    tags(
        (name = "assets", description = "Configured assets"),
        (name = "wallets", description = "Wallets and balances"),
        (name = "transactions", description = "History and sends, one payment each"),
        (name = "networks", description = "Configured networks, their indexing and RPC nodes"),
        (name = "triggers", description = "Triggers, their runs, and the tags they select by"),
        (name = "jobs", description = "The job queue: indexing, trigger rounds, broadcasts, pruning"),
        (name = "stats", description = "What the store holds and its last day"),
        (name = "health", description = "Readiness and indexing state"),
        (name = "openapi", description = "This document")
    )
)]
struct Contract;

/// The complete OpenAPI 3 document, without binding a listener.
///
/// `payment-server --openapi` prints exactly this, and `docs/openapi.json`
/// holds the committed copy typed clients are generated from.
#[must_use]
pub(crate) fn openapi() -> utoipa::openapi::OpenApi {
    let (_, mut document) = protected_routes().split_for_parts();
    let (_, public) = public_routes().split_for_parts();
    document.merge(public);
    document
}

/// The complete API router.
///
/// Wallet, transaction and network routes are protected by `config`'s bearer
/// authentication and body limit ([`http_server::protected_router`]); health
/// routes and `GET /openapi.json` are public. The OpenAPI contract is built
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
    let (protected, mut contract) = protected_routes().split_for_parts();
    let (public, public_contract) = public_routes().split_for_parts();
    contract.merge(public_contract);
    let contract = Arc::new(contract);

    let protected = http_server::protected_router(protected.with_state(state.clone()), config)?;
    Ok(protected
        .merge(public.with_state(state))
        .layer(Extension(contract)))
}

fn protected_routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::with_openapi(Contract::openapi())
        .merge(assets::routes())
        .merge(wallets::routes())
        .merge(wallet_indexes::routes())
        .merge(wallet_index_legs::routes())
        .merge(wallet_transactions::routes())
        .merge(jobs::routes())
        .merge(networks::routes())
        .merge(network_blocks::routes())
        .merge(network_endpoints::routes())
        .merge(network_endpoint_routes::routes())
        .merge(network_block_transactions::routes())
        .merge(network_transactions::routes())
        .merge(networks_tip::routes())
        .merge(stats::routes())
        .merge(triggers::routes())
        .merge(wallet_tags::routes())
}

fn public_routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .merge(health::routes())
        .merge(openapi::routes())
}

/// The body of every error response.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
pub(crate) struct ErrorBody {
    /// What went wrong.
    pub(crate) message: String,
}

/// An endpoint's failure: its status and body.
#[derive(Debug)]
pub(crate) struct Error {
    status: StatusCode,
    body: ErrorBody,
}

impl Error {
    pub(crate) fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub(crate) fn invalid_json(_error: JsonRejection) -> Self {
        Self::invalid_request("request body must match the documented JSON schema")
    }

    /// An unparsable query string: the contract answers JSON, never Axum's
    /// plain-text rejection.
    pub(crate) fn invalid_query(_error: QueryRejection) -> Self {
        Self::invalid_request("query parameters must match the documented schema")
    }

    pub(crate) fn invalid_response(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ErrorBody {
                message: message.into(),
            },
        }
    }
}

/// A malformed id: always a 400.
impl From<crate::models::IdError> for Error {
    fn from(error: crate::models::IdError) -> Self {
        Self::invalid_request(error.to_string())
    }
}

/// A usecase failure by its kind: invalid 400, not found 404, conflict 422,
/// unavailable 503, internal 500.
impl From<usecase::Error> for Error {
    fn from(error: usecase::Error) -> Self {
        let status = match error.kind {
            usecase::Kind::Invalid => StatusCode::BAD_REQUEST,
            usecase::Kind::NotFound => StatusCode::NOT_FOUND,
            usecase::Kind::Conflict => StatusCode::UNPROCESSABLE_ENTITY,
            usecase::Kind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            usecase::Kind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = ErrorBody {
            message: error.message,
        };
        Self { status, body }
    }
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
            return Err(Error::invalid_request(format!(
                "{name} limit must be between 1 and 500"
            )));
        }
        let after = self
            .cursor
            .as_deref()
            .map(|cursor| {
                parse(cursor)
                    .ok_or_else(|| Error::invalid_request(format!("{name} cursor is invalid")))
            })
            .transpose()?;
        Ok(Page { after, limit })
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}
