//! Wallets, building and signing, and broadcast to every node, over
//! scripted nodes.

use std::sync::Arc;

use ::rpc::testing::{Reply, Script};
use alloy_consensus::{Transaction as _, TxEnvelope, transaction::SignerRecoverable as _};
use alloy_eips::Decodable2718 as _;
use alloy_primitives::{hex, keccak256};
use alloy_sol_types::{SolCall as _, sol};
use base::{
    Asset, Broadcaster as _, Decimal, Fee, FeeLevel, Leg, Reason, Signer, TransactionErrorKind,
    TransactionState, Transfer, WalletFactory as _,
};
use chain_ethereum::{Client, Config, Factory, Network, Token, WalletFactory};
use crypto::SecretBytes;
use futures_executor::block_on;
use num_bigint::BigUint;
use serde_json::json;

/// `transfer`, signed by `builder`.
async fn pay(
    mut builder: Box<dyn base::Builder>,
    transfer: Transfer,
) -> Result<base::Transaction, base::TransactionError> {
    builder.from_transfer(transfer)?;
    builder.sign().await
}

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

fn connect(transport: Arc<dyn ::rpc::Client>) -> Client {
    Client::new(transport, Network::new(1, "mainnet"))
}

fn factory(script: &Script) -> WalletFactory {
    Factory::from(config()).wallets(
        &connect(Arc::new(script.clone())),
        Arc::new(base::NoHoldings),
        Arc::new(base::NoTransactionIndexes),
    )
}

fn config() -> Config {
    let mut config = Config::new(Network::new(1, "mainnet"));
    config.tokens = vec![Token {
        contract: TOKEN.parse().unwrap(),
        decimals: 6,
        ticker: None,
        display_decimals: None,
    }];
    config
}

/// The node's estimate of an ether transfer to an unused address, and the
/// gas limit it signs with: that plus the default 20% margin.
const ETHER_ESTIMATE: &str = "0x5208";
const ETHER_GAS: u64 = 25_200;

/// The estimate request a send makes: from `from`, as if it held ample
/// ether, of `amount` (an ether transfer of it, or a token `transfer` of
/// it); the recipient it names. The estimator asks for 1.
fn estimated_recipient(
    request: &serde_json::Value,
    from: &str,
    amount: u64,
) -> alloy_primitives::Address {
    assert_eq!(request[1], json!("latest"));
    assert_eq!(
        request[2],
        json!({ from: { "balance": "0xffffffffffffffffffffffff" } })
    );
    let call = &request[0];
    assert_eq!(call["from"], json!(from));
    if call["data"] == json!("0x") {
        assert_eq!(call["value"], json!(format!("{amount:#x}")));
        return call["to"].as_str().unwrap().parse().unwrap();
    }
    let data = hex::decode(call["data"].as_str().unwrap()).unwrap();
    let transfer = transferCall::abi_decode(&data).unwrap();
    assert_eq!(transfer.amount, alloy_primitives::U256::from(amount));
    transfer.recipient
}

fn key_one() -> SecretBytes {
    let mut bytes = [0_u8; 32];
    bytes[31] = 1;
    SecretBytes::new(bytes.to_vec())
}

/// `sources` as spends to new addresses.
fn spends(sources: &[(base::Address, Asset)]) -> base::Spends {
    sources.iter().cloned().collect()
}

