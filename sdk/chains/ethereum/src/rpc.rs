//! The Ethereum JSON-RPC calls this crate makes, over `::rpc`, answered in
//! `base` types.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ::rpc::{Answer, Batch, Call, Client as _, ErrorKind, Failure, RawJson, Request};
use alloy_consensus::TxEip1559;
use alloy_primitives::{B256, Bytes, U64, U256, hex, keccak256};
use alloy_rpc_types_eth::Block as RpcBlock;
use anyhow::{Context as _, anyhow, bail, ensure};
use base::{
    Asset, Block, BlockHash, BlockHeader, BlockParent, Blocks, Decimal, NodeInfo, NotServed,
    Reason, Reward, Tips, TransactionError, TransactionErrorKind, Transactions,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    Address, Network, Tokens, erc20,
    transaction::{Log, Transaction},
};

/// JSON-RPC "method not found": a node without `eth_getBlockReceipts`.
const METHOD_NOT_FOUND: i64 = -32_601;
/// EIP-4444 "history expired": the node pruned the block's body.
const HISTORY_EXPIRED: i64 = 4_444;
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

    /// The chain ID the node serves and its newest block.
    ///
    /// # Errors
    ///
    /// A node failure, or a node of another chain.
    pub async fn info(&self) -> anyhow::Result<NodeInfo> {
        let actual: U64 = self.call("eth_chainId", json!([])).await?;
        let expected = self.network().chain_id;
        ensure!(
            actual == U64::from(expected),
            "Ethereum node serves chain ID {actual}, not the configured {expected}"
        );
        let tip: U64 = self.call("eth_blockNumber", json!([])).await?;
        Ok(NodeInfo {
            chain_id: expected.to_string(),
            tip: tip.to(),
        })
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
    /// batches, the blocks then their receipts (still needed for every
    /// transaction's status and fee), and one `eth_getLogs` of the `tokens`'
    /// `Transfer`s over the blocks fetched, for the token legs.
    ///
    /// # Errors
    ///
    /// A node failure, a block the node does not hold yet, a block or
    /// receipts it keeps no history for ([`NotServed`], EIP-4444; `null`
    /// receipts of a finalized block count as pruned), or a run that does
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
            expired(height, &reply)?;
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
        let (logs, ahead) = self.transfers(&wires, tokens).await?;
        // A block the logs' node is not at yet: the run ends before it.
        if let Some(ahead) = ahead {
            wires.retain(|wire| wire.number() < ahead);
        }
        for (wire, receipts) in wires.into_iter().zip(receipts) {
            let height = wire.number();
            // No receipts for a block the node has: not ready yet near the
            // tip (the run ends before it), pruned at or below finalized.
            let Some(receipts) = receipts else {
                if height <= self.tips().await?.confirmed {
                    return Err(NotServed { position: height }.into());
                }
                break;
            };
            let block = block(wire, &receipts, &logs, network, tokens)
                .with_context(|| format!("Ethereum block {height}"))?;
            if !run.push(block) {
                bail!("Ethereum block {height} does not link; retry");
            }
        }
        Ok(run)
    }

    /// The time of block `height`: `eth_getBlockByNumber(n, false)`, no
    /// transactions and no receipts.
    ///
    /// # Errors
    ///
    /// A node failure, a block the node does not hold yet, or a node that
    /// keeps no history for it ([`NotServed`], EIP-4444).
    pub async fn time(&self, height: u64) -> anyhow::Result<u64> {
        let method = "eth_getBlockByNumber";
        let reply = self
            .0
            .transport
            .request(Request::new(method, json!([format!("{height:#x}"), false])))
            .await;
        expired(height, &reply)?;
        let timed: Option<Timed> = decode(method, reply)?;
        let timed =
            timed.with_context(|| format!("Ethereum block {height} is not available yet"))?;
        ensure!(
            timed.number == U64::from(height),
            "Ethereum node returned another block for {height}"
        );
        Ok(timed.timestamp.to())
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

    /// `eth_estimateGas` of `request` from `from`, as if `from` held ample
    /// ether: the gas does not depend on it, so a sender without ether (yet)
    /// is estimated too.
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
        let params = json!([
            {
                "from": from.to_string(),
                "to": request.to.to().map(hex::encode_prefixed),
                "value": format!("{:#x}", request.value),
                "data": hex::encode_prefixed(&request.input),
            },
            "latest",
            { from.to_string(): { "balance": "0xffffffffffffffffffffffff" } },
        ]);
        let reply = self
            .0
            .transport
            .request(Request::new("eth_estimateGas", params))
            .await;
        let failure = match reply {
            Ok(raw) => {
                let gas = raw.deserialize::<U64>().map_err(TransactionError::failed)?;
                return Ok(gas.to());
            }
            Err(error) if error.is_retryable() => {
                return Err(TransactionError::unavailable(format!(
                    "Ethereum eth_estimateGas: {error}"
                )));
            }
            Err(::rpc::Error::Remote(failure)) => failure,
            Err(error) => {
                return Err(TransactionError::failed(format!(
                    "Ethereum eth_estimateGas: {error}"
                )));
            }
        };
        let message = failure.message.to_ascii_lowercase();
        let failed = TransactionError::failed(&failure);
        if message.contains("execution reverted") {
            Err(failed.with_reason(Reason::Reverted))
        } else if let Some(reason) = reason(&message) {
            Err(failed.with_reason(reason))
        } else if failure.is_server_error() {
            Err(TransactionError::unavailable(&failure))
        } else {
            Err(failed)
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
        )
        .with_timeout(::rpc::SEND_TIMEOUT);
        let failure = match self.0.transport.broadcast(request).await {
            Ok(()) => return Ok(()),
            Err(::rpc::Error::Remote(failure)) => failure,
            Err(error) => {
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

    /// The `Transfer` logs of `tokens` in the blocks of `wires`, by
    /// transaction hash; none without tokens. A range the node refuses (an
    /// error object: too wide, too many results; or too large an answer) is
    /// halved until one block, which must answer, unless the node is not at
    /// that block yet: a node behind one URL can trail the one the blocks
    /// came from. Then the logs stop below it, and that block is returned:
    /// the run must end before it.
    ///
    /// # Errors
    ///
    /// A node failure, or a log of another block than the one fetched (the
    /// chain moved mid-fetch; retry).
    async fn transfers(
        &self,
        wires: &[WireBlock],
        tokens: &Tokens,
    ) -> anyhow::Result<(BTreeMap<B256, Vec<Log>>, Option<u64>)> {
        let mut transfers = BTreeMap::<B256, Vec<Log>>::new();
        let (Some(first), Some(last)) = (wires.first(), wires.last()) else {
            return Ok((transfers, None));
        };
        let contracts: Vec<_> = tokens.iter().map(hex::encode_prefixed).collect();
        if contracts.is_empty() {
            return Ok((transfers, None));
        }
        let mut ranges = vec![(first.number(), last.number())];
        while let Some((from, to)) = ranges.pop() {
            let filter = json!([{
                "fromBlock": format!("{from:#x}"),
                "toBlock": format!("{to:#x}"),
                "address": contracts,
                "topics": [hex::encode_prefixed(erc20::TRANSFER)],
            }]);
            let reply = self
                .0
                .transport
                .request(Request::new("eth_getLogs", filter))
                .await;
            let refused = match &reply {
                Ok(_) => false,
                Err(error) if error.is_rate_limited() => false,
                Err(::rpc::Error::Remote(_)) => true,
                Err(error) => error.kind() == Some(ErrorKind::ResponseTooLarge),
            };
            if refused && from < to {
                let middle = from + (to - from) / 2;
                ranges.push((middle + 1, to));
                ranges.push((from, middle));
                continue;
            }
            // Lower halves go first, so every range left is above this one.
            if let Err(::rpc::Error::Remote(failure)) = &reply
                && is_ahead(failure)
            {
                return Ok((transfers, Some(from)));
            }
            expired(from, &reply)?;
            for log in decode::<Vec<Log>>("eth_getLogs", reply)? {
                let height = log.block_number.to::<u64>();
                ensure!(
                    wires
                        .iter()
                        .any(|wire| wire.number() == height && wire.header.hash == log.block_hash),
                    "Ethereum logs of block {height} are of another block; retry"
                );
                transfers.entry(log.transaction_hash).or_default().push(log);
            }
        }
        Ok((transfers, None))
    }

    /// One repeatable call, decoded.
    async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> anyhow::Result<T> {
        let reply = self.0.transport.request(Request::new(method, params)).await;
        decode(method, reply)
    }

    /// Repeatable calls as one batch; one answer per call, in call order,
    /// an error object as the [`::rpc::Error`] a single call returns.
    async fn batch(&self, calls: Vec<Call>) -> anyhow::Result<Vec<Result<RawJson, ::rpc::Error>>> {
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
        let replies = replies.into_iter().map(|reply| reply.map_err(Into::into));
        Ok(replies.collect())
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
        self.0.transport.request(request)
    }

    fn batch<'a>(
        &'a self,
        batch: Batch,
    ) -> ::rpc::BoxFuture<'a, Result<Vec<Answer>, ::rpc::Error>> {
        self.0.transport.batch(batch)
    }

    fn broadcast<'a>(&'a self, request: Request) -> ::rpc::BoxFuture<'a, Result<(), ::rpc::Error>> {
        self.0.transport.broadcast(request)
    }
}

