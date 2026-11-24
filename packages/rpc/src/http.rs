use std::{
    fmt,
    num::{NonZeroU32, NonZeroU64},
    time::Duration,
};

use http::{HeaderMap, HeaderName, HeaderValue};
use jsonrpsee::{
    core::{
        client::{ClientT, Error as RpcError},
        http_helpers::HttpError,
        params::BatchRequestBuilder,
        traits::ToRpcParams,
    },
    http_client::{HttpClient, HttpClientBuilder, transport},
    types::ErrorObjectOwned,
};
use serde_json::{Value, value::RawValue};

use crate::{
    Answer, Batch, BoxFuture, Client, Endpoint, Error, ErrorKind, Failure, RawJson, Request, Url,
};

/// What [`Http`] reads from a node's URL: by default 15 s, 10 MiB
/// requests, 64 MiB responses and no headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpParams {
    /// `rpc.timeout_ms`: how long one execution may take.
    pub timeout_ms: u64,
    /// `rpc.max_request_bytes`: the largest request body this node accepts.
    pub max_request_bytes: u32,
    /// `rpc.max_response_bytes`: the largest response body it may return.
    pub max_response_bytes: u32,
    /// `header.<Name>`: static headers every call carries. Names ignore
    /// case, so naming one twice in any case is refused, as is a name or
    /// value HTTP does not allow.
    pub headers: HeaderMap,
}

impl Default for HttpParams {
    fn default() -> Self {
        Self {
            timeout_ms: 15_000,
            max_request_bytes: 10 * 1024 * 1024,
            max_response_bytes: 64 * 1024 * 1024,
            headers: HeaderMap::new(),
        }
    }
}

impl TryFrom<&Url> for HttpParams {
    type Error = Error;

    fn try_from(url: &Url) -> Result<Self, Error> {
        let rpc = url.params.namespace("rpc");
        let default = Self::default();
        let mut headers = HeaderMap::new();
        for (name, value) in url.params.namespace("header").iter() {
            let invalid = || crate::endpoint::invalid("JSON-RPC endpoint header is invalid");
            let name = HeaderName::try_from(name).map_err(|_| invalid())?;
            let value = HeaderValue::try_from(value).map_err(|_| invalid())?;
            if headers.insert(name, value).is_some() {
                return Err(crate::endpoint::invalid(
                    "JSON-RPC endpoint names a header twice",
                ));
            }
        }
        Ok(Self {
            timeout_ms: rpc
                .get_int::<NonZeroU64>("timeout_ms")?
                .map_or(default.timeout_ms, NonZeroU64::get),
            max_request_bytes: rpc
                .get_int::<NonZeroU32>("max_request_bytes")?
                .map_or(default.max_request_bytes, NonZeroU32::get),
            max_response_bytes: rpc
                .get_int::<NonZeroU32>("max_response_bytes")?
                .map_or(default.max_response_bytes, NonZeroU32::get),
            headers,
        })
    }
}

/// JSON-RPC over HTTP to one node: one execution per call, the node's
/// answers mapped to results, error objects and [`Error`] kinds. Nothing
/// else: [`crate::node`] stacks splitting, logging and counting on it. Cheap
/// to clone; clones share the connection pool.
///
/// `Debug` shows its URL.
#[derive(Clone)]
pub struct Http {
    client: HttpClient,
    endpoint: Endpoint,
}

impl fmt::Debug for Http {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Http")
            .field("url", &self.endpoint.url)
            .finish()
    }
}

impl Http {
    /// The client of `endpoint`, parsed as `url`, under its [`HttpParams`];
    /// does no I/O. Userinfo becomes HTTP Basic authorization.
    pub fn new(url: &Url, endpoint: Endpoint) -> Result<Self, Error> {
        let params = HttpParams::try_from(url)?;
        let client = HttpClientBuilder::default()
            .set_headers(params.headers)
            .request_timeout(Duration::from_millis(params.timeout_ms))
            .max_request_size(params.max_request_bytes)
            .max_response_size(params.max_response_bytes)
            .build(url.node.as_str())
            .map_err(translate)?;
        Ok(Self { client, endpoint })
    }

    async fn execute(&self, request: Request) -> Result<RawJson, Error> {
        let limit = request.timeout();
        let call = request.into_call();
        let params = Params::new(call.params)?;
        let sent = self
            .client
            .request::<Box<RawValue>, _>(&call.method, params);
        match within(limit, sent).await {
            Ok(value) => Ok(RawJson(value.get().as_bytes().to_vec())),
            Err(RpcError::Call(error)) => Err(Error::Remote(Failure::try_from(error)?)),
            Err(source) => Err(translate(source)),
        }
    }

