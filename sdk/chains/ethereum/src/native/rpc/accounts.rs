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

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use futures_executor::block_on as run;
    use json_rpc::{CallResult, RawJson};

    use super::*;
    use crate::native::Network;

    struct Probes {
        replies: Vec<Value>,
        calls: Arc<AtomicUsize>,
    }

    impl json_rpc::Client for Probes {
        fn request<'a>(
            &'a self,
            request: json_rpc::Request,
        ) -> json_rpc::BoxFuture<'a, Result<CallResult, json_rpc::Error>> {
            assert_eq!(
                request.repetition(),
                json_rpc::Repetition::Permitted,
                "token validation retains read transport policy"
            );
            let call = request.into_call();
            let (method, params) = (call.method, call.params);
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            let token = Address::from_bytes([0x0b; 20]).to_string();
            let block = json!({"blockHash": format!("0x{}", "aa".repeat(32)),
                "requireCanonical": true});
            let expected = match index {
                0 => ("eth_chainId", json!([])),
                1 => ("eth_getBlockByNumber", json!(["latest", false])),
                2 => ("eth_getCode", json!([token, block])),
                3 => (
                    "eth_call",
                    json!([{"to": token, "data": "0x313ce567"}, block]),
                ),
                4 => (
                    "eth_call",
                    json!([{"to": token,
                    "data": format!("0x70a08231{}", "00".repeat(32))}, block]),
                ),
                _ => panic!("extra token probe"),
            };
            assert_eq!((method.as_str(), params), expected);
            let reply = &self.replies[index];
            Box::pin(async move { Ok(Ok(RawJson::from_serializable(reply)?)) })
        }

        fn batch<'a>(
            &'a self,
            _batch: json_rpc::Batch,
        ) -> json_rpc::BoxFuture<'a, Result<Vec<CallResult>, json_rpc::Error>> {
            panic!("token probes remain sequential")
        }
    }

    #[test]
    fn token_admission_stops_at_each_failure_without_later_probes() {
        let valid = [
            json!("0x7a69"),
            json!({"hash": format!("0x{}", "aa".repeat(32))}),
            json!("0x6000"),
            json!(format!("0x{:064x}", 6)),
            json!(format!("0x{:064x}", 0)),
        ];
        for (length, replacement, message) in [
            (
                3,
                json!("0x"),
                "Ethereum ERC-20 token address has no deployed code",
            ),
            (
                4,
                json!(format!("0x{:064x}", 18)),
                "Ethereum ERC-20 token decimals 18 do not match configured decimals 6",
            ),
        ] {
            let mut replies = valid[..length].to_vec();
            replies[length - 1] = replacement;
            let calls = Arc::new(AtomicUsize::new(0));
            let client = Client::unverified(
                Probes {
                    replies,
                    calls: calls.clone(),
                },
                Network::new(31_337, "anvil"),
            );
            let error =
                run(client.validate_token(&Address::from_bytes([0x0b; 20]), 6)).unwrap_err();
            assert_eq!(error.message, message);
            assert!(!error.retryable);
            assert_eq!(calls.load(Ordering::SeqCst), length);
        }
    }

    #[test]
    fn zero_token_is_rejected_without_chain_or_code_requests() {
        let calls = Arc::new(AtomicUsize::new(0));
        let client = Client::unverified(
            Probes {
                replies: vec![],
                calls: calls.clone(),
            },
            Network::new(31_337, "anvil"),
        );
        let error = run(client.validate_token(&Address::from_bytes([0; 20]), 6)).unwrap_err();
        assert_eq!(
            error.message,
            "Ethereum ERC-20 token address must not be zero"
        );
        assert!(!error.retryable);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    mod scripted {
        use base::{BlockHash, BlockHeight, BlockPosition};
        use futures_executor::block_on;

        use super::super::*;
        use crate::native::rpc::script::{Reply, Script, Scripted};

        fn abi_word(value: u8) -> Value {
            json!(format!("0x{}{value:02x}", "00".repeat(31)))
        }

        fn block(hash_length: usize) -> BlockRef {
            BlockRef {
                position: BlockPosition(9),
                height: BlockHeight(9),
                hash: BlockHash(vec![0xaa; hash_length]),
                parent: None,
                timestamp: None,
            }
        }

        #[test]
        fn every_view_reaches_native_and_token_reads_as_the_same_block_parameter() {
            let owner = json!(format!("0x{}", "0a".repeat(20)));
            let exact =
                json!({"blockHash": format!("0x{}", "aa".repeat(32)), "requireCanonical": true});
            let script = Script::node(vec![
                Reply::ok("eth_getBalance", json!("0x2a")),
                Reply::ok("eth_getBalance", json!("0x2a")),
                Reply::ok("eth_getBalance", json!("0x2a")),
                Reply::ok("eth_call", json!(format!("0x{}", "00".repeat(31) + "2b"))),
                Reply::ok("eth_call", json!(format!("0x{}", "00".repeat(31) + "2b"))),
                Reply::ok("eth_call", json!(format!("0x{}", "00".repeat(31) + "2b"))),
                Reply::ok("eth_getTransactionCount", json!("0x3"))
                    .expecting(json!([owner, "pending"])),
            ]);
            let rpc = script.client();
            let address = Address::from_bytes([0x0a; 20]);
            let token = AssetKind::Erc20(Address::from_bytes([0x0b; 20]));
            for asset in [AssetKind::Native, token] {
                for view in [View::Latest, View::Pending, View::Block(block(32))] {
                    block_on(rpc.balance(address.clone(), &asset, view)).unwrap();
                }
            }
            assert_eq!(block_on(rpc.nonce(address)).unwrap(), 3);
            let requests = script.requests();
            // A reported balance is read at the head, never at the node's own
            // mempool; the token read follows the native one exactly.
            let parameters = requests[..6]
                .iter()
                .map(|(_, params)| params[1].clone())
                .collect::<Vec<_>>();
            let expected = json!(["latest", "pending", exact, "latest", "pending", exact]);
            assert_eq!(json!(parameters), expected);
            assert_eq!(
                requests[3].1[0]["to"],
                json!(format!("0x{}", "0b".repeat(20)))
            );
            assert_eq!(
                requests[3].1[0]["data"],
                json!(format!("0x70a08231{}{}", "00".repeat(12), "0a".repeat(20)))
            );
        }

        #[test]
        fn balance_rejects_invalid_block_hash_before_token_validation_or_rpc() {
            for (length, asset) in [
                (31, AssetKind::Native),
                (33, AssetKind::Native),
                (31, AssetKind::Erc20(Address::ZERO)),
                (33, AssetKind::Erc20(Address::ZERO)),
            ] {
                let script = Script::node(Vec::new());
                let error = block_on(script.client().balance(
                    Address::from_bytes([0x11; 20]),
                    &asset,
                    View::Block(block(length)),
                ))
                .unwrap_err();
                assert_eq!(
                    error.message,
                    "Ethereum balance block hash must contain exactly 32 bytes"
                );
                assert!(!error.retryable);
                assert!(script.requests().is_empty());
            }
        }

        #[test]
        fn validates_erc20_code_and_typed_probes_at_one_canonical_block() {
            let expected_block =
                json!({"blockHash": format!("0x{}", "aa".repeat(32)), "requireCanonical": true});
            let token = json!(format!("0x{}", "0b".repeat(20)));
            let script = Script::node(vec![
                Reply::ok("eth_chainId", json!("0x7a69")),
                Reply::ok(
                    "eth_getBlockByNumber",
                    json!({"hash": format!("0x{}", "aa".repeat(32))}),
                ),
                Reply::ok("eth_getCode", json!("0x6000")).expecting(json!([token, expected_block])),
                Reply::ok("eth_call", abi_word(6))
                    .expecting(json!([{"to": token, "data": "0x313ce567"}, expected_block])),
                Reply::ok("eth_call", abi_word(0)).expecting(json!([
                    {"to": token, "data": format!("0x70a08231{}", "00".repeat(32))},
                    expected_block
                ])),
            ]);
            block_on(
                script
                    .client()
                    .validate_token(&Address::from_bytes([0x0b; 20]), 6),
            )
            .unwrap();
            script.exhausted();
        }

        #[test]
        fn token_validation_rejects_malformed_balance_and_quantities() {
            let script = Script::node(vec![
                Reply::ok("eth_chainId", json!("0x7a69")),
                Reply::ok(
                    "eth_getBlockByNumber",
                    json!({"hash": format!("0x{}", "aa".repeat(32))}),
                ),
                Reply::ok("eth_getCode", json!("0x6000")),
                Reply::ok("eth_call", abi_word(6)),
                Reply::ok("eth_call", json!("0x01")),
                Reply::ok("eth_getBalance", json!("0x00")),
                Reply::ok("eth_call", json!("0x01")),
            ]);
            let rpc = script.client();
            assert!(
                block_on(rpc.validate_token(&Address::from_bytes([0x33; 20]), 6))
                    .unwrap_err()
                    .message
                    .contains("invalid length")
            );
            assert!(
                block_on(rpc.balance(
                    Address::from_bytes([1; 20]),
                    &AssetKind::Native,
                    View::Latest
                ))
                .unwrap_err()
                .message
                .contains("leading zero")
            );
            assert!(
                block_on(rpc.balance(
                    Address::from_bytes([1; 20]),
                    &AssetKind::Erc20(Address::from_bytes([2; 20])),
                    View::Latest
                ))
                .unwrap_err()
                .message
                .contains("invalid length")
            );
        }
    }
}
