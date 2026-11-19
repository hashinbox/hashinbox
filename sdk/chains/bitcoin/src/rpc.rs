//! The Bitcoin Core calls this crate makes, over `::rpc`, answered in
//! `base` types.

use std::sync::Arc;

use ::rpc::{Answer, Batch, Call, Client as _, Failure, RawJson, Request};
use anyhow::{Context as _, bail, ensure};
use base::{
    Block, BlockHash, BlockHeader, BlockParent, Blocks, InfoClient as _, NotServed, Reason, Tips,
    TransactionError, TransactionErrorKind, Transactions,
};
use bitcoin::{Amount, Denomination, hashes::Hash as _, hex::DisplayHex as _};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{FeeRate, Info, Network, rpc_info::decode, transaction::Transaction};

/// `RPC_INVALID_PARAMETER`: `getblockhash` above the tip.
const OUT_OF_RANGE: i64 = -8;
/// `RPC_IN_WARMUP`: the node is starting.
const WARMING_UP: i64 = -28;
/// `RPC_DESERIALIZATION_ERROR`: the bytes are not a transaction.
const UNDECODABLE: i64 = -22;
/// `RPC_VERIFY_ERROR`: an input is missing or already spent.
const INPUTS_SPENT: i64 = -25;
/// `RPC_VERIFY_REJECTED`: mempool policy refused it.
const REJECTED: i64 = -26;
/// `RPC_VERIFY_ALREADY_IN_CHAIN`: it is already mined.
const ALREADY_IN_CHAIN: i64 = -27;

/// A Bitcoin Core client of one network: its [`Info`] client and every
/// call this crate makes; cheap to clone.
#[derive(Clone)]
pub struct Client {
    node: Info,
    network: Network,
}

impl Client {
    /// A client of `network` over `transport`.
    #[must_use]
    pub fn new(transport: Arc<dyn ::rpc::Client>, network: Network) -> Self {
        Self {
            node: Info::new(transport),
            network,
        }
    }

    /// The network it serves.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// The node's best height, and the height `depth` blocks deep (`depth` 1
    /// is the tip itself) as confirmed.
    ///
    /// # Errors
    ///
    /// A node failure.
    pub async fn tips(&self, depth: u64) -> anyhow::Result<Tips> {
        let produced: u64 = self.node.call("getblockcount", json!([])).await?;
        Ok(Tips {
            produced,
            confirmed: produced.saturating_sub(depth.saturating_sub(1)),
        })
    }

    /// Blocks `start..=end`, linked, in two batches (every hash, then every
    /// block); cut at the node's tip.
    ///
    /// # Errors
    ///
    /// A node failure, a block it pruned ([`NotServed`]), a block that does
    /// not parse or is not the one asked, or a run that does not link (the
    /// chain moved mid-fetch; retry).
    pub async fn blocks(&self, start: u64, end: u64) -> anyhow::Result<Blocks> {
        let hashes = self.hashes(start, end).await?;
        let mut run = Blocks::new(self.network().into());
        if hashes.is_empty() {
            return Ok(run);
        }
        let replies = self
            .batch("getblock", hashes.iter().map(|hash| json!([hash, 3])))
            .await?;
        ensure!(
            replies.len() == hashes.len(),
            "Bitcoin getblock batch answered {} of {} calls",
            replies.len(),
            hashes.len()
        );
        for ((position, hash), reply) in (start..).zip(&hashes).zip(replies) {
            if matches!(&reply, Err(failure) if failure.message.contains("pruned data")) {
                return Err(NotServed { position }.into());
            }
            let wire: WireBlock = decode("getblock", reply)?;
            ensure!(
                wire.hash == *hash && wire.height == position,
                "Bitcoin node returned another block for {position}"
            );
            let block = self
                .block(wire)
                .with_context(|| format!("Bitcoin block {position}"))?;
            if !run.push(block) {
                bail!("Bitcoin block {position} does not link; retry");
            }
        }
        Ok(run)
    }

    /// The time of the block at `height`: `getblockhash`, then
    /// `getblockheader` (no transactions).
    ///
    /// # Errors
    ///
    /// A node failure, or a height above the node's tip.
    pub async fn time(&self, height: u64) -> anyhow::Result<u64> {
        let hash: String = self.node.call("getblockhash", json!([height])).await?;
        let header: WireHeader = self
            .node
            .call("getblockheader", json!([hash, true]))
            .await?;
        ensure!(
            header.height == height,
            "Bitcoin node returned another header for {height}"
        );
        Ok(header.time)
    }

    /// Core's conservative fee rate for confirmation within `target` blocks;
    /// `None` when the node has too little data to estimate.
    ///
    /// # Errors
    ///
    /// A node failure or an unreadable rate.
    pub async fn fee_rate(&self, target: u16) -> anyhow::Result<Option<FeeRate>> {
        let reply: Value = self
            .node
            .call("estimatesmartfee", json!([target, "conservative"]))
            .await?;
        let Some(rate) = reply.get("feerate").and_then(Value::as_number) else {
            return Ok(None);
        };
        let rate = Amount::from_str_in(&rate.to_string(), Denomination::Bitcoin)
            .context("Bitcoin fee rate")?;
        Ok(Some(FeeRate::from_sat_per_kvb(rate.to_sat())))
    }

    /// The lowest fee rate the node's mempool accepts now
    /// (`getmempoolinfo`'s `mempoolminfee`).
    ///
    /// # Errors
    ///
    /// A node failure or an unreadable rate.
    pub async fn mempool_minimum(&self) -> anyhow::Result<FeeRate> {
        let reply: Value = self.node.call("getmempoolinfo", json!([])).await?;
        let rate = reply
            .get("mempoolminfee")
            .and_then(Value::as_number)
            .context("getmempoolinfo without mempoolminfee")?;
        let rate = Amount::from_str_in(&rate.to_string(), Denomination::Bitcoin)
            .context("Bitcoin mempool minimum fee")?;
        Ok(FeeRate::from_sat_per_kvb(rate.to_sat()))
    }