    /// `batch` as one JSON-RPC batch. Every call is serialized before the
    /// first byte leaves, so an invalid parameter fails without contacting
    /// the node.
    async fn execute_batch(&self, batch: Batch) -> Result<Vec<Answer>, Error> {
        let limit = batch.timeout();
        let calls = batch.into_calls();
        let mut builder = BatchRequestBuilder::new();
        for call in &calls {
            builder
                .insert(call.method.as_str(), Params::new(call.params.clone())?)
                .map_err(|_| {
                    Error::transport(
                        ErrorKind::InvalidRequest,
                        "JSON-RPC parameters could not be serialized",
                    )
                })?;
        }
        let sent = self.client.batch_request::<Box<RawValue>>(builder);
        let responses = within(Some(limit), sent).await.map_err(translate)?;
        if responses.len() != calls.len() {
            return Err(Error::transport(
                ErrorKind::InvalidResponse,
                format!(
                    "a batch of {} calls was answered {} times",
                    calls.len(),
                    responses.len()
                ),
            ));
        }
        // A call answered twice leaves another unanswered: nothing usable.
        let mut results = Vec::new();
        for entry in responses {
            results.push(match entry {
                Ok(value) => Ok(RawJson(value.get().as_bytes().to_vec())),
                Err(error) => Err(Failure::try_from(error.into_owned())?),
            });
        }
        // One rate-limited answer fails the batch, so a retry repeats it whole.
        let limited = results
            .iter()
            .filter_map(|entry| entry.as_ref().err())
            .find(|failure| failure.is_rate_limited());
        if let Some(failure) = limited {
            return Err(Error::Remote(failure.clone()));
        }
        Ok(results)
    }
}

impl Client for Http {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        Box::pin(self.execute(request))
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        Box::pin(self.execute_batch(batch))
    }

    fn endpoint(&self) -> Option<&Endpoint> {
        Some(&self.endpoint)
    }
}

/// `sent`, given up past `limit` as a timeout; the client's own bound still
/// applies.
async fn within<T>(
    limit: Option<Duration>,
    sent: impl std::future::Future<Output = Result<T, RpcError>>,
) -> Result<T, RpcError> {
    let Some(limit) = limit else {
        return sent.await;
    };
    tokio::time::timeout(limit, sent)
        .await
        .unwrap_or(Err(RpcError::RequestTimeout))
}

#[derive(Clone)]
struct Params(Option<Box<RawValue>>);

impl Params {
    fn new(value: Value) -> Result<Self, Error> {
        if !matches!(value, Value::Array(_) | Value::Object(_)) {
            return Err(Error::transport(
                ErrorKind::InvalidRequest,
                "JSON-RPC parameters must be an array or object",
            ));
        }
        serde_json::value::to_raw_value(&value)
            .map(Some)
            .map(Self)
            .map_err(|_| {
                Error::transport(
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

/// The node's error object; jsonrpsee's placeholder for a batch call the
/// node never answered (code 0, no message) is an invalid response.
impl TryFrom<ErrorObjectOwned> for Failure {
    type Error = Error;

    fn try_from(error: ErrorObjectOwned) -> Result<Self, Error> {
        if error.code() == 0 && error.message().is_empty() {
            return Err(Error::transport(
                ErrorKind::InvalidResponse,
                "JSON-RPC answer left a call unanswered",
            ));
        }
        Ok(Self {
            code: error.code() as i64,
            message: error.message().to_owned(),
            data: error
                .data()
                .map(|data| RawJson(data.get().as_bytes().to_vec())),
        })
    }
}

/// What jsonrpsee reports, as a transport [`Error`]. A call's error object
/// is [`Error::Remote`] before this is asked; one answering a whole batch is
/// an invalid response.
fn translate(error: RpcError) -> Error {
    match error {
        RpcError::Call(error) => match Failure::try_from(error) {
            Ok(failure) => Error::transport(ErrorKind::InvalidResponse, failure.to_string()),
            Err(unanswered) => unanswered,
        },
        RpcError::RequestTimeout => {
            Error::transport(ErrorKind::Timeout, "JSON-RPC request timed out")
        }
        RpcError::Transport(source) => match source.downcast_ref::<transport::Error>() {
            Some(transport::Error::Rejected { status_code }) => Error::transport(
                ErrorKind::HttpStatus(*status_code),
                "JSON-RPC endpoint rejected the request",
            ),
            Some(transport::Error::Http(HttpError::TooLarge)) => Error::transport(
                ErrorKind::ResponseTooLarge,
                "JSON-RPC response exceeded its configured limit",
            ),
            Some(transport::Error::RequestTooLarge) => Error::transport(
                ErrorKind::InvalidRequest,
                "JSON-RPC request exceeded its configured limit",
            ),
            Some(transport::Error::Http(HttpError::Malformed)) => Error::transport(
                ErrorKind::InvalidResponse,
                "JSON-RPC endpoint returned an invalid response",
            ),
            _ => Error::transport(ErrorKind::Unavailable, "JSON-RPC transport is unavailable"),
        },
        RpcError::ParseError(_) | RpcError::InvalidRequestId(_) => Error::transport(
            ErrorKind::InvalidResponse,
            "JSON-RPC endpoint returned an invalid response",
        ),
        _ => Error::transport(ErrorKind::InvalidResponse, "JSON-RPC request failed"),
    }
}
