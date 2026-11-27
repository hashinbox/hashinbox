//! [`Error`]: why a request or a usecase failed, after gRPC's status codes
//! and `google.rpc.Status`/`ErrorInfo`: a [`Kind`], a message, and what a
//! client acts on (a fixed [`reason`] and its metadata). It is also the body
//! of every error response: the HTTP API answers each kind with one status.

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::rejection::{JsonRejection, QueryRejection},
    http::{HeaderValue, StatusCode, header::WWW_AUTHENTICATE},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::models::IdError;

/// The fixed machine strings of [`Error::reason`]: what a client acts on.
pub(crate) mod reason {
    /// A login's email, password or two-factor code is wrong, or the
    /// current password or the code of a two-factor change (401).
    pub(crate) const WRONG_CREDENTIALS: &str = "wrong_credentials";
    /// The password was right and the user's two-factor code is needed:
    /// ask it, post again (401).
    pub(crate) const TOTP_REQUIRED: &str = "totp_required";
    /// A genuine session (or key) no longer accepted: expired, or
    /// invalidated since it was issued (401). Log in again.
    pub(crate) const SESSION_EXPIRED: &str = "session_expired";
    /// The caller lacks a scope; metadata `scope` names it (403).
    pub(crate) const MISSING_SCOPE: &str = "missing_scope";
}

/// What kind of failure an [`Error`] is: gRPC's status codes, each one
/// HTTP status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    /// Malformed input: JSON, a query, an id, a value failing its check
    /// (400).
    InvalidArgument,
    /// Well formed, but the current state refuses it: a wallet without its
    /// key, insufficient funds, a paused index (422).
    FailedPrecondition,
    /// What the input would create is kept already: the id is taken (409).
    AlreadyExists,
    /// What the input names is not kept, or not configured here (404).
    NotFound,
    /// Who is calling is not known: no session, a bad one, wrong login
    /// details (401).
    Unauthenticated,
    /// Who is calling is known, but may not: a scope it does not hold
    /// (403).
    PermissionDenied,
    /// A quota or rate limit is used up (429).
    ResourceExhausted,
    /// A node or a store did not answer in time (504).
    DeadlineExceeded,
    /// A store, a node, the queue or key generation is down (503).
    Unavailable,
    /// This server failed: its bug, its clock, or a response not matching
    /// its schema (500).
    Internal,
}

impl Kind {
    /// The HTTP status of this kind.
    pub(crate) const fn status(self) -> StatusCode {
        match self {
            Self::InvalidArgument => StatusCode::BAD_REQUEST,
            Self::FailedPrecondition => StatusCode::UNPROCESSABLE_ENTITY,
            Self::AlreadyExists => StatusCode::CONFLICT,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::PermissionDenied => StatusCode::FORBIDDEN,
            Self::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
            Self::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// A failure: its kind, its message, and what a client acts on. The body of
/// every error response.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct Error {
    /// What kind of failure: the response's status follows from it.
    pub(crate) kind: Kind,
    /// What went wrong, for people.
    pub(crate) message: String,
    /// A fixed machine string a client acts on: `wrong_credentials`,
    /// `totp_required`, `session_expired`, `missing_scope`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub(crate) reason: Option<&'static str>,
    /// Details of the reason, as strings: `{"scope": "…"}`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    #[schema(value_type = BTreeMap<String, String>)]
    pub(crate) metadata: BTreeMap<&'static str, String>,
}

impl Error {
    /// Malformed input.
    pub(crate) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(Kind::InvalidArgument, message)
    }

    /// Well formed, but the current state refuses it.
    pub(crate) fn failed_precondition(message: impl Into<String>) -> Self {
        Self::new(Kind::FailedPrecondition, message)
    }

    /// What the input would create is kept already.
    pub(crate) fn already_exists(message: impl Into<String>) -> Self {
        Self::new(Kind::AlreadyExists, message)
    }

    /// What the input names is not kept, or not configured here.
    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(Kind::NotFound, message)
    }