/// `sources` as spends to `to`.
fn spends_to(sources: &[(base::Address, Asset)], to: &base::Address) -> base::Spends {
    sources
        .iter()
        .cloned()
        .map(|(from, asset)| (from, asset, to.clone()))
        .collect()
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
fn a_transfer_signs_one_transaction_at_the_next_nonce() {
    let script = Script::new([]);
    let factory = factory(&script);
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    let to = base::Address::new(network(), "0x00000000000000000000000000000000000000b0");
    let native = Asset::native(network());
    // A zero amount is refused before any I/O.
    let zero = block_on(pay(
        wallet.transaction(&native).unwrap(),
        Transfer::new(to.clone(), atomic(0)),
    ));
    assert_eq!(zero.unwrap_err().kind, TransactionErrorKind::Failed);
    script.push([
        Reply::ok("eth_getTransactionCount", json!("0x7")).expecting(json!([KEY_ONE, "pending"])),
        Reply::ok("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
        Reply::ok("eth_getBlockByNumber", json!({"baseFeePerGas": "0x2"})),
        Reply::ok("eth_estimateGas", json!(ETHER_ESTIMATE)),
    ]);
    let builder = wallet.transaction(&native).unwrap();
    let transaction = block_on(pay(builder, Transfer::new(to.clone(), atomic(1000)))).unwrap();
    script.exhausted();
    let (_, request) = script.requests().pop().unwrap();
    // Signing tests the very transfer: to its recipient, of its amount.
    assert_eq!(
        estimated_recipient(&request, KEY_ONE, 1000),
        to.value.parse::<alloy_primitives::Address>().unwrap()
    );

    assert_eq!(
        transaction.id.id,
        hex::encode_prefixed(keccak256(&transaction.bytes))
    );
    assert_eq!(transaction.state, TransactionState::Pending);
    let envelope = TxEnvelope::decode_2718_exact(&transaction.bytes).unwrap();
    assert_eq!(envelope.nonce(), 7);
    assert_eq!(envelope.gas_limit(), ETHER_GAS, "the node's estimate");
    assert_eq!(envelope.value(), alloy_primitives::U256::from(1000));
    assert_eq!(envelope.chain_id(), Some(1));
    let from = base::Address::new(network(), KEY_ONE);
    let worst = (2 * 2 + 1_000_000_000_u64) * ETHER_GAS;
    assert_eq!(
        transaction.legs,
        vec![
            Leg::Spent {
                address: from.clone(),
                asset: Asset::native(network()),
                amount: atomic(1000),
                coin: None
            },
            Leg::Received {
                address: to,
                asset: Asset::native(network()),
                amount: atomic(1000),
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

#[test]
fn a_token_transfer_calls_the_contract() {
    let script = Script::new([]);
    let factory = factory(&script);
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    let token = Asset {
        network: network(),
        id: TOKEN.to_owned(),
    };
    let to = base::Address::new(network(), "0x00000000000000000000000000000000000000b0");
    let builder = wallet.transaction(&token).unwrap();
    script.push([
        Reply::ok("eth_getTransactionCount", json!("0x3")),
        Reply::ok("eth_maxPriorityFeePerGas", json!("0x1")),
        Reply::ok("eth_getBlockByNumber", json!({"baseFeePerGas": "0x1"})),
        Reply::ok("eth_estimateGas", json!("0xc350")),
    ]);
    let transaction = block_on(pay(builder, Transfer::new(to.clone(), atomic(5)))).unwrap();
    script.exhausted();
    let (_, request) = script.requests().pop().unwrap();
    assert_eq!(request[0]["to"], json!(TOKEN));
    assert_eq!(request[0]["value"], json!("0x0"));
    // Signing tests the very transfer: to its recipient, of its amount.
    assert_eq!(
        estimated_recipient(&request, KEY_ONE, 5),
        to.value.parse::<alloy_primitives::Address>().unwrap()
    );

    let from = base::Address::new(network(), KEY_ONE);
    let envelope = TxEnvelope::decode_2718_exact(&transaction.bytes).unwrap();
    assert_eq!(envelope.nonce(), 3);
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
    assert_eq!(call.amount, alloy_primitives::U256::from(5));
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
                amount: atomic(5),
                coin: None
            },
            Leg::Received {
                address: to,
                asset: token,
                amount: atomic(5),
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

#[test]
fn a_reverting_transfer_fails_before_signing() {
    let script = Script::new(fee_replies());
    let factory = factory(&script);
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    let to = base::Address::new(network(), TOKEN);
    // The estimator keeps an answer for this kind of send.
    script.push([Reply::ok("eth_estimateGas", json!("0xc350"))]);
    let kept = [(base::Address::new(network(), KEY_TWO), token())];
    block_on(factory.fees().estimate(&spends_to(&kept, &to))).unwrap();
    let builder = wallet.transaction(&token()).unwrap();
    script.push([
        Reply::ok("eth_getTransactionCount", json!("0x0")),
        Reply::failure("eth_estimateGas", 3, "execution reverted: balance too low"),
    ]);
    let error = block_on(pay(builder, Transfer::new(to, atomic(5)))).unwrap_err();
    script.exhausted();
    // Refused: the kept answer is not reused.
    assert_eq!(error.kind, TransactionErrorKind::Failed);
    assert_eq!(error.reason, Some(Reason::Reverted));
}

#[test]
fn a_broadcast_sends_the_exact_bytes_once() {
    let node = Script::new([]).once_only("eth_sendRawTransaction");
    let broadcaster = connect(Arc::new(node.clone()));
    let bytes = b"\x02signed".to_vec();
    let id = hex::encode_prefixed(keccak256(&bytes));
    node.push([Reply::ok("eth_sendRawTransaction", json!(id))
        .expecting(json!([hex::encode_prefixed(&bytes)]))]);
    block_on(broadcaster.broadcast(&signed(&bytes))).unwrap();
    node.exhausted();
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
    block_on(broadcaster.broadcast(&signed(&bytes))).unwrap();

    script.push([
        Reply::failure(
            "eth_sendRawTransaction",
            -32_000,
            "insufficient funds for gas * price + value",
        ),
        Reply::ok("eth_getTransactionByHash", json!(null)),
    ]);
    let refused = block_on(broadcaster.broadcast(&signed(&bytes))).unwrap_err();
    assert_eq!(refused.kind, TransactionErrorKind::Rejected);
    assert_eq!(refused.reason, Some(Reason::InsufficientFunds));
    assert_eq!(
        refused.message,
        "insufficient funds for gas * price + value"
    );

    script.push([Reply::failure(
        "eth_sendRawTransaction",
        -32_603,
        "internal error",
    )]);
    let unknown = block_on(broadcaster.broadcast(&signed(&bytes))).unwrap_err();
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
fn a_transfer_from_its_own_source_takes_that_sources_nonce() {
    let script = Script::new([]);
    let factory = factory(&script);
    let one = block_on(factory.from_pk(key_one())).unwrap();
    let two = block_on(factory.from_pk(key_two())).unwrap();
    let to = base::Address::new(network(), "0x00000000000000000000000000000000000000b0");
    // One's builder, paid by two.
    let builder = one.transaction(&Asset::native(network())).unwrap();
    script.push([
        Reply::ok("eth_getTransactionCount", json!("0x2")).expecting(json!([KEY_TWO, "pending"])),
        Reply::ok("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
        Reply::ok("eth_getBlockByNumber", json!({"baseFeePerGas": "0x2"})),
        Reply::ok("eth_estimateGas", json!(ETHER_ESTIMATE)),
    ]);
    let transfer = Transfer::new(to, atomic(1000)).from(&two);
    let transaction = block_on(pay(builder, transfer)).unwrap();
    script.exhausted();
    let envelope = TxEnvelope::decode_2718_exact(&transaction.bytes).unwrap();
    assert_eq!(envelope.nonce(), 2);
    let fee = transaction.legs.iter().find_map(|leg| match leg {
        Leg::Fee { address, .. } => Some(address.value.as_str()),
        _ => None,
    });
    assert_eq!(fee, Some(KEY_TWO));
}

#[test]
fn a_factory_builder_needs_a_source_whose_key_signs_for_it() {
    let script = Script::new([]);
    let factory = factory(&script);
    let to = base::Address::new(network(), "0x00000000000000000000000000000000000000b0");
    let builder = factory.transaction(&Asset::native(network())).unwrap();
    let error = block_on(pay(builder, Transfer::new(to.clone(), atomic(1000)))).unwrap_err();
    assert_eq!(error.kind, TransactionErrorKind::Failed);

    let builder = factory.transaction(&Asset::native(network())).unwrap();
    let wrong = Signer::new(
        base::Address::new(network(), KEY_ONE),
        crypto::Curve::Secp256k1,
        key_two(),
    );
    let transfer = Transfer::new(to, atomic(1000)).from(wrong);
    let error = block_on(pay(builder, transfer)).unwrap_err();
    assert_eq!(error.kind, TransactionErrorKind::Failed);
    script.exhausted();
}

fn token() -> Asset {
    Asset {
        network: network(),
        id: TOKEN.to_owned(),
    }
}

/// The fee replies one refresh reads: a 1 gwei tip on a base fee of 2 wei.
fn fee_replies() -> [Reply; 2] {
    [
        Reply::ok("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
        Reply::ok("eth_getBlockByNumber", json!({"baseFeePerGas": "0x2"})),
    ]
}

#[test]
fn an_estimate_sums_one_transaction_per_source() {
    let script = Script::new(fee_replies());
    script.push([
        Reply::ok("eth_estimateGas", json!(ETHER_ESTIMATE)),
        Reply::ok("eth_estimateGas", json!("0xc350")),
    ]);
    let fees = factory(&script).fees();
    let one = base::Address::new(network(), KEY_ONE);
    let two = base::Address::new(network(), KEY_TWO);
    let max_fee = 2 * 2 + 1_000_000_000_u64;

    let single =
        block_on(fees.estimate(&spends(&[(one.clone(), Asset::native(network()))]))).unwrap();
    assert_eq!(single, atomic(max_fee * ETHER_GAS));

    let several = [
        (one.clone(), Asset::native(network())),
        (one, token()),
        (two, token()),
    ];
    let total = block_on(fees.estimate(&spends(&several))).unwrap();
    assert_eq!(total, atomic(max_fee * (ETHER_GAS + 2 * 60_000)));
    script.exhausted();
}

#[test]
fn fees_and_gas_refresh_once_per_time_to_live() {
    let estimate = || Reply::ok("eth_estimateGas", json!(ETHER_ESTIMATE));
    let script = Script::new(fee_replies());
    script.push([estimate()]);
    let source = [(
        base::Address::new(network(), KEY_ONE),
        Asset::native(network()),
    )];
    let fees = factory(&script).fees();
    block_on(fees.estimate(&spends(&source))).unwrap();
    block_on(fees.estimate(&spends(&source))).unwrap();
    script.exhausted();

    let mut config = config();
    config.fee_ttl_ms = 0;
    let script = Script::new(fee_replies());
    script.push([estimate()]);
    script.push(fee_replies());
    script.push([estimate()]);
    let fees = Factory::from(config)
        .wallets(
            &connect(Arc::new(script.clone())),
            Arc::new(base::NoHoldings),
            Arc::new(base::NoTransactionIndexes),
        )
        .fees();
    block_on(fees.estimate(&spends(&source))).unwrap();
    block_on(fees.estimate(&spends(&source))).unwrap();
    script.exhausted();
    // Each refresh asks about another unused address: none can have been funded since.
    let recipients: Vec<_> = script
        .requests()
        .iter()
        .filter(|(method, _)| method == "eth_estimateGas")
        .map(|(_, request)| estimated_recipient(request, KEY_ONE, 1))
        .collect();
    assert_eq!(recipients.len(), 2);
    assert_ne!(recipients[0], recipients[1]);
}

#[test]
fn a_sweep_of_balance_less_estimate_pays_the_estimate() {
    let script = Script::new(fee_replies());
    script.push([Reply::ok("eth_estimateGas", json!(ETHER_ESTIMATE))]);
    let factory = factory(&script);
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    let source = [(wallet.address().clone(), Asset::native(network()))];
    let fee = block_on(factory.fees().estimate(&spends(&source))).unwrap();
    let balance = atomic(10_u64.pow(18));
    let to = base::Address::new(network(), "0x00000000000000000000000000000000000000b0");
    let builder = wallet.transaction(&Asset::native(network())).unwrap();
    script.push([
        Reply::ok("eth_getTransactionCount", json!("0x0")),
        Reply::ok("eth_estimateGas", json!(ETHER_ESTIMATE)),
    ]);
    let signed = block_on(pay(builder, Transfer::new(to, balance - fee.clone()))).unwrap();
    script.exhausted();
    let paid = signed.legs.iter().find_map(|leg| match leg {
        Leg::Fee { amount, .. } => Some(amount.clone()),
        _ => None,
    });
    assert_eq!(paid, Some(fee), "the estimator's cached rate, no refresh");
}

/// Holdings where [`KEY_TWO`] and `0x…b0` hold every asset.
struct Held;

impl base::Holdings for Held {
    fn holds<'a>(
        &'a self,
        spends: &'a base::Spends,
    ) -> base::Future<'a, anyhow::Result<Vec<bool>>> {
        let held = spends
            .iter()
            .map(|spend| {
                spend
                    .to
                    .as_ref()
                    .is_some_and(|to| [KEY_TWO, HOLDER].contains(&to.value.as_str()))
            })
            .collect();
        Box::pin(async move { Ok(held) })
    }
}

/// An address [`Held`] says holds every asset.
const HOLDER: &str = "0x00000000000000000000000000000000000000b0";

#[test]
fn a_send_to_a_holder_is_priced_for_one_and_any_other_for_a_new_account() {
    let script = Script::new(fee_replies());
    let held = || Reply::ok("eth_estimateGas", json!(ETHER_ESTIMATE));
    // What an ether transfer to a new account costs on Sepolia today.
    let new = || Reply::ok("eth_estimateGas", json!("0x32a1f"));
    script.push([held(), new()]);
    let factory = Factory::from(config()).wallets(
        &connect(Arc::new(script.clone())),
        Arc::new(Held),
        Arc::new(base::NoTransactionIndexes),
    );
    let fees = factory.fees();
    let one = base::Address::new(network(), KEY_ONE);
    let holder = base::Address::new(network(), HOLDER);
    let source = [(one, Asset::native(network()))];
    let max_fee = 2 * 2 + 1_000_000_000_u64;

    let to_holder = block_on(fees.estimate(&spends_to(&source, &holder))).unwrap();
    assert_eq!(to_holder, atomic(max_fee * ETHER_GAS));
    let to_anyone = block_on(fees.estimate(&spends(&source))).unwrap();
    assert_eq!(
        to_anyone,
        atomic(max_fee * 248_870),
        "207 391 and the margin"
    );
    script.exhausted();
    // Both estimated from the sender: to the holder, then to an unused address.
    let requests: Vec<_> = script
        .requests()
        .into_iter()
        .filter(|(method, _)| method == "eth_estimateGas")
        .map(|(_, request)| estimated_recipient(&request, KEY_ONE, 1))
        .collect();
    assert_eq!(
        requests[0],
        HOLDER.parse::<alloy_primitives::Address>().unwrap()
    );
    assert_ne!(requests[1], requests[0]);
}

#[test]
fn each_payment_signs_with_the_gas_the_node_estimates_for_it() {
    let script = Script::new(first_send());
    let wallet = block_on(factory(&script).from_pk(key_one())).unwrap();
    let to = base::Address::new(network(), HOLDER);
    // Two payments of one kind in a row, inside the time to live: each its own estimate.
    let limits: Vec<u64> = [None, Some("0x2ee0")]
        .into_iter()
        .map(|estimate| {
            let builder = wallet.transaction(&Asset::native(network())).unwrap();
            if let Some(estimate) = estimate {
                script.push([
                    Reply::ok("eth_getTransactionCount", json!("0x8")),
                    Reply::ok("eth_estimateGas", json!(estimate)),
                ]);
            }
            let signed = block_on(pay(builder, Transfer::new(to.clone(), atomic(1000)))).unwrap();
            TxEnvelope::decode_2718_exact(&signed.bytes)
                .unwrap()
                .gas_limit()
        })
        .collect();
    script.exhausted();
    assert_eq!(limits, [ETHER_GAS, 14_400], "each estimate and the margin");
}

#[test]
fn a_sender_holding_no_token_gets_the_last_answer_of_its_kind() {
    let mut config = config();
    config.fee_ttl_ms = 0;
    let script = Script::new([]);
    let fees = Factory::from(config)
        .wallets(
            &connect(Arc::new(script.clone())),
            Arc::new(base::NoHoldings),
            Arc::new(base::NoTransactionIndexes),
        )
        .fees();
    let from = |key: &str| [(base::Address::new(network(), key), token())];
    let fee = atomic((2 * 2 + 1_000_000_000) * 60_000);
    // KEY_TWO holds the token: the node tests its transfer.
    script.push(fee_replies());
    script.push([Reply::ok("eth_estimateGas", json!("0xc350"))]);
    assert_eq!(
        block_on(fees.estimate(&spends(&from(KEY_TWO)))).unwrap(),
        fee
    );
    // KEY_ONE holds none: its test reverts, and the kind's last answer stands.
    script.push(fee_replies());
    script.push([Reply::failure(
        "eth_estimateGas",
        3,
        "execution reverted: balance too low",
    )]);
    assert_eq!(
        block_on(fees.estimate(&spends(&from(KEY_ONE)))).unwrap(),
        fee
    );
    script.exhausted();
}

/// A signed transaction of exactly `bytes`; broadcasting reads nothing else.
fn signed(bytes: &[u8]) -> base::Transaction {
    let network = base::Network::new(base::Chain::new("test"), "test");
    base::Transaction {
        id: base::TransactionId::new(network, "test"),
        state: base::TransactionState::Pending,
        order: None,
        legs: Vec::new(),
        bytes: bytes.to_vec(),
        expires_height: None,
    }
}

#[test]
fn a_hex_key_imports_with_or_without_its_prefix() {
    let factory = factory(&Script::new([]));
    let text = "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";
    let address = "0x2c7536e3605d9c16a7a3d7b1898e529396a65c23";
    for text in [format!("0x{text}"), text.to_uppercase()] {
        let wallet = block_on(factory.from_pk_text(&text)).unwrap();
        assert_eq!(wallet.address().value, address);
    }
    for bad in ["", "0x01", "zz", &"0".repeat(66)] {
        let error = block_on(factory.from_pk_text(bad)).err().unwrap();
        assert_eq!(
            error.downcast_ref::<base::WalletError>().map(|e| e.kind),
            Some(base::WalletErrorKind::InvalidSecret),
            "{bad:?}"
        );
    }
}

/// [`KEY_ONE`]'s sends in flight, as the application keeps them; nothing
/// else.
struct Pending(Vec<base::Transaction>);

impl base::TransactionIndexStore for Pending {
    fn insert<'a>(
        &'a self,
        index: &'a dyn base::IntoIndex<base::TransactionIndex>,
    ) -> base::Future<'a, anyhow::Result<()>> {
        base::TransactionIndexStore::insert(&base::NoTransactionIndexes, index)
    }

    fn find_all<'a>(
        &'a self,
        network: &'a base::Network,
    ) -> base::Future<'a, anyhow::Result<Vec<base::TransactionIndex>>> {
        base::TransactionIndexStore::find_all(&base::NoTransactionIndexes, network)
    }

    fn move_start<'a>(
        &'a self,
        index: &'a base::TransactionIndex,
        start: u64,
    ) -> base::Future<'a, anyhow::Result<()>> {
        base::TransactionIndexStore::move_start(&base::NoTransactionIndexes, index, start)
    }

    fn set_state<'a>(
        &'a self,
        index: &'a base::TransactionIndex,
        state: base::IndexState,
        reason: Option<&'a str>,
    ) -> base::Future<'a, anyhow::Result<()>> {
        base::TransactionIndexStore::set_state(&base::NoTransactionIndexes, index, state, reason)
    }

    fn settle<'a>(
        &'a self,
        index: &'a base::TransactionIndex,
        confirmed: &'a base::Confirmed,
    ) -> base::Future<'a, anyhow::Result<()>> {
        base::TransactionIndexStore::settle(&base::NoTransactionIndexes, index, confirmed)
    }

    fn give_up<'a>(
        &'a self,
        index: &'a base::TransactionIndex,
    ) -> base::Future<'a, anyhow::Result<()>> {
        base::TransactionIndexStore::give_up(&base::NoTransactionIndexes, index)
    }

    fn delete<'a>(
        &'a self,
        index: &'a base::TransactionIndex,
    ) -> base::Future<'a, anyhow::Result<()>> {
        base::TransactionIndexStore::delete(&base::NoTransactionIndexes, index)
    }

    fn find_sent<'a>(
        &'a self,
        sender: &'a base::Address,
    ) -> base::Future<'a, anyhow::Result<Vec<base::Transaction>>> {
        let sent = if sender.value == KEY_ONE {
            self.0.clone()
        } else {
            Vec::new()
        };
        Box::pin(async move { Ok(sent) })
    }
}

