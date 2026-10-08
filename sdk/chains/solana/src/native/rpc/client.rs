use std::sync::Arc;

use json_rpc::Request;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::native::{Error, ErrorKind, Network};

/// A verified Solana JSON-RPC connection over the transport `C`, which may
/// reach one node or several. Cheap to clone: clones share it.
///
/// [`Client::connect`] is the only public constructor, and it checks the
/// genesis hash and the executable Memo-v3 program first, so holding a
/// `Client` proves which cluster it serves. No call is ever repeated
/// (`json_rpc::Request::once`), whatever the deployment configures: a read
/// is one attempt at the node the transport routes it to, because Solana
/// reads carry context floors that a second node may not have reached, and a
/// submission is one [`json_rpc::Client::broadcast`] that reaches every node
/// the transport holds, each exactly once. Timeouts and size limits belong to
/// the transport `C`.
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
    /// Connects over `transport` and verifies that the node reports
    /// `network`'s genesis hash and serves the executable Memo-v3 program at
    /// a finalized slot, before returning.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] for an invalid `network` or a
    /// node without Memo-v3, [`ErrorKind::Divergent`] for another cluster,
    /// and [`ErrorKind::RpcUnavailable`] when the node cannot answer.
    pub async fn connect(transport: C, network: &Network) -> Result<Self, Error> {
        network.validate()?;
        let client = Self {
            inner: Arc::new(State {
                transport,
                network: network.clone(),
            }),
        };
        client.verify_genesis().await?;
        client.verify_memo().await?;
        Ok(client)
    }

    /// One attempt of `method` at one node, decoded as `T`. Remote failures
    /// show only their code, never node text.
    pub(crate) async fn request<T>(&self, method: &str, params: Value) -> Result<T, Error>
    where
        T: DeserializeOwned,
    {
        let outcome = self
            .inner
            .transport
            .request(Request::once(method, params))
            .await;
        Self::decode(method, outcome)
    }

    /// One signed envelope submitted to every node the transport reaches,
    /// each exactly once; the first acceptance answers, decoded as `T`.
    pub(crate) async fn broadcast_once<T>(&self, method: &str, params: Value) -> Result<T, Error>
    where
        T: DeserializeOwned,
    {
        let outcome = self
            .inner
            .transport
            .broadcast(Request::once(method, params))
            .await;
        Self::decode(method, outcome)
    }

    fn decode<T: DeserializeOwned>(
        method: &str,
        outcome: Result<json_rpc::CallResult, json_rpc::Error>,
    ) -> Result<T, Error> {
        let raw = outcome?.map_err(|failure| {
            Error::rpc(
                format!("Solana RPC {method} failed with code {}", failure.code),
                true,
            )
        })?;
        raw.deserialize::<T>().map_err(|_| Error::malformed(method))
    }
}

/// A transport failure: bad configuration is terminal, everything else may
/// clear. The transport's own text may name the endpoint, so only its class
/// is kept.
impl From<json_rpc::Error> for Error {
    fn from(error: json_rpc::Error) -> Self {
        let message = format!("Solana RPC request failed: {:?}", error.kind);
        match error.kind {
            json_rpc::ErrorKind::InvalidConfiguration | json_rpc::ErrorKind::InvalidRequest => {
                Self::new(ErrorKind::InvalidConfiguration, message)
            }
            _ => Self::rpc(message, true),
        }
    }
}
