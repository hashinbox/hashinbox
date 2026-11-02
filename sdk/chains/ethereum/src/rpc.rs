//! The Ethereum JSON-RPC calls this crate makes, over `::rpc`, answered in
//! `base` types.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use ::rpc::{Batch, Call, CallError, CallResult, Client as _, Failure, Request};
use alloy_consensus::TxEip1559;
use alloy_primitives::{B256, Bytes, U64, U256, hex, keccak256};
use alloy_rpc_types_eth::Block as RpcBlock;
use anyhow::{Context as _, anyhow, bail, ensure};
use base::{
    Asset, Block, BlockHash, BlockHeader, BlockParent, Blocks, Decimal, Reason, Reward, Tips,
    TransactionError, TransactionErrorKind, Transactions,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{Address, Network, Tokens, erc20, transaction::Transaction};

/// JSON-RPC "method not found": a node without `eth_getBlockReceipts`.
const METHOD_NOT_FOUND: i64 = -32_601;
/// Wei in one Gwei: withdrawal amounts are the only Gwei this crate meets.
const WEI_PER_GWEI: u64 = 1_000_000_000;

/// A block as `eth_getBlockByNumber(n, true)` answers it; alloy owns the
/// header shape, transaction objects stay JSON for [`Transaction::from_rpc`].
type WireBlock = RpcBlock<Value>;

/// An Ethereum JSON-RPC client of one network; cheap to clone, clones share
/// it.
#[derive(Clone)]
pub struct Client(Arc<Inner>);

struct Inner {
    transport: Arc<dyn ::rpc::Client>,
    network: Network,
    /// Set once the node refuses `eth_getBlockReceipts`: receipts then come
    /// one per transaction.
    receipts_by_transaction: AtomicBool,
}

impl Client {
    /// A client of `network` over `transport`; [`Self::verify`] checks the node.
    #[must_use]
    pub fn new(transport: Arc<dyn ::rpc::Client>, network: Network) -> Self {
        Self(Arc::new(Inner {
            transport,
            network,
            receipts_by_transaction: AtomicBool::new(false),
        }))
    }

    /// The network it serves.
    #[must_use]
    pub fn network(&self) -> &Network {
        &self.0.network
    }

    /// Checks that the node serves this network's chain ID.
    ///
    /// # Errors
    ///
    /// For another chain, or a node failure.
    pub async fn verify(&self) -> anyhow::Result<()> {
        let actual: U64 = self.call("eth_chainId", json!([])).await?;
        let expected = self.network().chain_id;
        ensure!(
            actual == U64::from(expected),
            "Ethereum node serves chain ID {actual}, not the configured {expected}"
        );
        Ok(())
    }

    /// The newest block (`eth_blockNumber`) and the newest block consensus
    /// finalized as confirmed.
    ///
    /// # Errors
    ///
    /// A node failure, or a node that serves no finalized block yet
    /// (syncing, or a development chain); retry later.
    pub async fn tips(&self) -> anyhow::Result<Tips> {
        let [produced, finalized] = self
            .batch(vec![
                Call::new("eth_blockNumber", json!([])),
                Call::new("eth_getBlockByNumber", json!(["finalized", false])),
            ])
            .await?
            .try_into()
            .map_err(|_| anyhow!("Ethereum tips batch is not two answers"))?;
        let produced: U64 = decode("eth_blockNumber", produced)?;
        let finalized: Option<Numbered> = decode("eth_getBlockByNumber", finalized)?;
        let finalized = finalized.context("Ethereum node serves no finalized block yet")?;
        let produced = produced.to::<u64>();
        Ok(Tips {
            produced,
            confirmed: finalized.number.to::<u64>().min(produced),
        })
    }

    /// Blocks `start..=end`, linked, legs of ether and of `tokens` only. Two
    /// batches: the blocks, then their receipts.
    ///
    /// # Errors
    ///
    /// A node failure, a block the node does not hold yet, or a run that does
    /// not link (the chain moved mid-fetch; retry).
    pub async fn blocks(&self, start: u64, end: u64, tokens: &Tokens) -> anyhow::Result<Blocks> {
        let network = self.network();
        let mut run = Blocks::new(network.into());
        if start > end {
            return Ok(run);
        }
        let calls = (start..=end)
            .map(|height| {
                Call::new(
                    "eth_getBlockByNumber",
                    json!([format!("{height:#x}"), true]),
                )
            })
            .collect();
        let mut wires = Vec::new();
        for (height, reply) in (start..).zip(self.batch(calls).await?) {
            // `null`: this node does not have it yet; the run ends before it.
            let Some(wire) = decode::<Option<WireBlock>>("eth_getBlockByNumber", reply)? else {
                break;
            };
            ensure!(
                wire.number() == height,
                "Ethereum node returned another block for {height}"
            );
            wires.push(wire);
        }
        let receipts = self.receipts(&wires).await?;
        for (wire, receipts) in wires.into_iter().zip(receipts) {
            // Receipts not served yet: the run ends before this block.
            let Some(receipts) = receipts else {
                break;
            };
            let height = wire.number();
            let block = block(wire, &receipts, network, tokens)
                .with_context(|| format!("Ethereum block {height}"))?;
            if !run.push(block) {
                bail!("Ethereum block {height} does not link; retry");
            }
        }
        Ok(run)
    }

    /// The next nonce of `address`, counting its pending transactions.
    ///
    /// # Errors
    ///
    /// A node failure.
    pub async fn nonce(&self, address: &Address) -> anyhow::Result<u64> {
        let nonce: U64 = self
            .call(
                "eth_getTransactionCount",
                json!([address.to_string(), "pending"]),
            )
            .await?;
        Ok(nonce.to())
    }

    /// The latest block's base fee and the node's suggested priority fee,
    /// per gas.
    ///
    /// # Errors
    ///
    /// A node failure, or a latest block without an EIP-1559 base fee.
    pub async fn fees(&self) -> anyhow::Result<(U256, U256)> {
        let [priority, latest] = self
            .batch(vec![
                Call::new("eth_maxPriorityFeePerGas", json!([])),
                Call::new("eth_getBlockByNumber", json!(["latest", false])),
            ])
            .await?
            .try_into()
            .map_err(|_| anyhow!("Ethereum fee batch is not two answers"))?;
        let priority: U256 = decode("eth_maxPriorityFeePerGas", priority)?;
        let latest: Latest = decode("eth_getBlockByNumber", latest)?;
        let base = latest
            .base_fee_per_gas
            .context("latest Ethereum block has no EIP-1559 base fee")?;
        Ok((base, priority))
    }

    /// `eth_estimateGas` of `request` from `from`.
    ///
    /// # Errors
    ///
    /// `Failed` when the call would revert (`Reason::Reverted`) or the node
    /// refuses it; `Unavailable` for a transient failure.
    pub async fn estimate(
        &self,
        from: &Address,
        request: &TxEip1559,
    ) -> Result<u64, TransactionError> {
        let params = json!([{
            "from": from.to_string(),
            "to": request.to.to().map(hex::encode_prefixed),
            "value": format!("{:#x}", request.value),
            "data": hex::encode_prefixed(&request.input),
        }]);
        let reply = match self
            .0
            .transport
            .request(Request::new("eth_estimateGas", params))
            .await
        {
            Ok(reply) => reply,
            Err(error) if error.is_retryable() => {
                return Err(TransactionError::unavailable(format!(
                    "Ethereum eth_estimateGas: {error}"
                )));
            }
            Err(error) => {
                return Err(TransactionError::failed(format!(
                    "Ethereum eth_estimateGas: {error}"
                )));
            }
        };
        match reply {
            Ok(raw) => Ok(raw
                .deserialize::<U64>()
                .map_err(TransactionError::failed)?
                .to()),
            Err(failure) => {
                let message = failure.message.to_ascii_lowercase();
                let failed = TransactionError::failed(CallError::Remote(failure.clone()));
                if message.contains("execution reverted") {
                    Err(failed.with_reason(Reason::Reverted))
                } else if let Some(reason) = reason(&message) {
                    Err(failed.with_reason(reason))
                } else if failure.is_server_error() {
                    Err(TransactionError::unavailable(CallError::Remote(failure)))
                } else {
                    Err(failed)
                }
            }
        }
    }

    /// What the ERC-20 contract `token` reports as its `decimals()`.
    ///
    /// # Errors
    ///
    /// A node failure, or a contract that does not answer one `uint8`.
    pub async fn decimals(&self, token: &alloy_primitives::Address) -> anyhow::Result<u8> {
        let call = json!({
            "to": hex::encode_prefixed(token),
            "data": hex::encode_prefixed(erc20::decimals()),
        });
        let word: Bytes = self.call("eth_call", json!([call, "latest"])).await?;
        erc20::decode_decimals(&word).with_context(|| format!("ERC-20 {token}"))
    }

    /// Submits `bytes` with `eth_sendRawTransaction` to every node the
    /// transport reaches. A refusal of bytes the node holds ("already known",
    /// or "nonce too low" for this very transaction) is acceptance.
    ///
    /// # Errors
    ///
    /// `Rejected` with its reason for a definite refusal; `Unavailable` when
    /// the outcome is unknown: retry with the same bytes.
    pub async fn send(&self, bytes: &[u8]) -> Result<(), TransactionError> {
        let request = Request::once(
            "eth_sendRawTransaction",
            json!([hex::encode_prefixed(bytes)]),
        );
        let failure = match self.0.transport.broadcast(request).await {
            Ok(()) => return Ok(()),
            Err(::rpc::CallError::Remote(failure)) => failure,
            Err(::rpc::CallError::Transport(error)) => {
                return Err(TransactionError::unavailable(format!(
                    "Ethereum eth_sendRawTransaction: {error}"
                )));
            }
        };
        let message = failure.message.to_ascii_lowercase();
        let reason = reason(&message);
        let held = message.contains("already known") || message.contains("known transaction");
        if (held || reason.is_some())
            && self
                .known(keccak256(bytes))
                .await
                .map_err(TransactionError::unavailable)?
        {
            return Ok(());
        }
        match reason {
            Some(reason) => Err(TransactionError::new(
                TransactionErrorKind::Rejected,
                format!(
                    "Ethereum node refused the transaction with code {}",
                    failure.code
                ),
            )
            .with_reason(reason)),
            None => Err(TransactionError::unavailable(format!(
                "Ethereum submission outcome unknown: code {}",
                failure.code
            ))),
        }
    }

    /// Whether the node holds the transaction `id`, mined or pending.
    async fn known(&self, id: B256) -> anyhow::Result<bool> {
        let found: Option<Value> = self
            .call(
                "eth_getTransactionByHash",
                json!([hex::encode_prefixed(id)]),
            )
            .await?;
        Ok(found.is_some())
    }

    /// Every block's receipts, in one batch; `None` for a block whose
    /// receipts the node does not have yet. A node without
    /// `eth_getBlockReceipts` is asked once per transaction from then on, so
    /// this loops at most twice.
    async fn receipts(&self, wires: &[WireBlock]) -> anyhow::Result<Vec<Option<Vec<Value>>>> {
        loop {
            let by_transaction = self.0.receipts_by_transaction.load(Ordering::Acquire);
            let calls = calls(wires, by_transaction);
            let replies = if calls.is_empty() {
                Vec::new()
            } else {
                self.batch(calls).await?
            };
            if let Some(receipts) = collect(wires, by_transaction, replies)? {
                return Ok(receipts);
            }
            self.0
                .receipts_by_transaction
                .store(true, Ordering::Release);
        }
    }

    /// One repeatable call, decoded.
    async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> anyhow::Result<T> {
        let reply = self
            .0
            .transport
            .request(Request::new(method, params))
            .await
            .with_context(|| format!("Ethereum {method}"))?;
        decode(method, reply)
    }

    /// Repeatable calls as one batch; one answer per call, in call order.
    async fn batch(&self, calls: Vec<Call>) -> anyhow::Result<Vec<CallResult>> {
        let count = calls.len();
        let replies = self
            .0
            .transport
            .batch(Batch::new(calls)?)
            .await
            .context("Ethereum JSON-RPC batch")?;
        ensure!(
            replies.len() == count,
            "Ethereum JSON-RPC batch answered {} of {count} calls",
            replies.len()
        );
        Ok(replies)
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

/// A node's height (`eth_blockNumber`): how a [`::rpc::Cluster`] picks its
/// primary.
pub(crate) fn height(node: &::rpc::Http) -> ::rpc::BoxFuture<'_, Result<u64, CallError>> {
    Box::pin(async move {
        let raw = node
            .request(Request::new("eth_blockNumber", json!([])))
            .await??;
        Ok(raw.deserialize::<U64>()?.to())
    })
}

/// `wire` with its receipts, as a base block; withdrawals are its rewards.
fn block(
    wire: WireBlock,
    receipts: &[Value],
    network: &Network,
    tokens: &Tokens,
) -> anyhow::Result<Block> {
    let listed = listed(&wire);
    ensure!(
        receipts.len() == listed.len(),
        "Ethereum receipt count does not match the transaction count"
    );
    let position = wire.number();
    let header = BlockHeader {
        position,
        height: position,
        hash: BlockHash(wire.header.hash.to_vec()),
        parent: position.checked_sub(1).map(|position| BlockParent {
            position,
            hash: BlockHash(wire.header.parent_hash.to_vec()),
        }),
        timestamp: Some(wire.header.timestamp),
    };
    let transactions = listed
        .iter()
        .zip(receipts)
        .map(|(transaction, receipt)| {
            Ok(Transaction::from_rpc(transaction, receipt, network, tokens)?.into())
        })
        .collect::<anyhow::Result<_>>()?;
    let rewards = wire
        .withdrawals
        .as_deref()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter(|withdrawal| withdrawal.amount != 0)
        .map(|withdrawal| Reward {
            index: withdrawal.index,
            address: Address::new(withdrawal.address, network).into(),
            asset: Asset::native(network.into()),
            amount: Decimal::from(withdrawal.amount) * Decimal::from(WEI_PER_GWEI),
        })
        .collect();
    Ok(Block {
        network: network.into(),
        header,
        transactions: Transactions(transactions),
        rewards,
    })
}

/// The transaction objects of `wire`.
fn listed(wire: &WireBlock) -> &[Value] {
    wire.transactions.as_transactions().unwrap_or_default()
}

/// The receipt calls of every non-empty block, in block order.
fn calls(wires: &[WireBlock], by_transaction: bool) -> Vec<Call> {
    let mut calls = Vec::new();
    for wire in wires.iter().filter(|wire| !listed(wire).is_empty()) {
        let hash = hex::encode_prefixed(wire.header.hash);
        if by_transaction {
            calls.extend(listed(wire).iter().map(|transaction| {
                Call::new("eth_getTransactionReceipt", json!([transaction["hash"]]))
            }));
        } else {
            calls.push(Call::new("eth_getBlockReceipts", json!([hash])));
        }
    }
    calls
}

/// The answers to [`calls`], per block, `None` for a block the node answered
/// `null` for (it does not have it yet); `None` overall when the node does
/// not serve `eth_getBlockReceipts`.
fn collect(
    wires: &[WireBlock],
    by_transaction: bool,
    replies: Vec<CallResult>,
) -> anyhow::Result<Option<Vec<Option<Vec<Value>>>>> {
    let mut replies = replies.into_iter();
    let mut next = || {
        replies
            .next()
            .context("Ethereum batch is missing an answer")
    };
    let mut collected = Vec::with_capacity(wires.len());
    for wire in wires {
        let count = listed(wire).len();
        if count == 0 {
            collected.push(Some(Vec::new()));
            continue;
        }
        let receipts = if by_transaction {
            (0..count)
                .map(|_| decode::<Option<Value>>("eth_getTransactionReceipt", next()?))
                .collect::<anyhow::Result<Option<Vec<_>>>>()?
        } else {
            match next()? {
                Err(Failure {
                    code: METHOD_NOT_FOUND,
                    ..
                }) => return Ok(None),
                reply => decode::<Option<Vec<Value>>>("eth_getBlockReceipts", reply)?,
            }
        };
        collected.push(receipts);
    }
    Ok(Some(collected))
}

/// A call's result decoded as `T`; a node error object is an error that
/// keeps its [`CallError`].
fn decode<T: DeserializeOwned>(method: &str, reply: CallResult) -> anyhow::Result<T> {
    let raw = reply
        .map_err(CallError::Remote)
        .with_context(|| format!("Ethereum {method}"))?;
    raw.deserialize()
        .with_context(|| format!("Ethereum {method} result"))
}

/// Why the node definitively refused a transaction, when its message says.
fn reason(message: &str) -> Option<Reason> {
    if message.contains("nonce too low") || message.contains("nonce has already been used") {
        Some(Reason::NonceUsed)
    } else if message.contains("insufficient funds") {
        Some(Reason::InsufficientFunds)
    } else if message.contains("underpriced") || message.contains("fee too low") {
        Some(Reason::FeeTooLow)
    } else if message.contains("invalid sender") || message.contains("invalid signature") {
        Some(Reason::Invalid)
    } else {
        None
    }
}

/// `eth_getBlockByNumber(tag, false)`, for its number.
#[derive(Deserialize)]
struct Numbered {
    number: U64,
}

/// `eth_getBlockByNumber("latest", false)`, for its base fee.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Latest {
    base_fee_per_gas: Option<U256>,
}
