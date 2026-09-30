use axum::{
    Json,
    extract::rejection::{JsonRejection, PathRejection, QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The body of every error response: `message` is always present, the
/// submission fields only when they are truthfully known.
// linter:disable rust/redundant-module-prefix -- Utoipa publishes the schema under this name.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
pub(super) struct ErrorBody {
    /// What went wrong.
    pub(super) message: String,
    /// Definitely acknowledged transaction IDs before an ordered batch failed.
    /// Present only when that accepted prefix is non-empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(required = false)]
    pub(super) transaction_ids: Vec<String>,
    /// Zero-based original request index. Present only when one public batch
    /// occurrence truthfully failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub(super) failed_index: Option<usize>,
    /// Canonical ID derived from the exact locally signed envelope. Present only
    /// when its submission outcome remains unknown; its presence produces 503.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub(super) ambiguous_transaction_id: Option<String>,
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
    pub(super) fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(Failure::BadRequest, message)
    }

    /// An index page that breaks the read contract: a 503.
    pub(super) fn indexed(message: impl Into<String>) -> Self {
        Self::new(Failure::Unavailable, message)
    }

    /// An indexed amount that is not a non-negative whole number of atomic
    /// units, which no chain reports.
    pub(super) fn unscalable(error: base::DecimalError) -> Self {
        Self::indexed(format!(
            "indexed amount is not a non-negative integer: {error}"
        ))
    }

    /// An indexed asset whose unit no configured chain declared. Composition
    /// refuses to start a process whose factories leave one undeclared, so
    /// this is an index reporting an asset nothing was configured for.
    pub(super) fn undeclared() -> Self {
        Self::indexed("indexed asset has no unit declared by any configured chain")
    }

    pub(super) fn invalid_json(_error: JsonRejection) -> Self {
        Self::invalid_request("request body must match the documented JSON schema")
    }

    /// An unparsable query string: the contract answers JSON, never Axum's
    /// plain-text rejection.
    pub(super) fn invalid_query(_error: QueryRejection) -> Self {
        Self::invalid_request("query parameters must match the documented schema")
    }

    /// An unparsable path segment (an asset tag that is not a tag at all).
    pub(super) fn invalid_path(_error: PathRejection) -> Self {
        Self::invalid_request("path parameters must match the documented schema")
    }

    /// Secret text that is not key material: every supported chain writes a
    /// key as 64 hexadecimal characters, so the one parse is
    /// [`wallets::SecretBytes::from_hex`] and the curve check belongs to the
    /// asset's factory. The message never echoes what was sent.
    pub(super) fn invalid_secret() -> Self {
        Self::invalid_request("wallet secret must be hexadecimal key material")
    }

    pub(super) fn invalid_batch(failed_index: usize, message: impl Into<String>) -> Self {
        Self {
            failed_index: Some(failed_index),
            ..Self::invalid_request(message)
        }
    }

    pub(super) fn invalid_response(message: impl Into<String>) -> Self {
        Self::new(Failure::Internal, message)
    }

    pub(super) fn not_found(message: impl Into<String>) -> Self {
        Self::new(Failure::NotFound, message)
    }

    /// A request the current state refuses rather than performs: a 409.
    pub(super) fn conflict(message: impl Into<String>) -> Self {
        Self::new(Failure::Conflict, message)
    }

    /// A wallet error of an operation selected by asset tag: an unregistered
    /// tag (`Unsupported`) is `404 wallet asset is not configured`.
    pub(super) fn by_asset(error: wallets::WalletError) -> Self {
        if error.kind == wallets::WalletErrorKind::Unsupported {
            return Self::not_found("wallet asset is not configured");
        }
        error.into()
    }

    /// No network watches this id, so no route can answer for it.
    pub(super) fn unknown_wallet() -> Self {
        Self::not_found("wallet does not exist")
    }

    /// The id is watched but this process holds no key for it.
    pub(super) fn watch_only() -> Self {
        Self::new(
            Failure::Unprocessable,
            "a watch-only address has no key to read a balance or send",
        )
    }

    /// The same failure, attributed to one authored batch occurrence.
    #[must_use]
    pub(super) fn at(self, failed_index: usize) -> Self {
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

impl From<wallets::WalletError> for Error {
    fn from(error: wallets::WalletError) -> Self {
        let wallets::WalletError {
            kind,
            message,
            ambiguous_transaction_id,
        } = error;
        let failure = match kind {
            wallets::WalletErrorKind::Unsupported
            | wallets::WalletErrorKind::InvalidSecret
            | wallets::WalletErrorKind::InvalidAddress
            | wallets::WalletErrorKind::InvalidAmount
            | wallets::WalletErrorKind::InvalidBatch
            | wallets::WalletErrorKind::AddressMismatch => Failure::BadRequest,
            wallets::WalletErrorKind::Duplicate | wallets::WalletErrorKind::Conflict => {
                Failure::Conflict
            }
            wallets::WalletErrorKind::NotFound => Failure::NotFound,
            wallets::WalletErrorKind::Transaction | wallets::WalletErrorKind::WatchOnly => {
                Failure::Unprocessable
            }
            wallets::WalletErrorKind::Unavailable
            | wallets::WalletErrorKind::SourceBusy
            | wallets::WalletErrorKind::Generation => Failure::Unavailable,
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

impl From<wallets::SendError> for Error {
    fn from(error: wallets::SendError) -> Self {
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

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, response::IntoResponse};
    use base::TransactionId;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use wallets::{SendError, WalletError, WalletErrorKind};

    use super::Error;

    fn ambiguous_error(message: &str, id: &str) -> WalletError {
        WalletError {
            kind: WalletErrorKind::Transaction,
            message: message.to_owned(),
            ambiguous_transaction_id: Some(TransactionId::new(id)),
        }
    }

    async fn projection(error: Error) -> (StatusCode, Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("error response body")
            .to_bytes();
        let body = serde_json::from_slice(&bytes).expect("error response JSON");
        (status, body)
    }

    #[tokio::test]
    async fn ambiguous_failures_preserve_reconciliation_metadata() {
        let (status, body) =
            projection(ambiguous_error("single ambiguous", "single-local").into()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            json!({
                "message": "single ambiguous",
                "ambiguous_transaction_id": "single-local"
            })
        );

        let item = SendError::item(
            1,
            vec![TransactionId::new("accepted-0")],
            ambiguous_error("item ambiguous", "item-local"),
        );
        let (status, body) = projection(item.into()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            json!({
                "message": "item ambiguous",
                "transaction_ids": ["accepted-0"],
                "failed_index": 1,
                "ambiguous_transaction_id": "item-local"
            })
        );

        let grouped = SendError::grouped(
            vec![TransactionId::new("accepted-group")],
            ambiguous_error("grouped ambiguous", "grouped-local"),
        );
        let (status, body) = projection(grouped.into()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            json!({
                "message": "grouped ambiguous",
                "transaction_ids": ["accepted-group"],
                "ambiguous_transaction_id": "grouped-local"
            })
        );
    }

    #[tokio::test]
    async fn definite_operation_failures_omit_submission_metadata() {
        for (error, status, body) in [
            (
                WalletError::new(WalletErrorKind::Transaction, "definite single").into(),
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({"message": "definite single"}),
            ),
            (
                SendError::definite(WalletErrorKind::InvalidBatch, "collection failure").into(),
                StatusCode::BAD_REQUEST,
                json!({"message": "collection failure"}),
            ),
            (
                SendError::definite(WalletErrorKind::Unavailable, "operation failure").into(),
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"message": "operation failure"}),
            ),
            (
                SendError::definite(WalletErrorKind::SourceBusy, "single source is busy").into(),
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"message": "single source is busy"}),
            ),
        ] {
            assert_eq!(projection(error).await, (status, body));
        }
    }

    #[tokio::test]
    async fn definite_batch_failures_preserve_only_known_submission_metadata() {
        for (error, status, body) in [
            (
                SendError::item(
                    3,
                    Vec::new(),
                    WalletError::new(WalletErrorKind::SourceBusy, "source is busy"),
                )
                .into(),
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"message": "source is busy", "failed_index": 3}),
            ),
            (
                SendError::item(
                    2,
                    Vec::new(),
                    WalletError::new(WalletErrorKind::Transaction, "item failure"),
                )
                .into(),
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({"message": "item failure", "failed_index": 2}),
            ),
            (
                SendError::grouped(
                    vec![TransactionId::new("accepted-definite")],
                    WalletError::new(WalletErrorKind::Transaction, "grouped failure"),
                )
                .into(),
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({
                    "message": "grouped failure",
                    "transaction_ids": ["accepted-definite"]
                }),
            ),
        ] {
            assert_eq!(projection(error).await, (status, body));
        }
    }

    #[test]
    fn every_failure_maps_to_its_own_status() {
        use super::Failure;

        let statuses = [
            Failure::BadRequest,
            Failure::NotFound,
            Failure::Conflict,
            Failure::Unprocessable,
            Failure::Internal,
            Failure::Unavailable,
        ]
        .map(Failure::status);
        let mut unique = statuses.to_vec();
        unique.sort_unstable_by_key(StatusCode::as_u16);
        unique.dedup();
        assert_eq!(unique.len(), statuses.len(), "one variant per status");
        assert_eq!(statuses[0], StatusCode::BAD_REQUEST);
        assert_eq!(statuses[4], StatusCode::INTERNAL_SERVER_ERROR);
    }
}
