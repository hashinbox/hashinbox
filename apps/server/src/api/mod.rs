//! The HTTP endpoints, one module per resource, assembled into the router and
//! the OpenAPI document, and the error every endpoint answers with. A shape
//! several endpoints share is a model (`crate::models`); each endpoint's own
//! request and response bodies live in its resource file.

mod assets;
mod balance;
mod health;
pub(crate) mod networks;
mod openapi;
mod transactions;
mod wallets;

use std::sync::Arc;

use axum::{Extension, Router};

use utoipa::{
    Modify, OpenApi,
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
};
use utoipa_axum::router::OpenApiRouter;

use axum::{
    Json,
    extract::rejection::{JsonRejection, QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::state::AppState;

/// The document root: title, version, description, servers, and the one
/// `bearerAuth` security scheme the protected operations declare. Title and
/// description are stated here and the version is this crate's own, so a
/// published contract never carries a dependency's metadata. The title is the
/// contract's published name, not the name of the crate that serves it.
// linter:disable rust/empty-struct -- Utoipa attaches the document root to a type, not a value.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "payment-api",
        description = "The payment HTTP routes and OpenAPI contract over the chain-neutral \
            wallet and index abstractions. Every asset is an open tag; no chain or asset is \
            named in this contract."
    ),
    servers((url = "/", description = "The payment server's own origin")),
    modifiers(&BearerAuth),
    tags(
        (name = "assets", description = "Configured assets"),
        (name = "wallets", description = "Wallets and balances"),
        (name = "transactions", description = "History, single transfers, and batches"),
        (name = "networks", description = "Configured networks, their RPC nodes, and rescans"),
        (name = "health", description = "Readiness, network state, and RPC statistics"),
        (name = "openapi", description = "This document")
    )
)]
struct Contract;

/// Declares the `bearerAuth` scheme the protected operations reference.
// linter:disable rust/empty-struct -- Utoipa's Modify contract is a trait on a type.
struct BearerAuth;

impl BearerAuth {
    /// The one credential the protected routes accept.
    fn scheme() -> SecurityScheme {
        SecurityScheme::Http(
            HttpBuilder::new()
                .scheme(HttpAuthScheme::Bearer)
                .description(Some(
                    "The configured bearer token; health and this document are public.",
                ))
                .build(),
        )
    }
}

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        // Cargo always defines CARGO_PKG_LICENSE, so Utoipa's env defaults
        // publish an empty license object for a crate that declares none.
        openapi.info.license = None;
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme("bearerAuth", Self::scheme());
    }
}

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
    state: AppState,
    config: &http_server::Config,
) -> Result<Router, http_server::ConfigError> {
    let state = Arc::new(state);
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
        .merge(balance::routes())
        .merge(transactions::routes())
        .merge(networks::routes())
}

fn public_routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .merge(health::routes())
        .merge(openapi::routes())
}

/// The body of every error response: `message` is always present, the
/// submission fields only when they are truthfully known.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
pub(crate) struct ErrorBody {
    /// What went wrong.
    pub(crate) message: String,
    /// Definitely acknowledged transaction IDs before an ordered batch failed.
    /// Present only when that accepted prefix is non-empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(required = false)]
    pub(crate) transaction_ids: Vec<String>,
    /// Zero-based original request index. Present only when one public batch
    /// occurrence truthfully failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub(crate) failed_index: Option<usize>,
    /// Canonical ID derived from the exact locally signed envelope. Present only
    /// when its submission outcome remains unknown; its presence produces 503.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub(crate) ambiguous_transaction_id: Option<String>,
}

/// One HTTP status the API answers with; every handler failure picks one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    BadRequest,
    NotFound,
    Conflict,
    Unprocessable,
    Internal,
    Unavailable,
}

