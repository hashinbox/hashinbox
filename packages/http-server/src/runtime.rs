use std::{error::Error as StdError, fmt, future::Future};

use axum::{
    Router,
    extract::{DefaultBodyLimit, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use tokio::net::TcpListener;

use crate::{BearerToken, Config, ConfigError};

/// Liveness route: the process answers `204 No Content` while it runs.
///
/// The paths of the two health routes are this crate's contract; the
/// application owns the handlers (`apps/api/src/api/health.rs`), because
/// readiness is an application fact.
pub const LIVENESS_PATH: &str = "/health/live";
/// Readiness route: `204` when the application is ready, else `503`.
pub const READINESS_PATH: &str = "/health/ready";

/// The authentication layer itself: `tower` middleware, not a step of
/// [`protected_router`].
///
/// It cannot be inlined into its one caller because `middleware::from_fn_with_
/// state` takes a function of exactly this shape and installs it per request,
/// while `protected_router` runs once at startup.
// linter:disable rust/single-use-free-function -- axum's middleware seam: a
// per-request handler installed by the one call that builds the router.
async fn require_bearer(
    State(token): State<BearerToken>,
    request: Request,
    next: Next,
) -> Response {
    if token.matches_authorization_header(request.headers().get(axum::http::header::AUTHORIZATION))
    {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [
                (axum::http::header::WWW_AUTHENTICATE, "Bearer"),
                (axum::http::header::CONTENT_TYPE, "application/json"),
            ],
            UNAUTHORIZED_BODY,
        )
            .into_response()
    }
}

/// Rejection body of [`protected_router`]'s authentication layer.
///
/// An application that documents a JSON error contract must answer every
/// rejection with it, authentication included; the single `message` member is
/// the smallest body such a contract can require. It deliberately says nothing
/// about which part of the credential was wrong.
pub const UNAUTHORIZED_BODY: &str = r#"{"message":"a valid bearer token is required"}"#;

/// Applies configured authentication and request limits without adding routes.
/// Applications that own their health resources use this to keep middleware
/// outside handlers while generating one complete transport contract.
pub fn protected_router(protected: Router, config: &Config) -> Result<Router, ConfigError> {
    config.validate()?;

    let mut protected = protected.layer(DefaultBodyLimit::max(config.limits.max_body_bytes()));
    if let Some(token) = config.bearer_token.clone() {
        protected = protected.layer(middleware::from_fn_with_state(token, require_bearer));
    }

    Ok(protected)
}

/// Stage at which [`serve`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// [`Config::validate`] rejected the configuration.
    Configuration,
    /// The listener could not bind.
    Bind,
    /// The server stopped with an I/O error.
    Serve,
}

/// A [`serve`] failure; the underlying cause is its `source()`.
#[derive(Debug)]
pub struct Error {
    /// Failed stage.
    pub kind: ErrorKind,
    /// Human-readable context.
    pub message: String,
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl Error {
    fn with_source(
        kind: ErrorKind,
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}

/// Binds and serves a configured router until `shutdown` resolves.
pub async fn serve<S>(router: Router, config: &Config, shutdown: S) -> Result<(), Error>
where
    S: Future<Output = ()> + Send + 'static,
{
    config.validate().map_err(|error| {
        Error::with_source(
            ErrorKind::Configuration,
            "invalid HTTP server configuration",
            error,
        )
    })?;
    let listener = TcpListener::bind(config.bind_addr).await.map_err(|error| {
        Error::with_source(ErrorKind::Bind, "failed to bind HTTP listener", error)
    })?;

    axum::serve(listener, router.into_make_service())
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|error| {
            Error::with_source(ErrorKind::Serve, "HTTP server stopped with an error", error)
        })
}
