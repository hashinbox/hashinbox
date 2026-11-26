//! Addresses, wallets, building and signing TRX and USDT sends, fees, and
//! broadcast, over scripted nodes.

use std::sync::Arc;

use ::rpc::testing::{Reply, Script};
use alloy_primitives::{hex, keccak256};
use base::{
    Asset, Broadcaster as _, Decimal, Leg, Reason, TransactionErrorKind, Transfer,
    WalletFactory as _,
};
use chain_tron::{
    Account, Client, Factory, WalletFactory,
    testing::fixtures::{USDT, config, network},
};
use crypto::SecretBytes;
use futures_executor::block_on;
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use serde_json::json;

/// The address of secret key 1 (Ethereum's `0x7e5f…5bdf`, `41`-prefixed).
const KEY_ONE: &str = "TMVQGm1qAQYVdetCeGRRkTWYYrLXuHK2HC";

/// A recipient: Nile's `TWLmw5…` (`41df76bd…`).
const TO: &str = "TWLmw5LzBxJ1M2zirtdgdhtajFVHgZz321";

/// The solidified block every send references.
const REFERENCE: u64 = 73_043_017;
const REFERENCE_ID: &str = "00000000045a8c49b8ac7ae6c18f565a3b51db43768f55d5bf01359ca34b898e";

fn key_one() -> SecretBytes {
    let mut bytes = [0_u8; 32];
    bytes[31] = 1;
    SecretBytes::new(bytes.to_vec())
}

fn factory(script: &Script) -> WalletFactory {
    let config = config();
    let rpc = Client::new(Arc::new(script.clone()), config.native());
    Factory::from(config).wallets(&rpc, Arc::new(base::NoHoldings))
}

fn usdt() -> Asset {
    Asset {
        network: network(),
        id: USDT.to_owned(),
    }
}

/// Mainnet's prices: 100 sun per energy, 1,000 per byte, 1.1 TRX per new
/// account.
fn prices() -> Reply {
    Reply::ok(
        "getchainparameters",
        json!({"chainParameter": [
            {"key": "getMaintenanceTimeInterval", "value": 21_600_000},
            {"key": "getCreateAccountFee", "value": 100_000},
            {"key": "getTransactionFee", "value": 1_000},
            {"key": "getCreateNewAccountFeeInSystemContract", "value": 1_000_000},
            {"key": "getEnergyFee", "value": 100},
            {"key": "getAllowTvmTransferTrc10"},
            // Real nodes list some parameters below zero.
            {"key": "getRemoveThePowerOfTheGr", "value": -1},
        ]}),
    )
}

fn solidified() -> Reply {
    Reply::ok(
        "solidity_getblock",
        json!({
            "blockID": REFERENCE_ID,
            "block_header": {"raw_data": {
                "number": REFERENCE,
                "timestamp": 1_795_672_845_000_u64,
                "parentHash": "00000000045a8c4817fd01e2f63b2cf65b1bd7294256cfbb102d74d413eadb39",
            }},
        }),
    )
    .expecting(json!({"detail": false}))
}

/// `bytes` read as one length-delimited protobuf field `field`, and the
/// rest.
fn field(bytes: &[u8], field: u8) -> (&[u8], &[u8]) {
    assert_eq!(bytes[0], (field << 3) | 2);
    let (mut length, mut at) = (0_usize, 1);
    loop {
        length |= usize::from(bytes[at] & 0x7f) << (7 * (at - 1));
        at += 1;
        if bytes[at - 1] < 0x80 {
            break;
        }
    }
    bytes[at..].split_at(length)
}