/// The tip is `eth_blockNumber`.
impl base::Tip for Client {
    fn tip(&self) -> base::Future<'_, anyhow::Result<u64>> {
        Box::pin(async move {
            let tip: U64 = self.call("eth_blockNumber", json!([])).await?;
            Ok(tip.to())
        })
    }

    fn info(&self) -> base::Future<'_, anyhow::Result<NodeInfo>> {
        Box::pin(Self::info(self))
    }
}

/// `wire` with its receipts and token logs, as a base block; withdrawals are
/// its rewards.
fn block(
    wire: WireBlock,
    receipts: &[Value],
    logs: &BTreeMap<B256, Vec<Log>>,
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
            Ok(Transaction::from_rpc(transaction, receipt, logs, network, tokens)?.into())
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
    replies: Vec<Result<RawJson, ::rpc::Error>>,
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
        let height = wire.number();
        let receipts = if by_transaction {
            (0..count)
                .map(|_| {
                    let reply = next()?;
                    expired(height, &reply)?;
                    decode::<Option<Value>>("eth_getTransactionReceipt", reply)
                })
                .collect::<anyhow::Result<Option<Vec<_>>>>()?
        } else {
            match next()? {
                Err(::rpc::Error::Remote(Failure {
                    code: METHOD_NOT_FOUND,
                    ..
                })) => return Ok(None),
                reply => {
                    expired(height, &reply)?;
                    decode::<Option<Vec<Value>>>("eth_getBlockReceipts", reply)?
                }
            }
        };
        collected.push(receipts);
    }
    Ok(Some(collected))
}

