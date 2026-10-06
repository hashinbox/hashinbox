use std::sync::{Arc, atomic::AtomicU8};

use json_rpc::{Batch, Call, CallError, CallResult, RawJson, Request};
use serde_json::{Value, json};

use super::wire::parse_quantity_u64;
use crate::native::{Error, ErrorKind, Network, Wei};

/// A verified Ethereum JSON-RPC connection. Cheap to clone: clones share it.
///
/// [`Client::connect`] is the only public constructor, and it checks
/// `eth_chainId` first, so holding a `Client` proves which chain it serves.
pub struct Client<C> {
    inner: Arc<State<C>>,
}

struct State<C> {
    transport: C,
    network: Network,
    /// How this node serves receipts; discovered on the first block read.
    receipts: AtomicU8,
}

impl<C> Clone for Client<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<C> Client<C> {
    /// A client that trusts `network` without asking the node. Tests only.
    pub(crate) fn unverified(transport: C, network: Network) -> Self {
        Self {
            inner: Arc::new(State {
                transport,
                network,
                receipts: AtomicU8::new(super::blocks::RECEIPTS_UNKNOWN),
            }),
        }
    }

    /// The network this client was verified against.
    #[must_use]
    pub fn network(&self) -> &Network {
        &self.inner.network
    }

    pub(super) fn receipt_mode(&self) -> &AtomicU8 {
        &self.inner.receipts
    }
}

impl<C: json_rpc::Client> Client<C> {
    /// Connects over `transport` and verifies that the node's `eth_chainId`
    /// equals `network.chain_id` before returning.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] for an invalid `network`,
    /// [`ErrorKind::Divergent`] for another chain, and
    /// [`ErrorKind::RpcUnavailable`] when the node cannot answer.
    pub async fn connect(transport: C, network: &Network) -> Result<Self, Error> {
        network.validate()?;
        let client = Self::unverified(transport, network.clone());
        client.verify_chain_id().await?;
        Ok(client)
    }

    /// Fails with [`ErrorKind::Divergent`] unless the node serves `network().chain_id`.
    pub(crate) async fn verify_chain_id(&self) -> Result<(), Error> {
        let actual = self.rpc_u64("eth_chainId", json!([])).await?;
        let expected = self.network().chain_id;
        if actual != expected {
            return Err(Error::new(
                ErrorKind::Divergent,
                format!(
                    "Ethereum RPC chain ID {actual} does not match configured chain ID {expected}"
                ),
            ));
        }
        Ok(())
    }

    /// One call under the transport's retry policy.
    pub(crate) async fn call(&self, method: &str, params: Value) -> Result<RawJson, CallError> {
        let request = Request::new(method, params);
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

    /// Calls as one JSON-RPC batch; results in input order.
    pub(crate) async fn batch(&self, calls: Vec<(&str, Value)>) -> Result<Vec<CallResult>, Error> {
        let calls = calls
            .into_iter()
            .map(|(method, params)| Call::new(method, params))
            .collect();
        Ok(self.inner.transport.batch(Batch::new(calls)?).await?)
    }

    /// One read call; remote failures use the wallet retry policy.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<RawJson, Error> {
        self.call(method, params)
            .await
            .map_err(|error| Error::from_call(method, error))
    }

    pub(crate) async fn rpc_u64(&self, method: &str, params: Value) -> Result<u64, Error> {
        let value: String = self.request(method, params).await?.deserialize()?;
        parse_quantity_u64(&value).map_err(|message| Error::invalid_response(method, message))
    }

    pub(crate) async fn rpc_wei(&self, method: &str, params: Value) -> Result<Wei, Error> {
        let value: String = self.request(method, params).await?.deserialize()?;
        Wei::from_quantity(&value).map_err(|message| Error::invalid_response(method, message))
    }
}