impl Failure {
    const fn status(self) -> StatusCode {
        match self {
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict => StatusCode::CONFLICT,
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
    transaction_ids: Vec<String>,
    failed_index: Option<usize>,
    ambiguous_transaction_id: Option<String>,
}

impl Error {
    pub(crate) fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(Failure::BadRequest, message)
    }

    /// An index page that breaks the read contract: a 503.
    pub(crate) fn indexed(message: impl Into<String>) -> Self {
        Self::new(Failure::Unavailable, message)
    }

    /// An indexed amount that is not a non-negative whole number of atomic
    /// units, which no chain reports.
    pub(crate) fn unscalable(error: base::DecimalError) -> Self {
        Self::indexed(format!(
            "indexed amount is not a non-negative integer: {error}"
        ))
    }

    /// An indexed asset whose unit no configured chain declared. Composition
    /// refuses to start a process whose factories leave one undeclared, so
    /// this is an index reporting an asset nothing was configured for.
    pub(crate) fn undeclared() -> Self {
        Self::indexed("indexed asset has no unit declared by any configured chain")
    }

    pub(crate) fn invalid_json(_error: JsonRejection) -> Self {
        Self::invalid_request("request body must match the documented JSON schema")
    }

    /// An unparsable query string: the contract answers JSON, never Axum's
    /// plain-text rejection.
    pub(crate) fn invalid_query(_error: QueryRejection) -> Self {
        Self::invalid_request("query parameters must match the documented schema")
    }

    pub(crate) fn invalid_batch(failed_index: usize, message: impl Into<String>) -> Self {
        Self {
            failed_index: Some(failed_index),
            ..Self::invalid_request(message)
        }
    }

    pub(crate) fn invalid_response(message: impl Into<String>) -> Self {
        Self::new(Failure::Internal, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(Failure::NotFound, message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(Failure::Conflict, message)
    }

    /// A network a rescan cannot touch: nothing of it was composed.
    pub(crate) fn not_in_service() -> Self {
        Self::conflict("network is not in service")
    }

    pub(crate) fn unprocessable(message: impl Into<String>) -> Self {
        Self::new(Failure::Unprocessable, message)
    }

    /// A wallet error of an operation selected by asset tag: an unregistered
    /// tag (`Unsupported`) is `404 wallet asset is not configured`.
    pub(crate) fn by_asset(error: ::wallets::WalletError) -> Self {
        if error.kind == ::wallets::WalletErrorKind::Unsupported {
            return Self::not_found("wallet asset is not configured");
        }
        error.into()
    }

    /// The network is being rescanned: no watch is registered and no send
    /// starts on it until indexing resumes.
    pub(crate) fn rescanning() -> Self {
        Self::conflict("the network is being rescanned; retry once it is indexing again")
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
            transaction_ids: Vec::new(),
            failed_index: None,
            ambiguous_transaction_id: None,
        }
    }
}

impl From<::wallets::WalletError> for Error {
    fn from(error: ::wallets::WalletError) -> Self {
        let ::wallets::WalletError {
            kind,
            message,
            ambiguous_transaction_id,
        } = error;
        let failure = match kind {
            ::wallets::WalletErrorKind::Unsupported
            | ::wallets::WalletErrorKind::InvalidSecret
            | ::wallets::WalletErrorKind::InvalidAddress
            | ::wallets::WalletErrorKind::InvalidAmount
            | ::wallets::WalletErrorKind::InvalidBatch
            | ::wallets::WalletErrorKind::AddressMismatch => Failure::BadRequest,
            ::wallets::WalletErrorKind::Duplicate | ::wallets::WalletErrorKind::Conflict => {
                Failure::Conflict
            }
            ::wallets::WalletErrorKind::NotFound => Failure::NotFound,
            ::wallets::WalletErrorKind::Transaction | ::wallets::WalletErrorKind::WatchOnly => {
                Failure::Unprocessable
            }
            ::wallets::WalletErrorKind::Unavailable
            | ::wallets::WalletErrorKind::SourceBusy
            | ::wallets::WalletErrorKind::Generation => Failure::Unavailable,
            // This server pauses a network only to rescan it.
            ::wallets::WalletErrorKind::Paused => return Self::rescanning(),
        };
        let mut response = Self::new(failure, message);
        response.ambiguous_transaction_id = ambiguous_transaction_id.map(|id| id.to_string());
        response
    }
}

/// The index's own failures, with the same status mapping the wallet errors
/// they used to travel through had.
impl From<indexing::IndexError> for Error {
    fn from(error: indexing::IndexError) -> Self {
        let failure = match error.kind {
            indexing::IndexErrorKind::InvalidRequest => Failure::BadRequest,
            indexing::IndexErrorKind::NetworkMismatch => Failure::NotFound,
            indexing::IndexErrorKind::Conflict => Failure::Conflict,
            _ => Failure::Unavailable,
        };
        Self::new(failure, error.message)
    }
}

impl From<::wallets::SendError> for Error {
    fn from(error: ::wallets::SendError) -> Self {
        let mut response = Self::from(error.source);
        response.transaction_ids = error
            .accepted
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        response.failed_index = error.failed_index;
        response.ambiguous_transaction_id = error.ambiguous_transaction_id.map(|id| id.to_string());
        response
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = if self.ambiguous_transaction_id.is_some() {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            self.failure.status()
        };
        (
            status,
            Json(ErrorBody {
                message: self.message,
                transaction_ids: self.transaction_ids,
                failed_index: self.failed_index,
                ambiguous_transaction_id: self.ambiguous_transaction_id,
            }),
        )
            .into_response()
    }
}
