//! [`Rest`]: a Tron node's transport. It takes calls as JSON-RPC names them
//! (`getblock`, `broadcasthex`) and sends each as the HTTP API's REST POST,
//! so the `rpc` decorators (retries, failover, throttle, logs, counts) wrap
//! it as they wrap `rpc::Http`.

use std::{fmt, time::Duration};

use ::rpc::{
    Answer, Batch, BoxFuture, Client, Endpoint, Error, ErrorKind, Failure, HttpParams, RawJson,
    Request, Url,
};
use serde_json::Value;

/// The code of a node's refusal: JSON-RPC's generic server error.
pub const REFUSAL: i64 = -32_000;

/// The REST path of each method this crate calls; `None`: not one.
fn path(method: &str) -> Option<&'static str> {
    Some(match method {
        "getblock" => "/wallet/getblock",
        "getblockbynum" => "/wallet/getblockbynum",
        "gettransactioninfobyblocknum" => "/wallet/gettransactioninfobyblocknum",
        "getchainparameters" => "/wallet/getchainparameters",
        "broadcasthex" => "/wallet/broadcasthex",
        "solidity_getblock" => "/walletsolidity/getblock",
        _ => return None,
    })
}

/// One Tron node over its HTTP API: a method's params POSTed as the JSON
/// body to the node's URL with the method's path appended (the query
/// kept), the answer the JSON body. A body with an `Error` text, or with
/// `result: false` (a refused broadcast: `"{code}: {message}"`), is the
/// node's refusal ([`Error::Remote`], code [`REFUSAL`], the body its data).
/// A batch runs call by call. Headers, timeout and size limits are the
/// URL's [`HttpParams`], as `rpc::Http` reads them. Cheap to clone.
///
/// `Debug` shows its URL.
#[derive(Clone)]
pub struct Rest {
    client: reqwest::Client,
    url: reqwest::Url,
    timeout: Duration,
    max_request_bytes: usize,
    max_response_bytes: usize,
    endpoint: Endpoint,
}

impl fmt::Debug for Rest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Rest")
            .field("url", &self.endpoint.url)
            .finish()
    }
}

impl Rest {
    /// The transport of `endpoint`, parsed as `url`; does no I/O.
    ///
    /// # Errors
    ///
    /// An invalid param ([`ErrorKind::InvalidConfiguration`]).
    pub fn new(url: &Url, endpoint: Endpoint) -> Result<Self, Error> {
        let params = HttpParams::try_from(url)?;
        let timeout = Duration::from_millis(params.timeout_ms);
        let client = reqwest::Client::builder()
            .default_headers(params.headers)
            .timeout(timeout)
            .build()
            .map_err(|_| {
                transport(
                    ErrorKind::InvalidConfiguration,
                    "Tron HTTP client could not be built",
                )
            })?;
        let size = |bytes: u32| usize::try_from(bytes).unwrap_or(usize::MAX);
        Ok(Self {
            client,
            url: url.node.clone(),
            timeout,
            max_request_bytes: size(params.max_request_bytes),
            max_response_bytes: size(params.max_response_bytes),
            endpoint,
        })
    }

    async fn execute(&self, request: Request) -> Result<RawJson, Error> {
        let timeout = request
            .timeout()
            .map_or(self.timeout, |t| t.min(self.timeout));
        let call = request.into_call();
        let Some(path) = path(&call.method) else {
            return Err(transport(
                ErrorKind::InvalidRequest,
                format!("Tron HTTP API has no method `{}`", call.method),
            ));
        };
        if !call.params.is_object() {
            return Err(transport(
                ErrorKind::InvalidRequest,
                "Tron HTTP API params must be an object",
            ));
        }
        let body = call.params.to_string().into_bytes();
        if body.len() > self.max_request_bytes {
            return Err(transport(
                ErrorKind::InvalidRequest,
                "Tron request exceeded its configured limit",
            ));
        }
        let mut url = self.url.clone();
        url.set_path(&format!("{}{path}", url.path().trim_end_matches('/')));
        let mut response = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .timeout(timeout)
            .body(body)
            .send()
            .await
            .map_err(failed)?;
        let status = response.status();
        if !status.is_success() {
            return Err(transport(
                ErrorKind::HttpStatus(status.as_u16()),
                "Tron node rejected the request",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(failed)? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > self.max_response_bytes {
                return Err(transport(
                    ErrorKind::ResponseTooLarge,
                    "Tron response exceeded its configured limit",
                ));
            }
        }
        answer(bytes)
    }

    /// Each call on its own, in order: an error object stays with its call;
    /// a rate limit or a transport error fails the batch.
    async fn execute_batch(&self, batch: Batch) -> Result<Vec<Answer>, Error> {
        let (timeout, repetition) = (batch.timeout(), batch.repetition());
        let mut answers = Vec::new();
        for call in batch.into_calls() {
            let request = match repetition {
                ::rpc::Repetition::Permitted => Request::new(call.method, call.params),
                ::rpc::Repetition::Forbidden => Request::once(call.method, call.params),
            };
            answers.push(match self.execute(request.with_timeout(timeout)).await {
                Ok(raw) => Ok(raw),
                Err(Error::Remote(failure)) if !failure.is_rate_limited() => Err(failure),
                Err(error) => return Err(error),
            });
        }
        Ok(answers)
    }
}

impl Client for Rest {
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

/// A body as the answer: JSON, unless the node refused ([`Rest`]).
fn answer(bytes: Vec<u8>) -> Result<RawJson, Error> {
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        transport(
            ErrorKind::InvalidResponse,
            "Tron node returned an invalid response",
        )
    })?;
    let text = |key| value.get(key).and_then(Value::as_str).unwrap_or_default();
    let message = match (value.get("Error"), value.get("result")) {
        (Some(Value::String(error)), _) => error.clone(),
        (_, Some(Value::Bool(false))) => format!("{}: {}", text("code"), text("message")),
        _ => return RawJson::new(bytes),
    };
    Err(Error::Remote(Failure {
        code: REFUSAL,
        message,
        data: Some(RawJson::new(bytes)?),
    }))
}

/// What reqwest reports, as a transport [`Error`].
fn failed(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        transport(ErrorKind::Timeout, "Tron request timed out")
    } else if error.is_decode() || error.is_body() {
        transport(
            ErrorKind::InvalidResponse,
            "Tron node returned an invalid response",
        )
    } else {
        transport(ErrorKind::Unavailable, "Tron node is unavailable")
    }
}

fn transport(kind: ErrorKind, message: impl Into<String>) -> Error {
    Error::Transport {
        kind,
        message: message.into(),
    }
}
