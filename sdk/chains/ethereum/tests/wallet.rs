//! Wallets, building and signing, and broadcast to every node, over
//! scripted nodes.

use std::sync::Arc;

use alloy_consensus::{Transaction as _, TxEnvelope};
use alloy_eips::Decodable2718 as _;
use alloy_primitives::{hex, keccak256};
use alloy_sol_types::{SolCall as _, sol};
use base::{
    Asset, Broadcaster as _, BuilderExt as _, Decimal, Leg, Reason, Signer, TransactionErrorKind,
    TransactionState, Transfer, WalletFactory as _,
};
use chain_ethereum::{Client, Config, Factory, Network, Token};
use crypto::SecretBytes;
use futures_executor::block_on;
use json_rpc::{
    BoxFuture, CallError, Cluster, Policy,
    testing::{Reply, Script},
};
use num_bigint::BigUint;
use serde_json::json;

/// The well-known address of secret key 1.
const KEY_ONE: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";
const TOKEN: &str = "0x00000000000000000000000000000000000000c0";

fn network() -> base::Network {
    base::Network::new(chain_ethereum::CHAIN_NAME, "mainnet")
}

fn atomic(amount: u64) -> Decimal {
    Decimal::from_atomic(BigUint::from(amount), 0)
}

sol! {
    function transfer(address recipient, uint256 amount) external returns (bool);
}

fn connect(transport: Arc<dyn json_rpc::Client>) -> Client {
    Client::new(transport, Network::new(1, "mainnet"))
}

fn factory(script: &Script) -> Factory {
    config().factory(&connect(Arc::new(script.clone())))
}

fn config() -> Config {
    let mut config = Config::new(Network::new(1, "mainnet"), Vec::new());
    config.tokens = vec![Token {
        contract: TOKEN.parse().unwrap(),
        decimals: 6,
    }];
    config
}

fn key_one() -> SecretBytes {
    let mut bytes = [0_u8; 32];
    bytes[31] = 1;
    SecretBytes::new(bytes.to_vec())
}

#[test]
fn a_factory_restores_keys_and_parses_addresses() {
    let script = Script::new([]);
    let factory = factory(&script);
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    assert_eq!(wallet.address().value, KEY_ONE);
    assert_eq!(wallet.private_key().1.as_bytes(), key_one().as_bytes());

    let checksummed = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";
    assert_eq!(factory.parse_address(checksummed).unwrap().value, KEY_ONE);
    assert!(
        factory
            .parse_address("0x7E5F4552091A69125d5DfCb7b8C2659029395BdF")
            .is_err()
    );
    assert!(block_on(factory.from_pk(SecretBytes::new(vec![0; 32]))).is_err());

    let unlisted = Asset {
        network: network(),
        id: "0x00000000000000000000000000000000000000c1".to_owned(),
    };
    let refused = wallet.transaction(&unlisted).err().unwrap();
    assert_eq!(refused.kind, TransactionErrorKind::Unsupported);
}

