use std::{fmt, sync::Arc};

use jsonrpsee::{
    core::{
        client::{ClientT, Error as RpcError},
        http_helpers::HttpError,
        params::BatchRequestBuilder,
        traits::ToRpcParams,
    },
    http_client::{HeaderMap, HttpClient, HttpClientBuilder, transport},
    types::ErrorObjectOwned,
};
use serde_json::{Value, value::RawValue};

use crate::{
    Batch, Bounds, BoxFuture, CallResult, Client, Error, ErrorKind, Failure, RawJson, Request,
    Statistics,
};

/// JSON-RPC over HTTP against one endpoint: one execution per call, no retry
/// and no failover. Compose [`crate::Retry`] and [`crate::Failover`] around it
/// for those. Cheap to clone; clones share the connection pool.
///
/// `Debug` shows its URL.
#[derive(Clone)]
pub struct Http {
    client: HttpClient,
    url: String,
    statistics: Statistics,
    events: Arc<crate::events::Events>,
}

impl fmt::Debug for Http {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Http")
            .field("url", &self.url)
            .finish()
    }
}

impl Http {
    /// Builds the client; does no I/O. Userinfo in `target` becomes HTTP
    /// Basic authorization; every call carries `headers`.
    pub(crate) fn build(
        target: &str,
        url: String,
        bounds: Bounds,
        headers: HeaderMap,
    ) -> Result<Self, Error> {
        let client = HttpClientBuilder::default()
            .set_headers(headers)
            .request_timeout(bounds.timeout())
            .max_request_size(bounds.max_request_bytes)
            .max_response_size(bounds.max_response_bytes)
            .build(target)
            .map_err(Error::from_rpc)?;
        Ok(Self {
            client,
            url,
            statistics: Statistics::default(),
            events: Arc::default(),
        })
    }

    /// A handle to this endpoint's attempts and outcomes, shared by every clone.
    #[must_use]
    pub fn statistics(&self) -> Statistics {
        self.statistics.clone()
    }

    /// Attaches an injected sink scoped to this node. Clones share failure grouping.
    #[must_use]
    pub fn with_logger(mut self, logger: logging::Logger) -> Self {
        self.events = Arc::new(crate::events::Events::new(logger));
        self
    }

    async fn execute(&self, request: Request) -> Result<CallResult, Error> {
        let call = request.into_call();
        let params = Params::new(call.params)?;
        self.statistics.begin();
        let result = match self
            .client
            .request::<Box<RawValue>, _>(&call.method, params)
            .await
        {
            Ok(value) => Ok(Ok(RawJson(value.get().as_bytes().to_vec()))),
            Err(RpcError::Call(error)) => Ok(Err(Failure::from(error))),
            Err(source) => Err(Error::from_rpc(source)),
        };
        self.events.outcome(self.statistics.call(&result));
        result
    }

    /// Every call is serialized before the first byte leaves, so an invalid
    /// parameter fails without contacting the node.
    async fn execute_batch(&self, batch: Batch) -> Result<Vec<CallResult>, Error> {
        let calls = batch.into_calls();
        let mut builder = BatchRequestBuilder::new();
        for call in &calls {
            builder
                .insert(call.method.as_str(), Params::new(call.params.clone())?)
                .map_err(|_| {
                    Error::new(
                        ErrorKind::InvalidRequest,
                        "JSON-RPC parameters could not be serialized",
                    )
                })?;
        }
        self.statistics.begin();
        let result = self
            .client
            .batch_request::<Box<RawValue>>(builder)
            .await
            .map_err(Error::from_rpc)
            .map(|responses| {
                responses
                    .into_iter()
                    .map(|entry| match entry {
                        Ok(value) => Ok(RawJson(value.get().as_bytes().to_vec())),
                        Err(error) => Err(Failure::from(error.into_owned())),
                    })
                    .collect()
            });
        self.events.outcome(self.statistics.batch(&result));
        result
    }
}

impl Client for Http {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        Box::pin(self.execute(request))
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        Box::pin(self.execute_batch(batch))
    }
}

#[derive(Clone)]
struct Params(Option<Box<RawValue>>);

impl Params {
    fn new(value: Value) -> Result<Self, Error> {
        if !matches!(value, Value::Array(_) | Value::Object(_)) {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "JSON-RPC parameters must be an array or object",
            ));
        }
        serde_json::value::to_raw_value(&value)
            .map(Some)
            .map(Self)
            .map_err(|_| {
                Error::new(
                    ErrorKind::InvalidRequest,
                    "JSON-RPC parameters could not be serialized",
                )
            })
    }
}

impl ToRpcParams for Params {
    fn to_rpc_params(self) -> Result<Option<Box<RawValue>>, serde_json::Error> {
        Ok(self.0)
    }
}

impl From<ErrorObjectOwned> for Failure {
    fn from(error: ErrorObjectOwned) -> Self {
        Self {
            code: error.code() as i64,
            message: error.message().to_owned(),
            data: error
                .data()
                .map(|data| RawJson(data.get().as_bytes().to_vec())),
        }
    }
}

impl Error {
    fn from_rpc(error: RpcError) -> Self {
        match error {
            RpcError::Call(error) => Self::new(
                ErrorKind::InvalidResponse,
                format!("JSON-RPC call failed with code {}", error.code()),
            ),
            RpcError::RequestTimeout => Self::new(ErrorKind::Timeout, "JSON-RPC request timed out"),
            RpcError::Transport(source) => match source.downcast_ref::<transport::Error>() {
                Some(transport::Error::Rejected { status_code }) => Self::new(
                    ErrorKind::HttpStatus(*status_code),
                    "JSON-RPC endpoint rejected the request",
                ),
                Some(transport::Error::Http(HttpError::TooLarge)) => Self::new(
                    ErrorKind::ResponseTooLarge,
                    "JSON-RPC response exceeded its configured limit",
                ),
                Some(transport::Error::Http(HttpError::Malformed)) => Self::new(
                    ErrorKind::InvalidResponse,
                    "JSON-RPC endpoint returned an invalid response",
                ),
                _ => Self::new(ErrorKind::Unavailable, "JSON-RPC transport is unavailable"),
            },
            RpcError::ParseError(_) | RpcError::InvalidRequestId(_) => Self::new(
                ErrorKind::InvalidResponse,
                "JSON-RPC endpoint returned an invalid response",
            ),
            _ => Self::new(ErrorKind::InvalidResponse, "JSON-RPC request failed"),
        }
    }
}