/// The signed `Transaction`'s `raw_data` and signature, checked: the id is
/// the SHA-256 of `raw_data`, the signature `r ‖ s ‖ 27 + recovery id` by
/// key 1 over it.
fn signed(transaction: &base::Transaction) -> Vec<u8> {
    let (raw, rest) = field(&transaction.bytes, 1);
    let (signature, rest) = field(rest, 2);
    assert!(rest.is_empty());
    let id = crypto::sha256(raw);
    assert_eq!(transaction.id.id, hex::encode(id));
    assert_eq!(signature.len(), 65);
    let recovery = RecoveryId::from_byte(signature[64] - 27).unwrap();
    let signature = Signature::from_slice(&signature[..64]).unwrap();
    let key = VerifyingKey::recover_from_prehash(&id, &signature, recovery).unwrap();
    let point = key.to_encoded_point(false);
    let signer = Account(keccak256(&point.as_bytes()[1..])[12..].try_into().unwrap());
    assert_eq!(signer.to_string(), KEY_ONE);
    raw.to_vec()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn hex_account(text: &str) -> Vec<u8> {
    text.parse::<Account>().unwrap().prefixed().to_vec()
}

#[test]
fn an_address_is_base58check_of_41_and_the_keys_last_20_keccak_bytes() {
    let script = Script::new([]);
    let factory = factory(&script);
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    assert_eq!(wallet.address().value, KEY_ONE);
    assert_eq!(wallet.private_key().1.as_bytes(), key_one().as_bytes());
    let text = block_on(
        factory.from_pk_text("0x0000000000000000000000000000000000000000000000000000000000000001"),
    )
    .unwrap();
    assert_eq!(text.address().value, KEY_ONE);

    // The node's two forms of Nile's USDT, and of key 1.
    let parse = |text| factory.parse_address(text).map(|address| address.value);
    assert_eq!(parse(USDT).unwrap(), USDT);
    assert_eq!(
        parse("41eca9bc828a3005b9a3b909f2cc5c2a54794de05f").unwrap(),
        USDT
    );
    assert_eq!(
        parse("417E5F4552091A69125D5DFCB7B8C2659029395BDF").unwrap(),
        KEY_ONE
    );
    for bad in [
        "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBg",
        "42eca9bc828a3005b9a3b909f2cc5c2a54794de05f",
        "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf",
        "",
    ] {
        assert!(parse(bad).is_err(), "{bad}");
    }
    assert!(block_on(factory.from_pk(SecretBytes::new(vec![0; 32]))).is_err());

    let unlisted = Asset {
        network: network(),
        id: KEY_ONE.to_owned(),
    };
    let refused = wallet.transaction(&unlisted).err().unwrap();
    assert_eq!(refused.kind, TransactionErrorKind::Unsupported);
}

#[test]
fn one_payment_carries_up_to_an_int64_of_sun_or_a_uint256_of_a_token() {
    let factory = factory(&Script::new([]));
    let trx = Asset::native(network());
    let units = |text: &str| text.parse::<Decimal>().unwrap();
    assert!(factory.carries(&trx, &units("9223372036854775807")));
    assert!(!factory.carries(&trx, &units("9223372036854775808")));
    let max = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
    let over = "115792089237316195423570985008687907853269984665640564039457584007913129639936";
    assert!(factory.carries(&usdt(), &units(max)));
    assert!(!factory.carries(&usdt(), &units(over)));
    let unlisted = Asset {
        network: network(),
        id: KEY_ONE.to_owned(),
    };
    assert!(!factory.carries(&unlisted, &units("1")));
}

/// A TRX send: a `TransferContract` on the solidified block, its fee leg
/// the worst case (300 bytes and a new account: 1.4 TRX), expiring 60
/// blocks above its reference.
#[test]
fn a_trx_send_signs_a_transfer_contract_on_the_solidified_block() {
    let script = Script::new([prices(), solidified()]);
    let factory = factory(&script);
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    let to = base::Address::new(network(), TO);
    let mut builder = wallet.transaction(&Asset::native(network())).unwrap();
    let zero = builder.from_transfer(Transfer::new(to.clone(), Decimal::from(0)));
    assert_eq!(zero.unwrap_err().kind, TransactionErrorKind::Failed);
    let to_self = base::Address::new(network(), KEY_ONE);
    builder
        .from_transfer(Transfer::new(to_self, Decimal::from(1)))
        .unwrap();
    assert!(block_on(builder.sign()).is_err(), "TRX to itself");

    builder
        .from_transfer(Transfer::new(to.clone(), Decimal::from(986_000)))
        .unwrap();
    let transaction = block_on(builder.sign()).unwrap();
    script.exhausted();
    let raw = signed(&transaction);
    // ref_block_bytes, ref_block_hash: the reference's number and id.
    assert!(raw.starts_with(&hex::decode("0a028c492208b8ac7ae6c18f565a").unwrap()));
    let mut contract = vec![0x0a, 0x15];
    contract.extend(hex_account(KEY_ONE));
    contract.extend([0x12, 0x15]);
    contract.extend(hex_account(TO));
    contract.extend(hex::decode("1890973c").unwrap());
    assert!(contains(&raw, &contract));
    assert!(contains(&raw, b"protocol.TransferContract"));
    assert!(!contains(&raw, &[0x90, 0x01]), "no fee_limit");
    assert_eq!(transaction.expires_height, Some(REFERENCE + 60));
    let from = wallet.address().clone();
    let trx = Asset::native(network());
    assert_eq!(
        transaction.legs,
        vec![
            Leg::Spent {
                address: from.clone(),
                asset: trx.clone(),
                amount: Decimal::from(986_000),
                coin: None,
            },
            Leg::Received {
                address: to,
                asset: trx.clone(),
                amount: Decimal::from(986_000),
                coin: None,
            },
            Leg::Fee {
                address: from,
                asset: trx,
                amount: Decimal::from(1_400_000),
            },
        ]
    );

    // Its next version: the same payment on a new reference, the rate a
    // tenth up.
    let state = builder.save().unwrap();
    let mut next = wallet.transaction(&Asset::native(network())).unwrap();
    next.load(&state).unwrap();
    script.push([solidified()]);
    let again = block_on(next.sign()).unwrap();
    assert_ne!(again.id, transaction.id);
    assert!(next.save().unwrap()["rate"]["base"].as_u64() > Some(100));
}

/// A USDT send: `transfer(to, amount)` on the contract, its `fee_limit`
/// 200,000 energy at 100 sun (20 TRX), its fee leg that and 400 bytes.
#[test]
fn a_usdt_send_signs_a_trigger_smart_contract_with_its_fee_limit() {
    let script = Script::new([prices(), solidified()]);
    let factory = factory(&script);
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    let to = base::Address::new(network(), TO);
    let mut builder = wallet.transaction(&usdt()).unwrap();
    builder
        .from_transfer(Transfer::new(to.clone(), Decimal::from(1_000_000)))
        .unwrap();
    let transaction = block_on(builder.sign()).unwrap();
    script.exhausted();
    let raw = signed(&transaction);
    let mut contract = vec![0x0a, 0x15];
    contract.extend(hex_account(KEY_ONE));
    contract.extend([0x12, 0x15]);
    contract.extend(hex_account(USDT));
    contract.extend([0x22, 0x44]);
    contract.extend(
        hex::decode(
            "a9059cbb000000000000000000000000df76bd95259da7466dce0ef7e53c1e754d6f1518\
             00000000000000000000000000000000000000000000000000000000000f4240",
        )
        .unwrap(),
    );
    assert!(contains(&raw, &contract));
    assert!(contains(&raw, b"protocol.TriggerSmartContract"));
    // fee_limit (field 18): 20,000,000 sun.
    assert!(raw.ends_with(&hex::decode("900180dac409").unwrap()));
    let from = wallet.address().clone();
    assert_eq!(
        transaction.legs,
        vec![
            Leg::Spent {
                address: from.clone(),
                asset: usdt(),
                amount: Decimal::from(1_000_000),
                coin: None,
            },
            Leg::Received {
                address: to,
                asset: usdt(),
                amount: Decimal::from(1_000_000),
                coin: None,
            },
            Leg::Fee {
                address: from,
                asset: Asset::native(network()),
                amount: Decimal::from(20_400_000),
            },
        ]
    );
}

#[test]
fn a_fee_limit_above_its_ceiling_is_a_fee_cap() {
    let script = Script::new([prices()]);
    let mut config = config();
    config.limits.max_fee_limit = 10_000_000;
    let rpc = Client::new(Arc::new(script.clone()), config.native());
    let factory = Factory::from(config).wallets(&rpc, Arc::new(base::NoHoldings));
    let wallet = block_on(factory.from_pk(key_one())).unwrap();
    let mut builder = wallet.transaction(&usdt()).unwrap();
    builder
        .from_transfer(Transfer::new(
            base::Address::new(network(), TO),
            Decimal::from(1),
        ))
        .unwrap();
    let error = block_on(builder.sign()).unwrap_err();
    assert_eq!(error.kind, TransactionErrorKind::FeeCap);
}

/// The estimate is each send's fee leg: TRX to a new account 1.4 TRX,
/// USDT 20.4 TRX; the prices are asked once.
#[test]
fn an_estimate_is_the_fee_legs_a_builder_signs() {
    let script = Script::new([prices()]);
    let factory = factory(&script);
    let from = base::Address::new(network(), KEY_ONE);
    let spends: base::Spends = [(from.clone(), Asset::native(network())), (from, usdt())]
        .into_iter()
        .collect();
    let estimate = block_on(factory.fees().estimate(&spends)).unwrap();
    assert_eq!(estimate, Decimal::from(21_800_000));
    script.exhausted();
}

/// `broadcasthex` once, of the exact bytes; a node holding it accepted it,
/// a bad signature is refused, an expired one expired, a busy node unknown.
#[test]
fn a_broadcast_maps_each_refusal() {
    let transaction = base::Transaction {
        id: base::TransactionId::new(network(), "ab"),
        state: base::TransactionState::Pending,
        order: None,
        legs: Vec::new(),
        bytes: vec![0x0a, 0x00],
        expires_height: None,
    };
    let refused = |message: &str| {
        let script = Script::new([Reply::failure("broadcasthex", -32_000, message)
            .expecting(json!({"transaction": "0a00"}))])
        .once_only("broadcasthex");
        let rpc = Client::new(Arc::new(script), config().native());
        block_on(rpc.broadcast(&transaction))
    };
    assert!(refused("DUP_TRANSACTION_ERROR: dup transaction").is_ok());
    let invalid = refused("SIGERROR: Validate signature error").unwrap_err();
    assert_eq!(
        (invalid.kind, invalid.reason),
        (TransactionErrorKind::Rejected, Some(Reason::Invalid))
    );
    let poor = refused(
        "CONTRACT_VALIDATE_ERROR: Validate TransferContract error, balance is not sufficient.",
    )
    .unwrap_err();
    assert_eq!(poor.reason, Some(Reason::InsufficientFunds));
    let expired = refused("TRANSACTION_EXPIRATION_ERROR: expired").unwrap_err();
    assert_eq!(expired.kind, TransactionErrorKind::Expired);
    let busy = refused("SERVER_BUSY: busy").unwrap_err();
    assert_eq!(busy.kind, TransactionErrorKind::Unavailable);

    let accepted = Script::new([Reply::ok(
        "broadcasthex",
        json!({"result": true, "txid": "ab"}),
    )])
    .once_only("broadcasthex");
    let rpc = Client::new(Arc::new(accepted), config().native());
    assert!(block_on(rpc.broadcast(&transaction)).is_ok());
}
