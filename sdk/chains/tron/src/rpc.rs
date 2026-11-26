//! The Tron HTTP API calls this crate makes, over `::rpc` (by method name;
//! [`crate::Rest`] sends each as its REST call), answered in `base` types.

use std::sync::Arc;

use ::rpc::{Answer, Batch, Client as _, RawJson, Request};
use alloy_primitives::hex;
use anyhow::{Context as _, bail, ensure};
use base::{
    Block, BlockHash, BlockHeader, BlockParent, Blocks, InfoClient as _, NotServed, Reason, Tips,
    TransactionError, TransactionErrorKind, Transactions,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Info, Network, Tokens,
    fee::Prices,
    rpc_info::{GET_BLOCK, Head},
    transaction::{Reference, Transaction},
};

/// The solidified block's header: what can no longer be reverted.
const SOLIDIFIED: &str = "solidity_getblock";

/// A Tron HTTP API client of one network: its [`Info`] client and every
/// call this crate makes; cheap to clone, clones share it.
#[derive(Clone)]
pub struct Client(Arc<Inner>);

struct Inner {
    node: Info,
    network: Network,
}

impl Client {
    /// A client of `network` over `transport`.
    #[must_use]
    pub fn new(transport: Arc<dyn ::rpc::Client>, network: Network) -> Self {
        Self(Arc::new(Inner {
            node: Info::new(transport),
            network,
        }))
    }

    /// The network it serves.
    #[must_use]
    pub fn network(&self) -> &Network {
        &self.0.network
    }

    /// The newest block (`getblock`).
    ///
    /// # Errors
    ///
    /// A node failure.
    pub async fn tips(&self) -> anyhow::Result<Tips> {
        Ok(Tips {
            position: self.0.node.tip().await?,
        })
    }

    /// Blocks `start..=end`, linked, legs of TRX and of `tokens` only: per
    /// block `getblockbynum`, then, when it holds transactions,
    /// `gettransactioninfobyblocknum` for each one's fee, outcome and logs.
    ///
    /// # Errors
    ///
    /// A node failure, infos that do not match the block's transactions,
    /// no infos for a block `confirmations` below the tip ([`NotServed`]),
    /// or a run that does not link (the chain moved mid-fetch; retry). A
    /// block the node does not hold yet, or one whose infos it does not
    /// have yet near the tip, ends the run before it.
    pub async fn blocks(
        &self,
        start: u64,
        end: u64,
        tokens: &Tokens,
        confirmations: u64,
    ) -> anyhow::Result<Blocks> {
        let network = self.network();
        let mut run = Blocks::new(network.into());
        for position in start..=end {
            let wire: Wire = self
                .call("getblockbynum", json!({ "num": position }))
                .await?;
            let Some(id) = &wire.head.id else {
                break;
            };
            ensure!(
                wire.head.number() == position,
                "Tron node returned another block for {position}"
            );
            let infos: Vec<Value> = if wire.transactions.is_empty() {
                Vec::new()
            } else {
                self.call("gettransactioninfobyblocknum", json!({ "num": position }))
                    .await?
            };
            if infos.is_empty() && !wire.transactions.is_empty() {
                if position <= self.tips().await?.confirmed(confirmations) {
                    return Err(NotServed { position }.into());
                }
                break;
            }
            ensure!(
                infos.len() == wire.transactions.len(),
                "Tron block {position} has {} infos for {} transactions; retry",
                infos.len(),
                wire.transactions.len()
            );
            let header = BlockHeader {
                position,
                height: position,
                hash: BlockHash(hex::decode(id).context("Tron block id")?),
                parent: position.checked_sub(1).map(|parent| BlockParent {
                    position: parent,
                    hash: BlockHash(
                        hex::decode(&wire.head.block_header.raw_data.parent_hash)
                            .unwrap_or_default(),
                    ),
                }),
                timestamp: Some(wire.head.time()),
            };
            let transactions = (0..)
                .zip(wire.transactions.iter().zip(&infos))
                .map(|(order, (listed, info))| {
                    Transaction::from_block(listed, info, order, network, tokens).map(Into::into)
                })
                .collect::<anyhow::Result<_>>()
                .with_context(|| format!("Tron block {position}"))?;
            let block = Block {
                network: network.into(),
                header,
                transactions: Transactions(transactions),
                rewards: Vec::new(),
            };
            if !run.push(block) {
                bail!("Tron block {position} does not link; retry");
            }
        }
        Ok(run)
    }

    /// The time of block `position`, in Unix seconds: its header only.
    ///
    /// # Errors
    ///
    /// A node failure, or a block the node does not hold yet.
    pub async fn time(&self, position: u64) -> anyhow::Result<u64> {
        let head = self.0.node.head(GET_BLOCK, Some(position)).await?;
        let head = head.with_context(|| format!("Tron block {position} is not available yet"))?;
        ensure!(
            head.number() == position,
            "Tron node returned another block for {position}"
        );
        Ok(head.time())
    }