    /// Who is calling is not known.
    pub(crate) fn unauthenticated(message: impl Into<String>) -> Self {
        Self::new(Kind::Unauthenticated, message)
    }

    /// Who is calling may not do it.
    pub(crate) fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(Kind::PermissionDenied, message)
    }

    /// A quota or rate limit is used up.
    #[allow(dead_code, reason = "for the rate limits to come")]
    pub(crate) fn resource_exhausted(message: impl Into<String>) -> Self {
        Self::new(Kind::ResourceExhausted, message)
    }

    /// A node or a store did not answer in time.
    pub(crate) fn deadline_exceeded(message: impl Into<String>) -> Self {
        Self::new(Kind::DeadlineExceeded, message)
    }

    /// A store, a node, the queue or key generation is down.
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(Kind::Unavailable, message)
    }

    /// This server failed.
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(Kind::Internal, message)
    }

    /// The same, with `reason` (one of [`reason`]).
    #[must_use]
    pub(crate) const fn reason(mut self, reason: &'static str) -> Self {
        self.reason = Some(reason);
        self
    }

    /// The same, with metadata `key` set to `value`.
    #[must_use]
    pub(crate) fn with(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.metadata.insert(key, value.into());
        self
    }

    /// A failure of `kind` saying `message`, with no reason or metadata.
    fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            reason: None,
            metadata: BTreeMap::new(),
        }
    }
}

/// A malformed id: invalid argument.
impl From<IdError> for Error {
    fn from(error: IdError) -> Self {
        Self::invalid_argument(error.to_string())
    }
}

/// A `crypto` failure (no randomness, a hash refused): internal, as any
/// other unhandled failure.
impl From<crypto::Error> for Error {
    fn from(error: crypto::Error) -> Self {
        anyhow::Error::from(error).into()
    }
}

/// The handled failures inside an `anyhow::Error` by kind: an unconfigured
/// network is not found; a bad address, key or start an invalid argument; a
/// timeout (a node's, a feed's, a bounded wait) deadline exceeded; any other
/// failure (a store, a node, generation) unavailable.
impl From<anyhow::Error> for Error {
    fn from(error: anyhow::Error) -> Self {
        let message = format!("{error:#}");
        match error.downcast_ref::<base::WalletError>().map(|e| e.kind) {
            Some(base::WalletErrorKind::Unsupported) => Self::not_found(message),
            Some(
                base::WalletErrorKind::InvalidAddress
                | base::WalletErrorKind::InvalidSecret
                | base::WalletErrorKind::InvalidStart,
            ) => Self::invalid_argument(message),
            _ if error.chain().any(is_timeout) => Self::deadline_exceeded(message),
            _ => Self::unavailable(message),
        }
    }
}

/// Whether `cause` is a timeout: a node's JSON-RPC call, an HTTP request,
/// or a bounded wait.
fn is_timeout(cause: &(dyn std::error::Error + 'static)) -> bool {
    cause
        .downcast_ref::<rpc::Error>()
        .is_some_and(|error| error.kind() == Some(rpc::ErrorKind::Timeout))
        || cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout)
        || cause.is::<tokio::time::error::Elapsed>()
}

/// A scope the caller does not hold: permission denied, `missing_scope`
/// with metadata `scope`, logged.
impl From<scope::Denied> for Error {
    fn from(denied: scope::Denied) -> Self {
        let scope = denied.needed().to_string();
        tracing::info!(target: crate::events::AUTH_DENIED, scope);
        Self::permission_denied(denied.to_string())
            .reason(reason::MISSING_SCOPE)
            .with("scope", scope)
    }
}

/// A body that does not parse as its documented JSON: invalid argument.
impl From<JsonRejection> for Error {
    fn from(_error: JsonRejection) -> Self {
        Self::invalid_argument("request body must match the documented JSON schema")
    }
}

/// An unparsable query string: invalid argument, so the contract answers
/// JSON, never Axum's plain-text rejection.
impl From<QueryRejection> for Error {
    fn from(_error: QueryRejection) -> Self {
        Self::invalid_argument("query parameters must match the documented schema")
    }
}

