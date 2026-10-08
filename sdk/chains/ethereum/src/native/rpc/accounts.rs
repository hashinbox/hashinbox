//! Account reads and the block they are answered at: `View` (which chain
//! state a read observes), the ether and ERC-20 balance calls, and the nonce
//! reads the send pipeline needs. `View` is part of the contract, not a
//! transport detail: it decides whether two honest nodes must agree.

use alloy_primitives::hex;
use base::{BlockRef, BoxFuture};
use serde_json::{Value, json};

use super::{
    Client,
    wire::{parse_data, parse_fixed_data},
};
use crate::native::{Address, AssetKind, Error, Wei, erc20};

/// Which chain state one account read observes.
///
/// Ethereum's block parameter is not a detail of the transport: it decides
/// whether two honest nodes must agree on the answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    /// The node's own latest block plus its local mempool. Nothing outside
    /// that node shares it, so it answers "may this spend proceed", counting
    /// the sender's in-flight transactions — never "what does this wallet
    /// hold".
    Pending,
    /// The node's current canonical head: every node that agrees on the chain
    /// answers the same. The head itself is not reported back, so a caller
    /// cannot say which block the amount belongs to.
    Latest,
    /// The newest block consensus finalized. Reverting it would require at
    /// least a third of all staked ether to be slashed, so an amount read here
    /// is settled rather than merely canonical. The block itself is not
    /// reported back either.
    Finalized,
    /// Exactly one canonical block, which the node must refuse once it has
    /// reorganised that block out.
    Block(BlockRef),
}

/// Account reads used by wallet balances and transaction preparation.
pub trait Accounts: Send + Sync {
    /// The balance of `asset` held by `address`, as seen in `at`.
    ///
    /// One [`View`] governs both `eth_getBalance` and the ERC-20 `balanceOf`
    /// `eth_call`, so the native and token balances of one wallet are read
    /// from the same state.
    fn balance<'a>(
        &'a self,
        address: Address,
        asset: &'a AssetKind,
        at: View,
    ) -> BoxFuture<'a, Result<Wei, Error>>;

    /// The next nonce of `address`, counting pending transactions.
    fn nonce<'a>(&'a self, address: Address) -> BoxFuture<'a, Result<u64, Error>>;
}

impl View {
    /// This view as a JSON-RPC block parameter.
    ///
    /// # Errors
    ///
    /// A terminal [`Error`] when [`View::Block`] does not carry a 32-byte hash.
    fn parameter(self) -> Result<Value, Error> {
        match self {
            Self::Pending => Ok(json!("pending")),
            Self::Latest => Ok(json!("latest")),
            Self::Finalized => Ok(json!("finalized")),
            Self::Block(block) if block.hash.0.len() != 32 => Err(Error::rpc(
                "Ethereum balance block hash must contain exactly 32 bytes",
                false,
            )),
            Self::Block(block) => Ok(json!({
                "blockHash": hex::encode_prefixed(&block.hash.0),
                "requireCanonical": true,
            })),
        }
    }
}

impl<C: json_rpc::Client> Accounts for Client<C> {
    fn balance<'a>(
        &'a self,
        address: Address,
        asset: &'a AssetKind,
        at: View,
    ) -> BoxFuture<'a, Result<Wei, Error>> {
        Box::pin(async move {
            let block = at.parameter()?;
            let token = match asset {
                AssetKind::Native => {
                    return self
                        .rpc_wei("eth_getBalance", json!([address.to_string(), block]))
                        .await;
                }
                AssetKind::Erc20(token) => token,
            };
            if token.is_zero() {
                return Err(Error::rpc(
                    "Ethereum ERC-20 token address must not be zero",
                    false,
                ));
            }
            let word = self
                .erc20_word(token, erc20::balance_of(&address), block)
                .await?;
            Wei::from_erc20_balance(&word)
                .map_err(|_| Error::invalid_response("eth_call", "invalid balanceOf result"))
        })
    }

    fn nonce<'a>(&'a self, address: Address) -> BoxFuture<'a, Result<u64, Error>> {
        Box::pin(async move {
            self.rpc_u64(
                "eth_getTransactionCount",
                json!([address.to_string(), "pending"]),
            )
            .await
        })
    }
}

impl<C: json_rpc::Client> Client<C> {
    /// Checks a configured ERC-20 contract at one canonical block: it has code,
    /// its `decimals()` equals `expected_decimals`, and `balanceOf` answers.
    ///
    /// The transport must reach exactly one endpoint: a failover transport
    /// cannot guarantee that every probe is answered by the same node.
    ///
    /// # Errors
    ///
    /// The first failing probe, as a terminal [`Error`] unless the node failed.
    pub async fn validate_token(
        &self,
        token: &Address,
        expected_decimals: u8,
    ) -> Result<(), Error> {
        if token.is_zero() {
            return Err(Error::rpc(
                "Ethereum ERC-20 token address must not be zero",
                false,
            ));
        }
        self.verify_chain_id().await?;
        let block = self.latest_canonical_parameter().await?;
        self.require_token_code(token, &block).await?;
        let decimals = self
            .erc20_word(token, erc20::decimals(), block.clone())
            .await
            .and_then(|word| {
                erc20::decode_decimals(&word).map_err(|_| {
                    Error::invalid_response("eth_call", "invalid ERC-20 decimals result")
                })
            })?;
        if decimals != expected_decimals {
            return Err(Error::rpc(
                format!(
                    "Ethereum ERC-20 token decimals {decimals} do not match configured \
                     decimals {expected_decimals}"
                ),
                false,
            ));
        }
        self.erc20_word(token, erc20::balance_of(&Address::ZERO), block)
            .await
            .and_then(|word| {
                Wei::from_erc20_balance(&word).map_err(|_| {
                    Error::invalid_response("eth_call", "invalid ERC-20 balanceOf result")
                })
            })?;
        Ok(())
    }

    /// `{blockHash, requireCanonical}` naming the current latest block.
    pub(crate) async fn latest_canonical_parameter(&self) -> Result<Value, Error> {
        let block: Value = self
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?
            .deserialize()?;
        let hash = block.get("hash").and_then(Value::as_str).ok_or_else(|| {
            Error::invalid_response("eth_getBlockByNumber", "latest block has no hash")
        })?;
        let hash = parse_fixed_data::<32>(hash)
            .map_err(|message| Error::invalid_response("eth_getBlockByNumber", message))?;
        Ok(json!({
            "blockHash": hex::encode_prefixed(hash),
            "requireCanonical": true,
        }))
    }

    async fn require_token_code(&self, token: &Address, block: &Value) -> Result<(), Error> {
        let code: String = self
            .request("eth_getCode", json!([token.to_string(), block]))
            .await?
            .deserialize()?;
        if parse_data(&code)
            .map_err(|message| Error::invalid_response("eth_getCode", message))?
            .is_empty()
        {
            return Err(Error::rpc(
                "Ethereum ERC-20 token address has no deployed code",
                false,
            ));
        }
        Ok(())
    }

    async fn erc20_word(
        &self,
        token: &Address,
        input: Vec<u8>,
        block: Value,
    ) -> Result<[u8; 32], Error> {
        let value: String = self
            .request(
                "eth_call",
                json!([{
                    "to": token.to_string(),
                    "data": hex::encode_prefixed(&input),
                }, block]),
            )
            .await?
            .deserialize()?;
        parse_fixed_data(&value).map_err(|message| Error::invalid_response("eth_call", message))
    }
}
