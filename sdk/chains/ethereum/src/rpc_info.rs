//! [`Info`]: the most basic Ethereum JSON-RPC client, a node's tip and
//! chain ID; the full [`crate::Client`] extends it.

use std::sync::Arc;

use ::rpc::{Answer, Batch, RawJson, Request};
use alloy_primitives::U64;
use anyhow::Context as _;
use base::{InfoClient, NodeInfo};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// An Ethereum node, read for its tip and chain ID.
pub struct Info {
    transport: Arc<dyn ::rpc::Client>,
}

impl Info {
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
    fn new(transport: Arc<dyn ::rpc::Client>) -> Self {
        Self { transport }
    }

    fn tip(&self) -> base::Future<'_, anyhow::Result<u64>> {
        Box::pin(async move {
            let tip: U64 = self.call("eth_blockNumber", json!([])).await?;
            Ok(tip.to())
        })
    }

    fn info(&self) -> base::Future<'_, anyhow::Result<NodeInfo>> {
        Box::pin(async move {
            let chain_id: U64 = self.call("eth_chainId", json!([])).await?;
            Ok(NodeInfo {
                chain_id: chain_id.to_string(),
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
