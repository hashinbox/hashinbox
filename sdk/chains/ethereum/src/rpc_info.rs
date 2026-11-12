//! [`Info`]: the most basic Ethereum JSON-RPC client, a node's tip and
//! chain ID; the full [`crate::Client`] extends it.

use std::sync::Arc;

use ::rpc::{Answer, Batch, RawJson, Request};
use alloy_primitives::U64;
use anyhow::{Context as _, ensure};
use base::{InfoClient, NodeInfo};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::Network;

/// An Ethereum node of one network, as a cluster ranks it.
pub struct Info {
    transport: Arc<dyn ::rpc::Client>,
    network: Network,
}

impl Info {
    /// The node at `transport`, of `network`.
    #[must_use]
    pub fn new(transport: Arc<dyn ::rpc::Client>, network: Network) -> Self {
        Self { transport, network }
    }

    /// The network it serves.
    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.network
    }

    /// One repeatable call, decoded.
    pub(crate) async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> anyhow::Result<T> {
        let reply = self.transport.request(Request::new(method, params)).await;
        decode(method, reply)
    }
}

/// The node it reaches, as is.
impl ::rpc::Client for Info {
    fn request<'a>(
        &'a self,
        request: Request,
    ) -> ::rpc::BoxFuture<'a, Result<RawJson, ::rpc::Error>> {
        self.transport.request(request)
    }

    fn batch<'a>(
        &'a self,
        batch: Batch,
    ) -> ::rpc::BoxFuture<'a, Result<Vec<Answer>, ::rpc::Error>> {
        self.transport.batch(batch)
    }

    fn broadcast<'a>(&'a self, request: Request) -> ::rpc::BoxFuture<'a, Result<(), ::rpc::Error>> {
        self.transport.broadcast(request)
    }
}

/// The tip is `eth_blockNumber`; the chain is `eth_chainId`.
impl InfoClient for Info {
    fn tip(&self) -> base::Future<'_, anyhow::Result<u64>> {
        Box::pin(async move {
            let tip: U64 = self.call("eth_blockNumber", json!([])).await?;
            Ok(tip.to())
        })
    }

    fn info(&self) -> base::Future<'_, anyhow::Result<NodeInfo>> {
        Box::pin(async move {
            let actual: U64 = self.call("eth_chainId", json!([])).await?;
            let expected = self.network.chain_id;
            ensure!(
                actual == U64::from(expected),
                "Ethereum node serves chain ID {actual}, not the configured {expected}"
            );
            Ok(NodeInfo {
                chain_id: expected.to_string(),
                tip: self.tip().await?,
            })
        })
    }
}

/// A call's result decoded as `T`; an error keeps its [`::rpc::Error`].
pub(crate) fn decode<T: DeserializeOwned>(
    method: &str,
    reply: Result<RawJson, ::rpc::Error>,
) -> anyhow::Result<T> {
    let raw = reply.with_context(|| format!("Ethereum {method}"))?;
    raw.deserialize()
        .with_context(|| format!("Ethereum {method} result"))
}
