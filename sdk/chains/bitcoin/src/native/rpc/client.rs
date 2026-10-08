use std::sync::Arc;

use json_rpc::{CallError, RawJson, Request};
use serde_json::Value;

use crate::native::{Error, Network};

/// Bitcoin Core's "still warming up" code: the node will answer shortly.
const WARMING_UP: i64 = -28;

/// A verified Bitcoin Core JSON-RPC connection. Cheap to clone: clones share it.
///
/// [`Client::connect`] is the only public constructor, and it checks the
/// node's chain, version, pruning, and transaction index first, so holding a
/// `Client` proves which network it serves. HTTP authentication, retry,
/// timeouts, and response limits belong to the transport `C`.
pub struct Client<C> {
    inner: Arc<State<C>>,
}

struct State<C> {
    transport: C,
    network: Network,
}

impl<C> Clone for Client<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<C> Client<C> {
    /// The network this client was verified against.
    #[must_use]
    pub fn network(&self) -> &Network {
        &self.inner.network
    }
}

impl<C: json_rpc::Client> Client<C> {
    /// Connects over `transport` and verifies that the node is an unpruned,
    /// synchronized Bitcoin Core 31.x with a transaction index, serving
    /// `network`.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::Divergent`] for another network,
    /// [`crate::native::ErrorKind::InvalidConfiguration`] for an unusable
    /// deployment, and a retryable
    /// [`crate::native::ErrorKind::RpcUnavailable`] while the node syncs.
    pub async fn connect(transport: C, network: &Network) -> Result<Self, Error> {
        let client = Self {
            inner: Arc::new(State {
                transport,
                network: *network,
            }),
        };
        client.verify().await?;
        Ok(client)
    }

    /// One call under the transport's retry policy.
    pub(crate) async fn call(&self, method: &str, params: Value) -> Result<RawJson, CallError> {
        let request = Request::new(method, params);
        Ok(self.inner.transport.request(request).await??)
    }

    /// One state-changing attempt, without transport retry or failover.
    pub(crate) async fn call_once(
        &self,
        method: &str,
        params: Value,
    ) -> Result<RawJson, CallError> {
        let request = Request::once(method, params);
        Ok(self.inner.transport.request(request).await??)
    }

    /// One signed envelope submitted to every node the transport reaches,
    /// each exactly once; the first acceptance answers.
    pub(crate) async fn broadcast_once(
        &self,
        method: &str,
        params: Value,
    ) -> Result<RawJson, CallError> {
        let request = Request::once(method, params);
        Ok(self.inner.transport.broadcast(request).await??)
    }

    /// One read call as JSON.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, Error> {
        let raw = self.call(method, params).await.map_err(Error::from_call)?;
        Ok(raw.deserialize()?)
    }

    /// One read call; `None` when the node answers with the `missing` code.
    pub(crate) async fn optional(
        &self,
        method: &str,
        params: Value,
        missing: i64,
    ) -> Result<Option<RawJson>, Error> {
        match self.call(method, params).await {
            Ok(raw) => Ok(Some(raw)),
            Err(error) if error.remote_code() == Some(missing) => Ok(None),
            Err(error) => Err(Error::from_call(error)),
        }
    }
}

impl Error {
    /// A failed call. Remote failures show only their code, never node
    /// text; warm-up and implementation-defined server errors are retryable.
    pub(crate) fn from_call(error: CallError) -> Self {
        match error {
            CallError::Transport(error) => Self::from(error),
            CallError::Remote(failure) => Self::rpc(
                format!("Bitcoin JSON-RPC request failed with code {}", failure.code),
                failure.code == WARMING_UP || failure.is_server_error(),
            ),
        }
    }
}

/// A transport failure keeps its message and the transport's retry decision.
impl From<json_rpc::Error> for Error {
    fn from(error: json_rpc::Error) -> Self {
        Self::rpc(error.to_string(), error.is_retryable())
    }
}
