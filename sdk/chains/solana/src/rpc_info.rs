//! [`Info`]: the most basic Solana JSON-RPC client, a node's tip and
//! genesis hash; the full [`crate::Client`] extends it.

use std::sync::Arc;

use ::rpc::{Answer, Batch, RawJson, Request};
use anyhow::{Context as _, ensure};
use base::{InfoClient, NodeInfo};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::Network;

/// Reads see blocks a supermajority voted on.
pub(crate) const CONFIRMED: &str = "confirmed";

/// A Solana node of one cluster, as a [`base::Cluster`] ranks it.
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

/// The tip is `getSlot` at `confirmed`; the chain is `getGenesisHash`.
impl InfoClient for Info {
    fn tip(&self) -> base::Future<'_, anyhow::Result<u64>> {
        Box::pin(self.call("getSlot", json!([{ "commitment": CONFIRMED }])))
    }

    fn info(&self) -> base::Future<'_, anyhow::Result<NodeInfo>> {
        Box::pin(async move {
            let genesis: String = self.call("getGenesisHash", json!([])).await?;
            let expected = &self.network.genesis_hash;
            ensure!(
                genesis == expected.to_string(),
                "Solana node serves genesis {genesis}, not the configured {expected}"
            );
            Ok(NodeInfo {
                chain_id: genesis,
                tip: self.tip().await?,
            })
        })
    }
}

/// A call's result decoded as `T`, a call's or a batch entry's; an error
/// keeps its [`::rpc::Error`].
pub(crate) fn decode<T, E>(method: &str, reply: Result<RawJson, E>) -> anyhow::Result<T>
where
    T: DeserializeOwned,
    E: Into<::rpc::Error>,
{
    let raw = reply
        .map_err(Into::into)
        .with_context(|| format!("Solana {method}"))?;
    raw.deserialize()
        .with_context(|| format!("Solana {method} result"))
}
