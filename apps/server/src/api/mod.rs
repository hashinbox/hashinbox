//! The HTTP endpoints, one module per resource, assembled into the router and
//! the OpenAPI document, and the error every endpoint answers with. A shape
//! several endpoints share is a model (`crate::models`); each endpoint's own
//! request and response bodies live in its resource file.

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

use crate::db::Page;
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
        (name = "transactions", description = "History, single transfers, and batches"),
        (name = "networks", description = "Configured networks, their indexing and RPC nodes"),
        (name = "triggers", description = "Triggers, their runs, and the tags they select by"),
        (name = "jobs", description = "The job queue: indexing, expiry, trigger rounds, broadcasts, pruning"),
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

/// The body of every error response: `message` is always present, the
/// failing transfer's index only when one transfer of a send failed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
pub(crate) struct ErrorBody {
    /// What went wrong.
    pub(crate) message: String,
    /// Zero-based index of the transfer that failed. Present only when one
    /// transfer of a send failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub(crate) failed_index: Option<usize>,
}

/// One HTTP status the API answers with; every handler failure picks one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    BadRequest,
    NotFound,
    Unprocessable,
    Internal,
    Unavailable,
}

impl Failure {
    const fn status(self) -> StatusCode {
        match self {
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Unprocessable => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Error {
    failure: Failure,
    message: String,
    failed_index: Option<usize>,
}

impl Error {
    pub(crate) fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(Failure::BadRequest, message)
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
        Self::new(Failure::Internal, message)
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(Failure::Unavailable, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(Failure::NotFound, message)
    }

    pub(crate) fn unprocessable(message: impl Into<String>) -> Self {
        Self::new(Failure::Unprocessable, message)
    }

    /// The same failure, attributed to one authored batch occurrence.
    #[must_use]
    pub(crate) fn at(self, failed_index: usize) -> Self {
        Self {
            failed_index: Some(failed_index),
            ..self
        }
    }

    fn new(failure: Failure, message: impl Into<String>) -> Self {
        Self {
            failure,
            message: message.into(),
            failed_index: None,
        }
    }
}

/// A malformed id: always a 400.
impl From<crate::models::IdError> for Error {
    fn from(error: crate::models::IdError) -> Self {
        Self::invalid_request(error.to_string())
    }
}

/// The handled failures inside an `anyhow::Error` by their status: an
/// unconfigured network is a 404, a bad address or key a 400; any other
/// failure (a store, a node, generation) is a 503.
impl From<anyhow::Error> for Error {
    fn from(error: anyhow::Error) -> Self {
        let failure = match error.downcast_ref::<base::WalletError>().map(|e| e.kind) {
            Some(base::WalletErrorKind::Unsupported) => Failure::NotFound,
            Some(base::WalletErrorKind::InvalidAddress | base::WalletErrorKind::InvalidSecret) => {
                Failure::BadRequest
            }
            _ => Failure::Unavailable,
        };
        Self::new(failure, format!("{error:#}"))
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

/// The address a wallet ID (`{chain}.{network}.{address}`, the model id) names, parsed by
/// its network's chain.
///
/// # Errors
///
/// A 400 for a malformed ID or address; a 404 for an unconfigured network.
fn wallet_address(
    container: &base::Container<crate::usecase::Transport>,
    id: &str,
) -> Result<base::Address, Error> {
    Ok(container.parse(&crate::models::wallet::WalletId::parse(id)?)?)
}

/// The asset `id` of `network` and its description.
///
/// # Errors
///
/// A 404 when the network does not configure it.
fn asset_info(
    container: &base::Container<crate::usecase::Transport>,
    network: &base::Network,
    id: &str,
) -> Result<base::AssetInfo, Error> {
    let asset = base::Asset {
        network: network.clone(),
        id: id.to_owned(),
    };
    container
        .service(network)?
        .assets
        .get(&asset)
        .cloned()
        .ok_or_else(|| Error::not_found("asset is not configured"))
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (
            self.failure.status(),
            Json(ErrorBody {
                message: self.message,
                failed_index: self.failed_index,
            }),
        )
            .into_response()
    }
}