/// 1000 wei from [`KEY_ONE`], signed after `pending`, the node counting
/// `count` pending.
fn sent_after(pending: Vec<base::Transaction>, count: &str) -> base::Transaction {
    let script = Script::new([]);
    let factory = Factory::from(config()).wallets(
        &connect(Arc::new(script.clone())),
        Arc::new(base::NoHoldings),
        Arc::new(Pending(pending)),
    );
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    let builder = wallet.transaction(&Asset::native(network())).unwrap();
    let to = base::Address::new(network(), "0x00000000000000000000000000000000000000b0");
    script.push([
        Reply::ok("eth_getTransactionCount", json!(count)).expecting(json!([KEY_ONE, "pending"])),
        Reply::ok("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
        Reply::ok("eth_getBlockByNumber", json!({"baseFeePerGas": "0x2"})),
        Reply::ok("eth_estimateGas", json!(ETHER_ESTIMATE)),
    ]);
    let signed = block_on(pay(builder, Transfer::new(to, atomic(1000)))).unwrap();
    script.exhausted();
    signed
}

fn nonce(transaction: &base::Transaction) -> u64 {
    TxEnvelope::decode_2718_exact(&transaction.bytes)
        .unwrap()
        .nonce()
}

#[test]
fn a_node_whose_pending_count_misses_our_send_still_gives_the_next_send_the_next_nonce() {
    let first = sent_after(Vec::new(), "0x7");
    assert_eq!(nonce(&first), 7);
    // Another node behind the balancer has not seen it: it counts 7 again.
    let second = sent_after(vec![first.clone()], "0x7");
    assert_eq!(nonce(&second), 8);
    // A node ahead of what is recorded is followed.
    let third = sent_after(vec![first, second], "0xa");
    assert_eq!(nonce(&third), 10);
}

/// The tip and the max fee per gas `transaction` signs.
fn fees_of(transaction: &base::Transaction) -> (u128, u128) {
    let envelope = TxEnvelope::decode_2718_exact(&transaction.bytes).unwrap();
    (
        envelope.max_priority_fee_per_gas().unwrap(),
        envelope.max_fee_per_gas(),
    )
}

/// The replies a first ether send of a factory reads: the nonce (7), the
/// tip and base fee, and the gas.
fn first_send() -> Vec<Reply> {
    let mut replies = vec![Reply::ok("eth_getTransactionCount", json!("0x7"))];
    replies.extend(fee_replies());
    replies.push(Reply::ok("eth_estimateGas", json!(ETHER_ESTIMATE)));
    replies
}

#[test]
fn a_fee_level_sets_the_tip_and_the_max_fee_follows() {
    let script = Script::new(first_send());
    let wallet = block_on(factory(&script).from_pk(key_one())).unwrap();
    let to = base::Address::new(network(), HOLDER);
    let mut fees = Vec::new();
    for level in [FeeLevel::Low, FeeLevel::Normal, FeeLevel::High] {
        if level != FeeLevel::Low {
            script.push([
                Reply::ok("eth_getTransactionCount", json!("0x7")),
                Reply::ok("eth_estimateGas", json!(ETHER_ESTIMATE)),
            ]);
        }
        let mut builder = wallet.transaction(&Asset::native(network())).unwrap();
        builder
            .from_transfer(Transfer::new(to.clone(), atomic(1000)))
            .unwrap();
        builder.with_fee(Fee::Set(level));
        fees.push(fees_of(&block_on(builder.sign()).unwrap()));
    }
    script.exhausted();
    // A 1 gwei tip on a base fee of 2 wei: the max fee is 2 × 2 wei + the tip.
    assert_eq!(
        fees,
        [
            (500_000_000, 500_000_004),
            (1_000_000_000, 1_000_000_004),
            (2_000_000_000, 2_000_000_004)
        ]
    );
}

#[test]
fn a_saved_state_loads_as_it_was_saved() {
    let script = Script::new(first_send());
    script.push([
        Reply::ok("eth_getTransactionCount", json!("0x8")),
        Reply::ok("eth_estimateGas", json!("0xc350")),
    ]);
    let wallet = block_on(factory(&script).from_pk(key_one())).unwrap();
    let to = base::Address::new(network(), HOLDER);
    for asset in [Asset::native(network()), token()] {
        let mut builder = wallet.transaction(&asset).unwrap();
        builder
            .from_transfer(Transfer::new(to.clone(), atomic(1000)))
            .unwrap();
        block_on(builder.sign()).unwrap();
        let state = builder.save().unwrap();
        let mut loaded = wallet.transaction(&asset).unwrap();
        loaded.load(&state).unwrap();
        assert_eq!(loaded.save().unwrap(), state);
    }
    script.exhausted();
}

/// An ether payment of `wallet` to `to`, signed, and its builder's state.
fn signed_and_saved(
    wallet: &Arc<dyn base::Wallet>,
    to: base::Address,
) -> (base::Transaction, serde_json::Value) {
    let mut builder = wallet.transaction(&Asset::native(network())).unwrap();
    builder
        .from_transfer(Transfer::new(to, atomic(1000)))
        .unwrap();
    let signed = block_on(builder.sign()).unwrap();
    (signed, builder.save().unwrap())
}

#[test]
fn a_bump_raises_both_fees_a_tenth_at_least_at_the_same_nonce() {
    let script = Script::new(first_send());
    let wallet = block_on(factory(&script).from_pk(key_one())).unwrap();
    let native = Asset::native(network());
    let to = base::Address::new(network(), HOLDER);
    let (first, state) = signed_and_saved(&wallet, to);
    let mut builder = wallet.transaction(&native).unwrap();
    builder.load(&state).unwrap();
    builder.with_fee(Fee::Times(1.1));
    let bumped = block_on(builder.sign()).unwrap();
    // Nothing asked again: the saved nonce and gas limit.
    script.exhausted();
    assert_eq!(nonce(&bumped), 7);
    assert_eq!(
        TxEnvelope::decode_2718_exact(&bumped.bytes)
            .unwrap()
            .gas_limit(),
        ETHER_GAS
    );
    assert_ne!(bumped.id, first.id);
    // A 1 gwei tip and a 2 wei base, each a tenth up, rounded up: 3 wei.
    assert_eq!(fees_of(&bumped), (1_100_000_000, 1_100_000_006));
    // A level above that is the level's tip, its base a tenth up again.
    builder.with_fee(Fee::Set(FeeLevel::High));
    let high = block_on(builder.sign()).unwrap();
    assert_eq!(fees_of(&high), (2_000_000_000, 2_000_000_008));
    assert_eq!(nonce(&high), 7);
}

#[test]
fn a_cancel_pays_zero_to_its_source_at_the_same_nonce_a_tenth_up() {
    let script = Script::new(first_send());
    let wallet = block_on(factory(&script).from_pk(key_one())).unwrap();
    let native = Asset::native(network());
    let to = base::Address::new(network(), HOLDER);
    let (_, state) = signed_and_saved(&wallet, to);
    let mut builder = wallet.transaction(&native).unwrap();
    builder.load(&state).unwrap();
    let source = wallet.address().clone();
    let replace = builder.replaceable().unwrap();
    replace
        .replace(Transfer::new(source, Decimal::zero()))
        .unwrap();
    let cancel = block_on(builder.sign()).unwrap();
    // Its state loads as a cancel.
    let mut again = wallet.transaction(&native).unwrap();
    again.load(&builder.save().unwrap()).unwrap();
    again.with_fee(Fee::Times(1.1));
    let next = block_on(again.sign()).unwrap();
    script.exhausted();
    for signed in [&cancel, &next] {
        let envelope = TxEnvelope::decode_2718_exact(&signed.bytes).unwrap();
        assert_eq!(nonce(signed), 7);
        assert!(envelope.value().is_zero(), "zero ether");
        assert!(envelope.input().is_empty(), "no call");
        let from = envelope.recover_signer().unwrap();
        assert_eq!(envelope.to(), Some(from), "to its source");
    }
    assert_eq!(fees_of(&cancel), (1_100_000_000, 1_100_000_006));
    assert!(fees_of(&next).0 > fees_of(&cancel).0, "a tenth up again");
}

#[test]
fn a_fee_above_the_limits_fails_as_the_fee_cap() {
    let mut config = config();
    config.limits.max_fee_per_gas = 1_100_000_004;
    config.limits.max_priority_fee_per_gas = 1_100_000_000;
    let script = Script::new(first_send());
    let factory = Factory::from(config).wallets(
        &connect(Arc::new(script.clone())),
        Arc::new(base::NoHoldings),
        Arc::new(base::NoTransactionIndexes),
    );
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    let native = Asset::native(network());
    let to = base::Address::new(network(), HOLDER);
    let (_, state) = signed_and_saved(&wallet, to);
    let mut builder = wallet.transaction(&native).unwrap();
    builder.load(&state).unwrap();
    builder.with_fee(Fee::Times(1.1));
    let error = block_on(builder.sign()).unwrap_err();
    assert_eq!(error.kind, TransactionErrorKind::FeeCap, "{error}");
    script.exhausted();
}