/// Whether `failure` says the block asked for is past the node's head, as
/// reth and Erigon word it: the node has not reached it yet.
fn is_ahead(failure: &Failure) -> bool {
    failure.message.contains("beyond current head")
}

/// [`NotServed`] when `reply` says the node keeps no history for block
/// `height` (EIP-4444).
fn expired(height: u64, reply: &Result<RawJson, ::rpc::Error>) -> Result<(), NotServed> {
    match reply {
        Err(::rpc::Error::Remote(failure)) if failure.code == HISTORY_EXPIRED => {
            Err(NotServed { position: height })
        }
        _ => Ok(()),
    }
}

/// A call's result decoded as `T`; an error keeps its [`::rpc::Error`].
fn decode<T: DeserializeOwned>(
    method: &str,
    reply: Result<RawJson, ::rpc::Error>,
) -> anyhow::Result<T> {
    let raw = reply.with_context(|| format!("Ethereum {method}"))?;
    raw.deserialize()
        .with_context(|| format!("Ethereum {method} result"))
}

/// Why the node definitively refused a transaction, when its message says.
fn reason(message: &str) -> Option<Reason> {
    if message.contains("nonce too low") || message.contains("nonce has already been used") {
        Some(Reason::NonceUsed)
    } else if message.contains("insufficient funds") {
        Some(Reason::InsufficientFunds)
    } else if message.contains("underpriced")
        || message.contains("fee too low")
        || message.contains("max fee per gas less than block base fee")
    {
        Some(Reason::FeeTooLow)
    } else if message.contains("invalid sender")
        || message.contains("invalid signature")
        || message.contains("intrinsic gas too low")
    {
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

/// `eth_getBlockByNumber(n, false)`, for its time.
#[derive(Deserialize)]
struct Timed {
    number: U64,
    timestamp: U64,
}

/// `eth_getBlockByNumber("latest", false)`, for its base fee.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Latest {
    base_fee_per_gas: Option<U256>,
}

#[cfg(test)]
mod tests {
    use base::Reason;

    use super::reason;

    #[test]
    fn a_refusal_is_read_from_what_geth_and_reth_say() {
        for (message, expected) in [
            ("nonce too low: next nonce 5, tx nonce 4", Reason::NonceUsed),
            (
                "insufficient funds for gas * price + value",
                Reason::InsufficientFunds,
            ),
            ("replacement transaction underpriced", Reason::FeeTooLow),
            (
                "max fee per gas less than block base fee: address 0x1, maxFeePerGas: 1, baseFee: 7",
                Reason::FeeTooLow,
            ),
            (
                "intrinsic gas too low: gas 20000, minimum needed 21000",
                Reason::Invalid,
            ),
            ("invalid sender", Reason::Invalid),
        ] {
            assert_eq!(reason(message), Some(expected), "{message}");
        }
        assert_eq!(reason("something else"), None);
    }
}