/// The kind's status and the error as JSON; a 401 names the `Bearer`
/// scheme.
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = self.kind.status();
        let mut response = (status, Json(self)).into_response();
        if status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        extract::{FromRequest as _, Query},
        http::Request,
    };
    use serde_json::json;

    use super::*;

    #[test]
    fn the_body_is_kind_message_reason_metadata_each_absent_when_empty() {
        let plain = serde_json::to_value(Error::not_found("no such wallet")).unwrap();
        assert_eq!(
            plain,
            json!({"kind": "not_found", "message": "no such wallet"})
        );
        let full = Error::permission_denied("no")
            .reason(reason::MISSING_SCOPE)
            .with("scope", "com.hashinbox.wallets:read");
        assert_eq!(
            serde_json::to_value(full).unwrap(),
            json!({
                "kind": "permission_denied",
                "message": "no",
                "reason": "missing_scope",
                "metadata": {"scope": "com.hashinbox.wallets:read"},
            })
        );
    }

    #[test]
    fn each_kind_has_its_status_and_a_401_names_bearer() {
        let response = Error::unauthenticated("who").into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
        let exhausted = Error::resource_exhausted("slow down").into_response();
        assert_eq!(exhausted.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(!exhausted.headers().contains_key(WWW_AUTHENTICATE));
        assert_eq!(
            Error::already_exists("taken").kind.status(),
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn an_id_error_is_an_invalid_argument() {
        let error = crate::models::Id::new(&[""]).unwrap_err();
        assert_eq!(Error::from(error).kind, Kind::InvalidArgument);
    }

    #[test]
    fn a_wallet_error_by_its_kind_a_timeout_deadline_exceeded_else_unavailable() {
        let of = |kind| Error::from(anyhow::Error::new(base::WalletError::new(kind, "x"))).kind;
        assert_eq!(of(base::WalletErrorKind::Unsupported), Kind::NotFound);
        assert_eq!(
            of(base::WalletErrorKind::InvalidAddress),
            Kind::InvalidArgument
        );
        let timeout = rpc::Error::Transport {
            kind: rpc::ErrorKind::Timeout,
            message: "slow".to_owned(),
        };
        let wrapped = anyhow::Error::new(timeout).context("reading the tip");
        assert_eq!(Error::from(wrapped).kind, Kind::DeadlineExceeded);
        let down = anyhow::anyhow!("the store is down");
        assert_eq!(Error::from(down).kind, Kind::Unavailable);
    }

    #[test]
    fn a_denied_scope_is_permission_denied_with_missing_scope_and_the_scope() {
        let needed: scope::Scope = "com.hashinbox.wallets:read".parse().unwrap();
        let error = Error::from(scope::Denied::new(needed));
        assert_eq!(error.kind, Kind::PermissionDenied);
        assert_eq!(error.reason, Some(reason::MISSING_SCOPE));
        assert_eq!(error.metadata["scope"], "com.hashinbox.wallets:read");
    }

    #[tokio::test]
    async fn a_json_rejection_is_an_invalid_argument() {
        let request = Request::post("/").body(Body::from("{")).unwrap();
        let rejection = Json::<serde_json::Value>::from_request(request, &())
            .await
            .unwrap_err();
        let error = Error::from(rejection);
        assert_eq!(error.kind, Kind::InvalidArgument);
        assert_eq!(
            error.message,
            "request body must match the documented JSON schema"
        );
    }

    #[test]
    fn a_query_rejection_is_an_invalid_argument() {
        let uri = "/?limit=many".parse().unwrap();
        let rejection = Query::<BTreeMap<String, u32>>::try_from_uri(&uri).unwrap_err();
        let error = Error::from(rejection);
        assert_eq!(error.kind, Kind::InvalidArgument);
        assert_eq!(
            error.message,
            "query parameters must match the documented schema"
        );
    }
}