    /// The solidified block, which a transaction references.
    ///
    /// # Errors
    ///
    /// A node failure, or a header that does not decode.
    pub(crate) async fn reference(&self) -> Result<Reference, TransactionError> {
        let head = self.0.node.head(SOLIDIFIED, None).await;
        let head = head.map_err(TransactionError::unavailable)?;
        let head = head.ok_or_else(|| TransactionError::unavailable("no solidified Tron block"))?;
        let mut id = [0; 32];
        hex::decode_to_slice(head.id.as_deref().unwrap_or_default(), &mut id)
            .map_err(|_| TransactionError::failed("Tron block id is not 32 bytes"))?;
        Ok(Reference {
            number: head.number(),
            id,
            timestamp: head.block_header.raw_data.timestamp,
        })
    }

    /// What resources cost now (`getchainparameters`).
    ///
    /// # Errors
    ///
    /// A node failure, or a price it does not list.
    pub(crate) async fn prices(&self) -> anyhow::Result<Prices> {
        let listed: Parameters = self.call("getchainparameters", json!({})).await?;
        let price = |key: &str| {
            listed
                .parameters
                .iter()
                .find(|parameter| parameter.key == key)
                .and_then(|parameter| u64::try_from(parameter.value).ok())
                .with_context(|| format!("Tron chain parameter {key}"))
        };
        Ok(Prices {
            energy: price("getEnergyFee")?,
            bandwidth: price("getTransactionFee")?,
            account: price("getCreateAccountFee")?
                .saturating_add(price("getCreateNewAccountFeeInSystemContract")?),
        })
    }

    /// Submits `bytes` (a signed `Transaction` protobuf) with
    /// `broadcasthex` to every node the transport reaches. A node
    /// that holds it already (`DUP_TRANSACTION_ERROR`) accepted it.
    ///
    /// # Errors
    ///
    /// `Rejected` with its reason for a definite refusal, `Expired` past its
    /// expiration; `Unavailable` when the outcome is unknown: retry with the
    /// same bytes.
    pub async fn send(&self, bytes: &[u8]) -> Result<(), TransactionError> {
        let request = Request::once("broadcasthex", json!({ "transaction": hex::encode(bytes) }))
            .with_timeout(::rpc::SEND_TIMEOUT);
        let failure = match self.0.node.broadcast(request).await {
            Ok(()) => return Ok(()),
            Err(::rpc::Error::Remote(failure)) => failure,
            Err(error) => {
                return Err(TransactionError::unavailable(format!(
                    "Tron broadcasthex: {error}"
                )));
            }
        };
        let (code, message) = failure
            .message
            .split_once(": ")
            .unwrap_or((failure.message.as_str(), ""));
        let rejected = |reason| {
            Err(
                TransactionError::new(TransactionErrorKind::Rejected, failure.shown())
                    .with_reason(reason),
            )
        };
        let insufficient = message.contains("balance is not sufficient")
            || message.contains("not exist")
            || message.contains("no OwnerAccount");
        match code {
            "DUP_TRANSACTION_ERROR" => Ok(()),
            "TRANSACTION_EXPIRATION_ERROR" => Err(TransactionError::new(
                TransactionErrorKind::Expired,
                failure.shown(),
            )
            .with_reason(Reason::Expired)),
            "BANDWITH_ERROR" => rejected(Reason::InsufficientFunds),
            "CONTRACT_VALIDATE_ERROR" if insufficient => rejected(Reason::InsufficientFunds),
            "SIGERROR" | "TAPOS_ERROR" | "TOO_BIG_TRANSACTION_ERROR" => rejected(Reason::Invalid),
            "CONTRACT_EXE_ERROR" => rejected(Reason::Reverted),
            "CONTRACT_VALIDATE_ERROR" => rejected(Reason::Other(message.to_owned())),
            _ => Err(TransactionError::unavailable(format!(
                "Tron submission outcome unknown: {}",
                failure.shown()
            ))),
        }
    }

    /// One repeatable call of `method`, decoded.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        body: Value,
    ) -> anyhow::Result<T> {
        self.0.node.call(method, body).await
    }
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
        ::rpc::Client::request(&self.0.node, request)
    }

    fn batch<'a>(
        &'a self,
        batch: Batch,
    ) -> ::rpc::BoxFuture<'a, Result<Vec<Answer>, ::rpc::Error>> {
        ::rpc::Client::batch(&self.0.node, batch)
    }

    fn broadcast<'a>(&'a self, request: Request) -> ::rpc::BoxFuture<'a, Result<(), ::rpc::Error>> {
        ::rpc::Client::broadcast(&self.0.node, request)
    }
}

/// A block as `getblockbynum` answers it: `{}` for one not produced yet;
/// transactions stay JSON for [`Transaction::from_block`].
#[derive(Deserialize)]
struct Wire {
    #[serde(flatten)]
    head: Head,
    #[serde(default)]
    transactions: Vec<Value>,
}

/// `getchainparameters`: each parameter's key and value (left out when
/// zero; signed, as some are `-1`).
#[derive(Deserialize)]
struct Parameters {
    #[serde(rename = "chainParameter")]
    parameters: Vec<Parameter>,
}

#[derive(Deserialize)]
struct Parameter {
    key: String,
    #[serde(default)]
    value: i64,
}
