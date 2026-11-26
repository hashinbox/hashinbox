//! [`Info`]: the most basic Tron HTTP API client, a node's tip and genesis
//! id; the full [`crate::Client`] extends it.

use std::sync::Arc;

use ::rpc::{Answer, Batch, RawJson, Request};
use anyhow::Context as _;
use base::{InfoClient, NodeInfo};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

/// The newest block's header: `getblock` without transactions.
pub(crate) const GET_BLOCK: &str = "getblock";

/// A Tron node, read for its tip and genesis id.
pub struct Info {
    transport: Arc<dyn ::rpc::Client>,
}

impl Info {
    /// One repeatable call of `method`, decoded.
    pub(crate) async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        body: Value,
    ) -> anyhow::Result<T> {
        let reply = self.transport.request(Request::new(method, body)).await;
        decode(method, reply)
    }

    /// The header of block `at` (`None`: the newest) by `method`
    /// (`getblock`, or `solidity_getblock` for the solidified one); `None`
    /// for a block the node does not hold.
    pub(crate) async fn head(&self, method: &str, at: Option<u64>) -> anyhow::Result<Option<Head>> {
        let body = match at {
            Some(position) => json!({ "id_or_num": position.to_string(), "detail": false }),
            None => json!({ "detail": false }),
        };
        let head: Head = self.call(method, body).await?;
        Ok(head.id.is_some().then_some(head))
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

/// The tip is the newest block's number; the chain is the genesis block's
/// id.
impl InfoClient for Info {
    fn new(transport: Arc<dyn ::rpc::Client>) -> Self {
        Self { transport }
    }

    fn tip(&self) -> base::Future<'_, anyhow::Result<u64>> {
        Box::pin(async move {
            let head = self.head(GET_BLOCK, None).await?;
            Ok(head.context("Tron node holds no block")?.number())
        })
    }

    fn info(&self) -> base::Future<'_, anyhow::Result<NodeInfo>> {
        Box::pin(async move {
            let genesis = self.head(GET_BLOCK, Some(0)).await?;
            let genesis = genesis.context("Tron node holds no genesis block")?;
            Ok(NodeInfo {
                chain_id: genesis.id.unwrap_or_default(),
                tip: self.tip().await?,
            })
        })
    }
}

/// A block's id and header, as `getblock` and `getblockbynum` write them;
/// `{}` (no id) for a block the node does not hold.
#[derive(Deserialize)]
pub(crate) struct Head {
    /// 32 bytes, hex: the block number then the header's hash.
    #[serde(rename = "blockID")]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) block_header: Header,
}

#[derive(Default, Deserialize)]
pub(crate) struct Header {
    pub(crate) raw_data: Raw,
}

/// Zero fields are left out, as protobuf JSON does: genesis has no
/// `number`.
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Raw {
    pub(crate) number: u64,
    /// Unix milliseconds.
    pub(crate) timestamp: u64,
    pub(crate) parent_hash: String,
}

impl Head {
    /// Its number.
    pub(crate) const fn number(&self) -> u64 {
        self.block_header.raw_data.number
    }

    /// Its time, in Unix seconds.
    pub(crate) const fn time(&self) -> u64 {
        self.block_header.raw_data.timestamp / 1_000
    }
}

/// A call's result decoded as `T`; an error keeps its [`::rpc::Error`].
pub(crate) fn decode<T: DeserializeOwned>(
    method: &str,
    reply: Result<RawJson, ::rpc::Error>,
) -> anyhow::Result<T> {
    let raw = reply.with_context(|| format!("Tron {method}"))?;
    raw.deserialize()
        .with_context(|| format!("Tron {method} result"))
}