#[test]
fn a_batch_signs_consecutive_nonces_in_order() {
    let script = Script::new([]);
    let factory = factory(&script);
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    let to = base::Address::new(network(), "0x00000000000000000000000000000000000000b0");
    let mut builder = wallet.transaction(&Asset::native(network())).unwrap();
    builder.send((to.clone(), atomic(1000))).unwrap();
    builder.send((to.clone(), atomic(2000))).unwrap();
    assert!(builder.send((to.clone(), atomic(0))).is_err());
    script.push([
        Reply::ok("eth_getTransactionCount", json!("0x7")).expecting(json!([KEY_ONE, "pending"])),
        Reply::ok("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
        Reply::ok("eth_getBlockByNumber", json!({"baseFeePerGas": "0x2"})),
        Reply::ok("eth_estimateGas", json!("0x5208")),
        Reply::ok("eth_estimateGas", json!("0x5208")),
    ]);
    let signed = block_on(builder.sign()).unwrap();
    script.exhausted();

    assert_eq!(signed.len(), 2);
    for (transaction, (nonce, value)) in signed.iter().zip([(7, 1000_u64), (8, 2000)]) {
        assert_eq!(
            transaction.id.id,
            hex::encode_prefixed(keccak256(&transaction.bytes))
        );
        assert_eq!(transaction.state, TransactionState::Pending);
        let envelope = TxEnvelope::decode_2718_exact(&transaction.bytes).unwrap();
        assert_eq!(envelope.nonce(), nonce);
        assert_eq!(envelope.gas_limit(), 25_200, "21000 plus the 20% margin");
        assert_eq!(envelope.value(), alloy_primitives::U256::from(value));
        assert_eq!(envelope.chain_id(), Some(1));
        let from = base::Address::new(network(), KEY_ONE);
        let worst = (2 * 2 + 1_000_000_000_u64) * 25_200;
        assert_eq!(
            transaction.legs,
            vec![
                Leg::Spent {
                    address: from.clone(),
                    asset: Asset::native(network()),
                    amount: atomic(value),
                    coin: None
                },
                Leg::Received {
                    address: to.clone(),
                    asset: Asset::native(network()),
                    amount: atomic(value),
                    coin: None
                },
                Leg::Fee {
                    address: from,
                    asset: Asset::native(network()),
                    amount: atomic(worst)
                },
            ]
        );
    }
}

#[test]
fn a_token_batch_calls_the_contract_with_consecutive_nonces() {
    let script = Script::new([]);
    let factory = factory(&script);
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    let token = Asset {
        network: network(),
        id: TOKEN.to_owned(),
    };
    let to = base::Address::new(network(), "0x00000000000000000000000000000000000000b0");
    let mut builder = wallet.transaction(&token).unwrap();
    builder.send((to.clone(), atomic(5))).unwrap();
    builder.send((to.clone(), atomic(6))).unwrap();
    script.push([
        Reply::ok("eth_getTransactionCount", json!("0x3")),
        Reply::ok("eth_maxPriorityFeePerGas", json!("0x1")),
        Reply::ok("eth_getBlockByNumber", json!({"baseFeePerGas": "0x1"})),
        Reply::ok("eth_estimateGas", json!("0xc350")).expecting(json!([{
            "from": KEY_ONE,
            "to": TOKEN,
            "value": "0x0",
            "data": hex::encode_prefixed(transferCall {
                recipient: to.value.parse().unwrap(),
                amount: alloy_primitives::U256::from(5),
            }.abi_encode()),
        }])),
        Reply::ok("eth_estimateGas", json!("0xc350")),
    ]);
    let signed = block_on(builder.sign()).unwrap();
    script.exhausted();

    assert_eq!(signed.len(), 2);
    let from = base::Address::new(network(), KEY_ONE);
    for (transaction, (nonce, amount)) in signed.iter().zip([(3, 5_u64), (4, 6)]) {
        let envelope = TxEnvelope::decode_2718_exact(&transaction.bytes).unwrap();
        assert_eq!(envelope.nonce(), nonce);
        assert_eq!(
            envelope.to(),
            Some(TOKEN.parse().unwrap()),
            "the token contract"
        );
        assert_eq!(envelope.value(), alloy_primitives::U256::ZERO);
        let call = transferCall::abi_decode(envelope.input()).unwrap();
        assert_eq!(
            call.recipient,
            to.value.parse::<alloy_primitives::Address>().unwrap()
        );
        assert_eq!(call.amount, alloy_primitives::U256::from(amount));
        assert_eq!(
            transaction.id.id,
            hex::encode_prefixed(keccak256(&transaction.bytes))
        );
        let worst = (2 + 1) * 60_000_u64;
        assert_eq!(
            transaction.legs,
            vec![
                Leg::Spent {
                    address: from.clone(),
                    asset: token.clone(),
                    amount: atomic(amount),
                    coin: None
                },
                Leg::Received {
                    address: to.clone(),
                    asset: token.clone(),
                    amount: atomic(amount),
                    coin: None
                },
                Leg::Fee {
                    address: from.clone(),
                    asset: Asset::native(network()),
                    amount: atomic(worst)
                },
            ]
        );
    }
}

#[test]
fn a_reverting_transfer_fails_before_signing() {
    let script = Script::new([]);
    let factory = factory(&script);
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    let token = Asset {
        network: network(),
        id: TOKEN.to_owned(),
    };
    let mut builder = wallet.transaction(&token).unwrap();
    builder
        .send((base::Address::new(network(), TOKEN), atomic(5)))
        .unwrap();
    script.push([
        Reply::ok("eth_getTransactionCount", json!("0x0")),
        Reply::ok("eth_maxPriorityFeePerGas", json!("0x1")),
        Reply::ok("eth_getBlockByNumber", json!({"baseFeePerGas": "0x1"})),
        Reply::failure("eth_estimateGas", 3, "execution reverted: balance too low"),
    ]);
    let error = block_on(builder.sign()).unwrap_err();
    assert_eq!(error.kind, TransactionErrorKind::Failed);
    assert_eq!(error.reason, Some(Reason::Reverted));
}

/// Every node is equally high.
fn height(_: &Script) -> BoxFuture<'_, Result<u64, CallError>> {
    Box::pin(async { Ok(1) })
}

#[test]
fn a_broadcast_reaches_every_node_once() {
    let nodes =
        [Script::new([]), Script::new([])].map(|node| node.once_only("eth_sendRawTransaction"));
    let cluster = block_on(Cluster::new(nodes.to_vec(), height, Policy::default())).unwrap();
    let broadcaster = connect(Arc::new(cluster));
    let bytes = b"\x02signed".to_vec();
    let id = hex::encode_prefixed(keccak256(&bytes));
    for node in &nodes {
        node.push([Reply::ok("eth_sendRawTransaction", json!(id))
            .expecting(json!([hex::encode_prefixed(&bytes)]))]);
    }
    block_on(broadcaster.broadcast(&bytes)).unwrap();
    for node in &nodes {
        node.exhausted();
    }
}