    /// Submits `bytes` with `sendrawtransaction` to every node the transport
    /// reaches. A node that already holds the transaction (in its chain or
    /// mempool) counts as accepting it.
    ///
    /// # Errors
    ///
    /// `Unavailable` when no node answers or one is warming up; `Rejected`
    /// with the node's reason otherwise.
    pub async fn send(&self, bytes: &[u8]) -> Result<(), TransactionError> {
        let request = Request::once("sendrawtransaction", json!([bytes.to_lower_hex_string()]))
            .with_timeout(::rpc::SEND_TIMEOUT);
        match self.node.broadcast(request).await {
            Ok(()) => Ok(()),
            Err(::rpc::Error::Remote(failure)) => refusal(&failure),
            Err(error) => Err(TransactionError::new(
                TransactionErrorKind::Unavailable,
                format!("Bitcoin sendrawtransaction: {error}"),
            )),
        }
    }

    /// The hashes at `start..=end`, in one batch, cut at the node's tip.
    async fn hashes(&self, start: u64, end: u64) -> anyhow::Result<Vec<String>> {
        if start > end {
            return Ok(Vec::new());
        }
        let replies = self
            .batch("getblockhash", (start..=end).map(|height| json!([height])))
            .await?;
        let mut hashes = Vec::with_capacity(replies.len());
        for reply in replies {
            if matches!(&reply, Err(failure) if failure.code == OUT_OF_RANGE) {
                break;
            }
            hashes.push(decode("getblockhash", reply)?);
        }
        Ok(hashes)
    }

    /// `wire` with every leg of every transaction; no rewards (the coinbase
    /// is a transaction).
    fn block(&self, wire: WireBlock) -> anyhow::Result<Block> {
        let hash = |text: &str| -> anyhow::Result<BlockHash> {
            let hash: bitcoin::BlockHash = text.parse().context("Bitcoin block hash")?;
            Ok(BlockHash(hash.to_byte_array().to_vec()))
        };
        let parent = match (wire.height.checked_sub(1), &wire.previousblockhash) {
            (Some(position), Some(parent)) => Some(BlockParent {
                position,
                hash: hash(parent)?,
            }),
            _ => None,
        };
        let header = BlockHeader {
            position: wire.height,
            height: wire.height,
            hash: hash(&wire.hash)?,
            parent,
            timestamp: Some(wire.time),
        };
        let transactions = (0_u64..)
            .zip(&wire.tx)
            .map(|(order, json)| Ok(Transaction::from_core(json, self.network(), order)?.into()))
            .collect::<anyhow::Result<_>>()?;
        Ok(Block {
            network: self.network().into(),
            header,
            transactions: Transactions(transactions),
            rewards: Vec::new(),
        })
    }

    /// One repeatable batch of `method`, one call per parameter list.
    async fn batch(
        &self,
        method: &str,
        params: impl IntoIterator<Item = Value>,
    ) -> anyhow::Result<Vec<Answer>> {
        let calls = params
            .into_iter()
            .map(|params| Call::new(method, params))
            .collect();
        self.node
            .batch(Batch::new(calls)?)
            .await
            .with_context(|| format!("Bitcoin {method} batch"))
    }
}

/// What a node's refusal means: accepted after all, transient, or definite.
fn refusal(failure: &Failure) -> Result<(), TransactionError> {
    let message = failure.message.to_lowercase();
    let reason = match failure.code {
        ALREADY_IN_CHAIN => return Ok(()),
        REJECTED if message.contains("already") => return Ok(()),
        code if code == WARMING_UP || failure.is_server_error() => {
            return Err(TransactionError::new(
                TransactionErrorKind::Unavailable,
                format!("Bitcoin node unavailable with code {code}"),
            ));
        }
        INPUTS_SPENT => Reason::NonceUsed,
        UNDECODABLE => Reason::Invalid,
        REJECTED if message.contains("fee") => Reason::FeeTooLow,
        _ => Reason::Other(failure.message.clone()),
    };
    Err(TransactionError::new(TransactionErrorKind::Rejected, failure.shown()).with_reason(reason))
}

/// `getblock <hash> 3`.
#[derive(Deserialize)]
struct WireBlock {
    hash: String,
    height: u64,
    previousblockhash: Option<String>,
    time: u64,
    tx: Vec<Value>,
}

/// `getblockheader <hash> true`, for its time.
#[derive(Deserialize)]
struct WireHeader {
    height: u64,
    time: u64,
}

/// Submits exact signed bytes to every node it reaches ([`Client::send`]).
impl base::Broadcaster for Client {
    fn broadcast<'a>(
        &'a self,
        transaction: &'a base::Transaction,
    ) -> base::Future<'a, Result<(), TransactionError>> {
        Box::pin(self.send(&transaction.bytes))
    }
}

/// The node it reaches, as is.
impl ::rpc::Client for Client {
    fn request<'a>(
        &'a self,
        request: Request,
    ) -> ::rpc::BoxFuture<'a, Result<RawJson, ::rpc::Error>> {
        self.node.request(request)
    }

    fn batch<'a>(
        &'a self,
        batch: Batch,
    ) -> ::rpc::BoxFuture<'a, Result<Vec<Answer>, ::rpc::Error>> {
        self.node.batch(batch)
    }

    fn broadcast<'a>(&'a self, request: Request) -> ::rpc::BoxFuture<'a, Result<(), ::rpc::Error>> {
        self.node.broadcast(request)
    }
}
