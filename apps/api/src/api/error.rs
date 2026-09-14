use axum::{
    Json,
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
#[schema(as = ErrorBody)]
pub struct Body {
    pub message: String,
    /// Definitely acknowledged transaction IDs before an ordered batch failed.
    /// Present only when that accepted prefix is non-empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(required = false)]
    pub transaction_ids: Vec<String>,
    /// Zero-based original request index. Present only when one public batch
    /// occurrence truthfully failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_index: Option<usize>,
    /// Canonical ID derived from the exact locally signed envelope. Present only
    /// when its submission outcome remains unknown; its presence produces 503.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ambiguous_transaction_id: Option<String>,
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    InvalidRequest,
    NotFound,
    Conflict,
    Transaction,
    Unavailable,
    InvalidResponse,
}

#[derive(Debug)]
pub struct Error {
    kind: Kind,
    message: String,
    transaction_ids: Vec<String>,
    failed_index: Option<usize>,
    ambiguous_transaction_id: Option<String>,
}

impl Error {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(Kind::InvalidRequest, message)
    }

    pub fn invalid_json(_error: JsonRejection) -> Self {
        Self::invalid_request("request body must match the documented JSON schema")
    }

    pub fn invalid_batch(failed_index: usize, message: impl Into<String>) -> Self {
        Self {
            failed_index: Some(failed_index),
            ..Self::invalid_request(message)
        }
    }

    pub fn invalid_response(message: impl Into<String>) -> Self {
        Self::new(Kind::InvalidResponse, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Kind::NotFound, message)
    }

    fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            transaction_ids: Vec::new(),
            failed_index: None,
            ambiguous_transaction_id: None,
        }
    }
}

impl From<wallets::Error> for Error {
    fn from(error: wallets::Error) -> Self {
        let wallets::Error {
            kind,
            message,
            ambiguous_transaction_id,
        } = error;
        let kind = match kind {
            wallets::ErrorKind::Unsupported
            | wallets::ErrorKind::InvalidSecret
            | wallets::ErrorKind::InvalidAddress
            | wallets::ErrorKind::InvalidAmount
            | wallets::ErrorKind::InvalidBatch
            | wallets::ErrorKind::AddressMismatch => Kind::InvalidRequest,
            wallets::ErrorKind::Duplicate | wallets::ErrorKind::Conflict => Kind::Conflict,
            wallets::ErrorKind::NotFound => Kind::NotFound,
            wallets::ErrorKind::Transaction => Kind::Transaction,
            wallets::ErrorKind::Unavailable
            | wallets::ErrorKind::SourceBusy
            | wallets::ErrorKind::Generation
            | wallets::ErrorKind::Balance
            | wallets::ErrorKind::History => Kind::Unavailable,
        };
        let mut response = Self::new(kind, message);
        response.ambiguous_transaction_id = ambiguous_transaction_id.map(|id| id.to_string());
        response
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
            match self.kind {
                Kind::InvalidRequest => StatusCode::BAD_REQUEST,
                Kind::NotFound => StatusCode::NOT_FOUND,
                Kind::Conflict => StatusCode::CONFLICT,
                Kind::Transaction => StatusCode::UNPROCESSABLE_ENTITY,
                Kind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                Kind::InvalidResponse => StatusCode::INTERNAL_SERVER_ERROR,
            }
        };
        (
            status,
            Json(Body {
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
    use base::Id;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use wallets::{Error, ErrorKind, SendError};

    use super::Error as HttpError;

    fn ambiguous_error(message: &str, id: &str) -> Error {
        Error {
            kind: ErrorKind::Transaction,
            message: message.to_owned(),
            ambiguous_transaction_id: Some(Id::new(id)),
        }
    }

    async fn projection(error: HttpError) -> (StatusCode, Value) {
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
            vec![Id::new("accepted-0")],
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
            vec![Id::new("accepted-group")],
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
                Error::new(ErrorKind::Transaction, "definite single").into(),
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({"message": "definite single"}),
            ),
            (
                SendError::collection(ErrorKind::InvalidBatch, "collection failure").into(),
                StatusCode::BAD_REQUEST,
                json!({"message": "collection failure"}),
            ),
            (
                SendError::operation(ErrorKind::Unavailable, "operation failure").into(),
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"message": "operation failure"}),
            ),
            (
                SendError::operation(ErrorKind::SourceBusy, "single source is busy").into(),
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
                    Error::new(ErrorKind::SourceBusy, "source is busy"),
                )
                .into(),
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"message": "source is busy", "failed_index": 3}),
            ),
            (
                SendError::item(
                    2,
                    Vec::new(),
                    Error::new(ErrorKind::Transaction, "item failure"),
                )
                .into(),
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({"message": "item failure", "failed_index": 2}),
            ),
            (
                SendError::grouped(
                    vec![Id::new("accepted-definite")],
                    Error::new(ErrorKind::Transaction, "grouped failure"),
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
}
