//! The Solana JSON-RPC calls this crate makes, over `::rpc`, answered in
//! `base` types.

use std::{str::FromStr as _, sync::Arc};

use ::rpc::{Answer, Batch, Call, Client as _, RawJson, Request};
use anyhow::{Context as _, anyhow, bail, ensure};
use base::{
    Asset, Block, BlockHash, BlockHeader, BlockParent, Blocks, Decimal, InfoClient, NodeInfo,
    NotServed, Reason, Reward, Tips, TransactionError, TransactionErrorKind, Transactions,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use solana_hash::Hash;
use solana_message::Message;

use crate::{
    Info, Network, Tokens,
    rpc_info::{CONFIRMED, decode},
    spl,
    transaction::Transaction,
};

/// `getBlock` and `getBlockTime` codes meaning no block was produced at the
/// slot.
const SKIPPED: [i64; 2] = [-32_007, -32_009];
/// `getBlock` and `getBlockTime` code: the node cleaned the block up.
const CLEANED_UP: i64 = -32_001;
/// Blocks requested per JSON-RPC batch: a full block is large.
const BATCH: usize = 10;

/// A Solana JSON-RPC client of one cluster: its [`Info`] client and every
/// call this crate makes; cheap to clone, clones share it.
#[derive(Clone)]
pub struct Client(Arc<Info>);

impl Client {
    /// A client of `network` over `transport`; [`InfoClient::info`] checks the node.
    #[must_use]
    pub fn new(transport: Arc<dyn ::rpc::Client>, network: Network) -> Self {
        Self(Arc::new(Info::new(transport, network)))
    }

    /// The network it serves.
    #[must_use]
    pub fn network(&self) -> &Network {
        self.0.network()
    }

    /// The newest confirmed slot as produced, and the newest rooted
    /// (`finalized`) slot as confirmed.
    ///
    /// # Errors
    ///
    /// A node failure.
    pub async fn tips(&self) -> anyhow::Result<Tips> {
        let [produced, rooted] = self
            .batch(vec![
                Call::new("getSlot", json!([{ "commitment": CONFIRMED }])),
                Call::new("getSlot", json!([{ "commitment": "finalized" }])),
            ])
            .await?
            .try_into()
            .map_err(|_| anyhow!("Solana tips batch is not two answers"))?;
        let produced: u64 = decode("getSlot", produced)?;
        let rooted: u64 = decode("getSlot", rooted)?;
        Ok(Tips {
            produced,
            confirmed: rooted.min(produced),
        })
    }

    /// Blocks `start..=end`, linked, skipped slots absent, legs of SOL and of
    /// `tokens` only: one `getBlocks` for the produced slots, then `getBlock`
    /// batches of ten.
    ///
    /// # Errors
    ///
    /// A node failure, a block the node no longer keeps ([`NotServed`]), or
    /// a run that does not link (the chain moved mid-fetch; retry).
    pub async fn blocks(&self, start: u64, end: u64, tokens: &Tokens) -> anyhow::Result<Blocks> {
        let network = self.network();
        let mut run = Blocks::new(network.into());
        if start > end {
            return Ok(run);
        }
        let slots: Vec<u64> = self
            .0
            .call(
                "getBlocks",
                json!([start, end, { "commitment": CONFIRMED }]),
            )
            .await?;
        // A node lists no slot below the first block it keeps: unless `start`
        // is listed, it may be one it no longer serves, not a skipped one.
        if slots.first() != Some(&start) {
            let first: u64 = self.0.call("getFirstAvailableBlock", json!([])).await?;
            if start < first {
                return Err(NotServed { position: start }.into());
            }
        }
        for chunk in slots.chunks(BATCH) {
            let calls = chunk
                .iter()
                .map(|slot| {
                    Call::new(
                        "getBlock",
                        json!([slot, {
                            "commitment": CONFIRMED,
                            "encoding": "json",
                            // Accounts, fee, status and balances: what the
                            // legs are read from; no instructions or logs.
                            "transactionDetails": "accounts",
                            "maxSupportedTransactionVersion": 1,
                            "rewards": true,
                        }]),
                    )
                })
                .collect();
            for (slot, reply) in chunk.iter().copied().zip(self.batch(calls).await?) {
                match &reply {
                    Err(failure) if SKIPPED.contains(&failure.code) => {
                        bail!("Solana slot {slot} was listed and then skipped; retry");
                    }
                    Err(failure) if failure.code == CLEANED_UP => {
                        return Err(NotServed { position: slot }.into());
                    }
                    _ => {}
                }
                let wire: WireBlock = decode("getBlock", reply)?;
                let block = block(slot, wire, network, tokens)
                    .with_context(|| format!("Solana block {slot}"))?;
                if !run.push(block) {
                    bail!("Solana block {slot} does not link; retry");
                }
            }
        }
        Ok(run)
    }

    /// The time of the block at `slot` (`getBlockTime`); `None` for a
    /// skipped slot, or one the node reports no time for.
    ///
    /// # Errors
    ///
    /// A node failure, or a block it no longer keeps ([`NotServed`]).
    pub async fn time(&self, slot: u64) -> anyhow::Result<Option<u64>> {
        let reply = self
            .0
            .request(Request::new("getBlockTime", json!([slot])))
            .await;
        match &reply {
            Err(::rpc::Error::Remote(failure)) if SKIPPED.contains(&failure.code) => Ok(None),
            Err(::rpc::Error::Remote(failure)) if failure.code == CLEANED_UP => {
                Err(NotServed { position: slot }.into())
            }
            _ => decode("getBlockTime", reply),
        }
    }

    /// A recent confirmed blockhash.
    ///
    /// # Errors
    ///
    /// A node failure.
    pub async fn blockhash(&self) -> anyhow::Result<Hash> {
        let latest: Context<Latest> = self
            .0
            .call("getLatestBlockhash", json!([{ "commitment": CONFIRMED }]))
            .await?;
        Hash::from_str(&latest.value.blockhash)
            .ok()
            .context("Solana getLatestBlockhash result")
    }

    /// The fee of `message`, in lamports.
    ///
    /// # Errors
    ///
    /// A node failure, or a node that does not price it (its blockhash is
    /// unknown to the node).
    pub async fn fee(&self, message: &Message) -> anyhow::Result<u64> {
        let fee: Context<Option<u64>> = self
            .0
            .call(
                "getFeeForMessage",
                json!([STANDARD.encode(message.serialize()), { "commitment": CONFIRMED }]),
            )
            .await?;
        fee.value.context("Solana node did not price the message")
    }

    /// The lamports an account of `bytes` must hold to be rent-exempt.
    ///
    /// # Errors
    ///
    /// A node failure.
    pub(crate) async fn rent(&self, bytes: u64) -> anyhow::Result<u64> {
        self.0
            .call("getMinimumBalanceForRentExemption", json!([bytes]))
            .await
    }

    /// The decimals of the SPL Token mint `mint`.
    ///
    /// # Errors
    ///
    /// A node failure, or an account that is not an initialized SPL Token
    /// mint (Token-2022 included).
    pub async fn decimals(&self, mint: &solana_address::Address) -> anyhow::Result<u8> {
        let account = self
            .account(mint)
            .await?
            .with_context(|| format!("Solana mint {mint} does not exist"))?;
        let owner = solana_address::Address::from_str(&account.owner)
            .ok()
            .context("Solana account owner")?;
        let data = STANDARD
            .decode(&account.data.0)
            .context("Solana account data")?;
        spl::decimals(&owner, &data).with_context(|| format!("Solana mint {mint}"))
    }

    /// Submits `bytes` with `sendTransaction` once to every node the
    /// transport reaches, with preflight and no node rebroadcast. A refusal
    /// as "already been processed" is acceptance.
    ///
    /// # Errors
    ///
    /// `Expired` once its blockhash is gone; `Rejected` with its reason for
    /// any other refusal; `Unavailable` when no node answered: the outcome is
    /// unknown, retry with the same bytes.
    pub async fn send(&self, bytes: &[u8]) -> Result<(), TransactionError> {
        let request = Request::once(
            "sendTransaction",
            json!([STANDARD.encode(bytes), {
                "encoding": "base64",
                "skipPreflight": false,
                "preflightCommitment": CONFIRMED,
                "maxRetries": 0,
            }]),
        )
        .with_timeout(::rpc::SEND_TIMEOUT);
        let failure = match self.0.broadcast(request).await {
            Ok(()) => return Ok(()),
            Err(::rpc::Error::Remote(failure)) => failure,
            Err(error) => {
                return Err(TransactionError::unavailable(format!(
                    "Solana sendTransaction: {error}"
                )));
            }
        };
        let message = &failure.message;
        let (kind, reason) = if message.contains("already been processed") {
            return Ok(());
        } else if message.contains("Blockhash not found") {
            (TransactionErrorKind::Expired, Reason::Expired)
        } else if message.contains("insufficient funds") || message.contains("InsufficientFunds") {
            (TransactionErrorKind::Rejected, Reason::InsufficientFunds)
        } else {
            (
                TransactionErrorKind::Rejected,
                Reason::Other(format!("code {}", failure.code)),
            )
        };
        Err(TransactionError::new(
            kind,
            format!(
                "Solana node refused the transaction (code {})",
                failure.code
            ),
        )
        .with_reason(reason))
    }

    /// The finalized account at `address`; `None` when it does not exist.
    async fn account(&self, address: &solana_address::Address) -> anyhow::Result<Option<Account>> {
        let account: Context<Option<Account>> = self
            .0
            .call(
                "getAccountInfo",
                json!([address.to_string(), { "commitment": "finalized", "encoding": "base64" }]),
            )
            .await?;
        Ok(account.value)
    }

    /// Whether account `address` exists (finalized).
    ///
    /// # Errors
    ///
    /// A node failure.
    pub(crate) async fn exists(&self, address: &solana_address::Address) -> anyhow::Result<bool> {
        Ok(self.account(address).await?.is_some())
    }

    /// Repeatable calls as one batch; one answer per call, in call order.
    async fn batch(&self, calls: Vec<Call>) -> anyhow::Result<Vec<Answer>> {
        let count = calls.len();
        let replies = self
            .0
            .batch(Batch::new(calls)?)
            .await
            .context("Solana JSON-RPC batch")?;
        ensure!(
            replies.len() == count,
            "Solana JSON-RPC batch answered {} of {count} calls",
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

/// The node it reaches, as is.
impl ::rpc::Client for Client {
    fn request<'a>(
        &'a self,
        request: Request,
    ) -> ::rpc::BoxFuture<'a, Result<RawJson, ::rpc::Error>> {
        self.0.request(request)
    }

    fn batch<'a>(
        &'a self,
        batch: Batch,
    ) -> ::rpc::BoxFuture<'a, Result<Vec<Answer>, ::rpc::Error>> {
        self.0.batch(batch)
    }

    fn broadcast<'a>(&'a self, request: Request) -> ::rpc::BoxFuture<'a, Result<(), ::rpc::Error>> {
        self.0.broadcast(request)
    }
}