#[test]
fn a_refusal_of_held_bytes_is_acceptance() {
    let script = Script::new([]);
    let broadcaster = connect(Arc::new(script.clone()));
    let bytes = b"\x02signed".to_vec();
    let id = hex::encode_prefixed(keccak256(&bytes));
    script.push([
        Reply::failure("eth_sendRawTransaction", -32_000, "nonce too low"),
        Reply::ok("eth_getTransactionByHash", json!({"hash": id})).expecting(json!([id])),
    ]);
    block_on(broadcaster.broadcast(&bytes)).unwrap();

    script.push([
        Reply::failure(
            "eth_sendRawTransaction",
            -32_000,
            "insufficient funds for gas * price + value",
        ),
        Reply::ok("eth_getTransactionByHash", json!(null)),
    ]);
    let refused = block_on(broadcaster.broadcast(&bytes)).unwrap_err();
    assert_eq!(refused.kind, TransactionErrorKind::Rejected);
    assert_eq!(refused.reason, Some(Reason::InsufficientFunds));

    script.push([Reply::failure(
        "eth_sendRawTransaction",
        -32_603,
        "internal error",
    )]);
    let unknown = block_on(broadcaster.broadcast(&bytes)).unwrap_err();
    assert_eq!(unknown.kind, TransactionErrorKind::Unavailable);
    script.exhausted();
}

/// The well-known address of secret key 2.
const KEY_TWO: &str = "0x2b5ad5c4795c026514f8317c7a215e218dccd6cf";

fn key_two() -> SecretBytes {
    let mut bytes = [0_u8; 32];
    bytes[31] = 2;
    SecretBytes::new(bytes.to_vec())
}

#[test]
fn two_sources_sign_one_transaction_each_with_their_own_nonces() {
    let script = Script::new([]);
    let factory = factory(&script);
    let one = block_on(factory.from_pk(key_one())).unwrap();
    let two = block_on(factory.from_pk(key_two())).unwrap();
    let to = base::Address::new(network(), "0x00000000000000000000000000000000000000b0");
    let mut builder = factory.transaction(&Asset::native(network())).unwrap();
    builder
        .send(Transfer::new(to.clone(), atomic(1000)).from(&one))
        .unwrap();
    builder
        .send(Transfer::new(to.clone(), atomic(2000)).from(&two))
        .unwrap();
    script.push([
        Reply::ok("eth_getTransactionCount", json!("0x7")).expecting(json!([KEY_ONE, "pending"])),
        Reply::ok("eth_getTransactionCount", json!("0x2")).expecting(json!([KEY_TWO, "pending"])),
        Reply::ok("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
        Reply::ok("eth_getBlockByNumber", json!({"baseFeePerGas": "0x2"})),
        Reply::ok("eth_estimateGas", json!("0x5208")),
        Reply::ok("eth_estimateGas", json!("0x5208")),
    ]);
    let signed = block_on(builder.sign()).unwrap();
    script.exhausted();
    assert_eq!(signed.len(), 2);
    for (transaction, (nonce, payer)) in signed.iter().zip([(7, KEY_ONE), (2, KEY_TWO)]) {
        let envelope = TxEnvelope::decode_2718_exact(&transaction.bytes).unwrap();
        assert_eq!(envelope.nonce(), nonce);
        let fee = transaction.legs.iter().find_map(|leg| match leg {
            Leg::Fee { address, .. } => Some(address.value.as_str()),
            _ => None,
        });
        assert_eq!(fee, Some(payer));
    }
}

#[test]
fn a_factory_builder_needs_a_source_whose_key_signs_for_it() {
    let script = Script::new([]);
    let factory = factory(&script);
    let to = base::Address::new(network(), "0x00000000000000000000000000000000000000b0");
    let mut builder = factory.transaction(&Asset::native(network())).unwrap();
    builder.send((to.clone(), atomic(1000))).unwrap();
    let error = block_on(builder.sign()).unwrap_err();
    assert_eq!(error.kind, TransactionErrorKind::Failed);

    let mut builder = factory.transaction(&Asset::native(network())).unwrap();
    let wrong = Signer::new(
        base::Address::new(network(), KEY_ONE),
        crypto::Curve::Secp256k1,
        key_two(),
    );
    builder
        .send(Transfer::new(to, atomic(1000)).from(wrong))
        .unwrap();
    let error = block_on(builder.sign()).unwrap_err();
    assert_eq!(error.kind, TransactionErrorKind::Failed);
    script.exhausted();
}
