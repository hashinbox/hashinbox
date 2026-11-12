//! [`Info`]: the most basic Bitcoin Core client, a node's tip and chain;
//! the full [`crate::Client`] extends it.

use std::sync::Arc;

use ::rpc::{Answer, Batch, RawJson, Request};
use anyhow::{Context as _, ensure};
use base::NodeInfo;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::Network;

/// A Bitcoin Core node of one network, as a cluster ranks it; cheap to
/// clone.
#[derive(Clone)]
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
    pub const fn network(&self) -> Network {
        self.network
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

/// The tip is `getblockcount`; the chain is Bitcoin Core's name for it.
impl base::InfoClient for Info {
    fn tip(&self) -> base::Future<'_, anyhow::Result<u64>> {
        Box::pin(self.call("getblockcount", json!([])))
    }

    fn info(&self) -> base::Future<'_, anyhow::Result<NodeInfo>> {
        Box::pin(async move {
            let chain: Value = self.call("getblockchaininfo", json!([])).await?;
            let name = chain["chain"].as_str().unwrap_or_default();
            let expected = self.network.native().to_core_arg();
            ensure!(
                Network::from_core_chain(name) == Some(self.network),
                "Bitcoin node serves {name}, not the configured {expected}"
            );
            let tip = chain["blocks"]
                .as_u64()
                .context("Bitcoin getblockchaininfo has no blocks")?;
            Ok(NodeInfo {
                chain_id: name.to_owned(),
                tip,
            })
        })
    }
}

/// A call's result decoded as `T`, a call's or a batch entry's; a node error
/// object is an error naming its code.
pub(crate) fn decode<T, E>(method: &str, reply: Result<RawJson, E>) -> anyhow::Result<T>
where
    T: DeserializeOwned,
    E: Into<::rpc::Error>,
{
    let raw = match reply.map_err(Into::into) {
        Ok(raw) => raw,
        Err(::rpc::Error::Remote(failure)) => {
            anyhow::bail!("Bitcoin {method} failed with code {}", failure.code)
        }
        Err(error) => return Err(anyhow::Error::new(error).context(format!("Bitcoin {method}"))),
    };
    raw.deserialize()
        .with_context(|| format!("Bitcoin {method} result"))
}