/// Its [`Info`]'s.
impl InfoClient for Client {
    fn tip(&self) -> base::Future<'_, anyhow::Result<u64>> {
        self.0.tip()
    }

    fn info(&self) -> base::Future<'_, anyhow::Result<NodeInfo>> {
        self.0.info()
    }
}

/// `wire`, produced at `slot`, as a base block: position is the slot, height
/// the block height; its rewards (rent, staking, fees) are signed rewards.
fn block(slot: u64, wire: WireBlock, network: &Network, tokens: &Tokens) -> anyhow::Result<Block> {
    let parent = match slot {
        0 => None,
        _ => Some(BlockParent {
            position: wire.parent_slot,
            hash: hash(&wire.previous_blockhash)?,
        }),
    };
    let header = BlockHeader {
        position: slot,
        height: wire.block_height.context("Solana block has no height")?,
        hash: hash(&wire.blockhash)?,
        parent,
        timestamp: wire.block_time.and_then(|time| u64::try_from(time).ok()),
    };
    let transactions = (0_u64..)
        .zip(&wire.transactions)
        .map(|(order, transaction)| {
            Ok(Transaction::from_rpc(transaction, order, network, tokens)?.into())
        })
        .collect::<anyhow::Result<_>>()?;
    let rewards = (0_u64..)
        .zip(wire.rewards.unwrap_or_default())
        .map(|(index, reward)| {
            let lamports = Decimal::from(reward.lamports.unsigned_abs());
            Reward {
                index,
                address: base::Address::new(network.into(), reward.pubkey),
                asset: Asset::native(network.into()),
                amount: if reward.lamports < 0 {
                    Decimal::zero() - lamports
                } else {
                    lamports
                },
            }
        })
        .collect();
    Ok(Block {
        network: network.into(),
        header,
        transactions: Transactions(transactions),
        rewards,
    })
}

/// A Base58 blockhash's bytes.
fn hash(text: &str) -> anyhow::Result<BlockHash> {
    let hash = Hash::from_str(text)
        .ok()
        .context("Solana blockhash is not Base58")?;
    Ok(BlockHash(hash.to_bytes().to_vec()))
}

/// A reply read at a slot: `{ context, value }`; only the value is kept.
#[derive(Deserialize)]
struct Context<T> {
    value: T,
}

/// `getLatestBlockhash`, for its blockhash.
#[derive(Deserialize)]
struct Latest {
    blockhash: String,
}

/// `getAccountInfo` in base64.
#[derive(Deserialize)]
struct Account {
    owner: String,
    data: (String, String),
}

/// `getBlock` in `json` encoding with full transactions; the transactions
/// stay JSON for [`Transaction::from_rpc`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireBlock {
    blockhash: String,
    previous_blockhash: String,
    parent_slot: u64,
    block_height: Option<u64>,
    block_time: Option<i64>,
    transactions: Vec<Value>,
    rewards: Option<Vec<WireReward>>,
}

#[derive(Deserialize)]
struct WireReward {
    pubkey: String,
    lamports: i64,
}
