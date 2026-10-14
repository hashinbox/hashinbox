//! Wallets: keys and addresses, building and signing, broadcasting.

use std::sync::Arc;

use base::{
    Address, Asset, Broadcaster as _, Coin, Future, Input, InputStore, Inputs, Leg, Reason,
    TransactionErrorKind, TransactionId, TransactionState, WalletError, WalletErrorKind,
    WalletFactory as _,
};
use bitcoin::{Transaction, consensus};
use chain_bitcoin::{AddressType, Client, Config, Factory, Network};
use crypto::SecretBytes;
use json_rpc::{
    Cluster, Policy,
    testing::{Reply, Script},
};
use serde_json::json;

/// One wallet's unspent outputs, as a store would answer.
struct Store(Vec<Input>);

impl InputStore for Store {
    fn find_unspent_for_address<'a>(
        &'a self,
        address: &'a Address,
    ) -> Future<'a, anyhow::Result<Inputs>> {
        let found: Vec<Input> = self
            .0
            .iter()
            .filter(|input| &input.address == address)
            .cloned()
            .collect();
        Box::pin(async move { Ok(found.into()) })
    }
}

fn network() -> base::Network {
    Network::Regtest.into()
}

fn factory(script: &Script, kind: AddressType, inputs: Vec<Input>) -> Factory {
    let rpc = Client::new(Arc::new(script.clone()), Network::Regtest);
    let config = Config {
        address_type: kind,
        ..Config::new(Network::Regtest, Vec::new())
    };
    config.factory(&rpc, Arc::new(Store(inputs)))
}

fn funded(owner: &Address, sats: u64) -> Input {
    Input {
        coin: Coin {
            transaction: TransactionId::new(network(), "07".repeat(32)),
            index: 1,
        },
        address: owner.clone(),
        asset: Asset::native(network()),
        amount: sats.into(),
        height: 10,
    }
}

async fn address_of(seed: u8) -> Address {
    let factory = factory(&Script::default(), AddressType::SegwitV0, Vec::new());
    let wallet = factory
        .from_pk(SecretBytes::new([seed; 32]))
        .await
        .expect("valid key");
    wallet.address().clone()
}

fn kind(error: &anyhow::Error) -> Option<WalletErrorKind> {
    error.downcast_ref::<WalletError>().map(|error| error.kind)
}

#[tokio::test]
async fn keys_and_addresses_round_trip() {
    let factory = factory(&Script::default(), AddressType::Taproot, Vec::new());
    let secret = factory.generate().await.expect("a key");
    let wallet = factory.from_pk(secret).await.expect("its wallet");
    assert!(wallet.address().value.starts_with("bcrt1p"), "taproot");
    let text = wallet.address().value.to_uppercase();
    assert_eq!(
        &factory.parse_address(&text).expect("any case"),
        wallet.address()
    );
    let (curve, bytes) = wallet.private_key();
    assert_eq!(curve, crypto::Curve::Secp256k1);
    assert_eq!(bytes.as_bytes().len(), 32);

    let invalid = factory.from_pk(SecretBytes::new([0; 32])).await.err();
    assert_eq!(
        invalid.as_ref().and_then(kind),
        Some(WalletErrorKind::InvalidSecret)
    );
    let mainnet = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";
    let other = factory.parse_address(mainnet).expect_err("another network");
    assert_eq!(kind(&other), Some(WalletErrorKind::InvalidAddress));
}

#[tokio::test]
async fn signs_one_pending_transaction_with_its_legs() {
    let recipient = address_of(2).await;
    for kind in [AddressType::SegwitV0, AddressType::Taproot] {
        let owner = factory(&Script::default(), kind, Vec::new())
            .from_pk(SecretBytes::new([1; 32]))
            .await
            .expect("valid key")
            .address()
            .clone();
        let script = Script::new([Reply::ok("estimatesmartfee", json!({"feerate": 0.00002}))
            .expecting(json!([6, "conservative"]))]);
        let factory = factory(&script, kind, vec![funded(&owner, 100_000)]);
        let wallet = factory
            .from_pk(SecretBytes::new([1; 32]))
            .await
            .expect("key");
        let mut builder = wallet.transaction(&Asset::native(network())).expect("BTC");
        builder
            .transfer(recipient.clone(), 40_000_u64.into())
            .expect("valid transfer");
        let mut signed = builder.sign().await.expect("signed");
        script.exhausted();
        assert_eq!(signed.len(), 1);
        let signed = signed.0.remove(0);
        assert_eq!(signed.state, TransactionState::Pending);
        assert_eq!(signed.order, None);
        let native: Transaction = consensus::deserialize(&signed.bytes).expect("decodes");
        assert_eq!(signed.id.id, native.compute_txid().to_string());
        assert!(
            native.input.iter().all(|input| !input.witness.is_empty()),
            "signed"
        );
        let change = native.output[1].value.to_sat();
        let fee = 100_000 - 40_000 - change;
        let vsize = u64::try_from(native.vsize()).expect("small");
        assert!(fee >= 2 * vsize, "at least 2 sat/vB: {fee} for {vsize} vB");
        let asset = Asset::native(network());
        let created = |index| {
            Some(Coin {
                transaction: signed.id.clone(),
                index,
            })
        };
        assert_eq!(
            signed.legs,
            vec![
                Leg::Received {
                    address: recipient.clone(),
                    asset: asset.clone(),
                    amount: 40_000_u64.into(),
                    coin: created(0)
                },
                Leg::Received {
                    address: owner.clone(),
                    asset: asset.clone(),
                    amount: change.into(),
                    coin: created(1)
                },
                Leg::Spent {
                    address: owner.clone(),
                    asset: asset.clone(),
                    amount: (40_000 + change).into(),
                    coin: Some(funded(&owner, 0).coin),
                },
                Leg::Fee {
                    address: owner.clone(),
                    asset,
                    amount: fee.into()
                },
            ]
        );
    }
}

#[tokio::test]
async fn without_an_estimate_the_floor_prices_the_fee() {
    let owner = address_of(1).await;
    let script = Script::new([Reply::ok(
        "estimatesmartfee",
        json!({"errors": ["few data"]}),
    )]);
    let factory = factory(
        &script,
        AddressType::SegwitV0,
        vec![funded(&owner, 100_000)],
    );
    let wallet = factory
        .from_pk(SecretBytes::new([1; 32]))
        .await
        .expect("key");
    let mut builder = wallet.transaction(&Asset::native(network())).expect("BTC");
    builder
        .transfer(address_of(2).await, 40_000_u64.into())
        .expect("valid");
    let signed = builder.sign().await.expect("signed");
    let native: Transaction = consensus::deserialize(&signed[0].bytes).expect("decodes");
    let fee = 100_000 - 40_000 - native.output[1].value.to_sat();
    let vsize = u64::try_from(native.vsize()).expect("small");
    assert!(
        (vsize..=vsize + 2).contains(&fee),
        "1 sat/vB: {fee} for {vsize}"
    );
}

#[tokio::test]
async fn refuses_what_it_cannot_send() {
    let owner = address_of(1).await;
    let script = Script::new([Reply::ok("estimatesmartfee", json!({"feerate": 0.00001}))]);
    let factory = factory(&script, AddressType::SegwitV0, vec![funded(&owner, 10_000)]);
    let wallet = factory
        .from_pk(SecretBytes::new([1; 32]))
        .await
        .expect("key");
    let token = Asset {
        network: network(),
        id: "token".to_owned(),
    };
    let unsupported = wallet.transaction(&token).err().map(|error| error.kind);
    assert_eq!(unsupported, Some(TransactionErrorKind::Unsupported));

    let mut builder = wallet.transaction(&Asset::native(network())).expect("BTC");
    let zero = builder.transfer(address_of(2).await, 0_u64.into());
    assert_eq!(
        zero.err().map(|error| error.kind),
        Some(TransactionErrorKind::Failed)
    );
    let foreign = Address::new(network(), "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4");
    assert!(builder.transfer(foreign, 1_000_u64.into()).is_err());
    builder
        .transfer(address_of(2).await, 40_000_u64.into())
        .expect("valid");
    let error = builder.sign().await.expect_err("too poor");
    assert_eq!(error.kind, TransactionErrorKind::Failed);
    assert_eq!(error.reason, Some(Reason::InsufficientFunds));

    let empty = wallet.transaction(&Asset::native(network())).expect("BTC");
    assert!(empty.sign().await.is_err(), "no transfers");
}

fn probe(node: &Script) -> json_rpc::BoxFuture<'_, Result<u64, json_rpc::CallError>> {
    use json_rpc::Client as _;
    Box::pin(async move {
        let reply = node
            .request(json_rpc::Request::new("getblockcount", json!([])))
            .await??;
        Ok(reply.deserialize()?)
    })
}

#[tokio::test]
async fn broadcasts_exact_bytes_to_every_node() {
    let bytes = hex_transaction();
    let nodes: Vec<Script> = (0..2)
        .map(|_| {
            Script::new([
                Reply::ok("getblockcount", json!(5)),
                Reply::ok("sendrawtransaction", json!("txid")).expecting(json!([
                    bitcoin::hex::DisplayHex::to_lower_hex_string(&bytes[..])
                ])),
            ])
            .once_only("sendrawtransaction")
        })
        .collect();
    let cluster = Cluster::new(nodes.clone(), probe, Policy::default())
        .await
        .expect("cluster");
    let rpc = Client::new(Arc::new(cluster), Network::Regtest);
    let broadcaster = rpc;
    broadcaster.broadcast(&bytes).await.expect("accepted");
    for node in &nodes {
        node.exhausted();
    }
}

#[tokio::test]
async fn maps_node_refusals() {
    let cases = [
        (-27, "transaction already in block chain", None),
        (-26, "txn-already-known", None),
        (
            -25,
            "bad-txns-inputs-missingorspent",
            Some(Reason::NonceUsed),
        ),
        (-26, "min relay fee not met", Some(Reason::FeeTooLow)),
        (-22, "TX decode failed", Some(Reason::Invalid)),
    ];
    for (code, message, reason) in cases {
        let script = Script::new([Reply::failure("sendrawtransaction", code, message)]);
        let rpc = Client::new(Arc::new(script), Network::Regtest);
        let broadcaster = rpc;
        let outcome = broadcaster.broadcast(&hex_transaction()).await;
        match reason {
            None => assert_eq!(outcome, Ok(()), "{message}"),
            Some(reason) => {
                let error = outcome.expect_err(message);
                assert_eq!(error.kind, TransactionErrorKind::Rejected);
                assert_eq!(error.reason, Some(reason));
            }
        }
    }
    let script = Script::new([Reply::failure("sendrawtransaction", -28, "warming up")]);
    let rpc = Client::new(Arc::new(script), Network::Regtest);
    let broadcaster = rpc;
    let error = broadcaster
        .broadcast(&hex_transaction())
        .await
        .expect_err("warming");
    assert_eq!(error.kind, TransactionErrorKind::Unavailable);
}

fn hex_transaction() -> Vec<u8> {
    let transaction = Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn::default()],
        output: vec![bitcoin::TxOut {
            value: bitcoin::Amount::from_sat(1_000),
            script_pubkey: bitcoin::ScriptBuf::new(),
        }],
    };
    consensus::serialize(&transaction)
}

/// The exact signed bytes: signing is deterministic (RFC 6979 ECDSA, BIP-340
/// Schnorr with zero auxiliary randomness), so any change in construction,
/// fee or signing shows here.
#[tokio::test]
async fn signed_bytes_are_golden() {
    let golden = [
        (
            AddressType::SegwitV0,
            "0200000000010107070707070707070707070707070707070707070707070707070707070707070100000000fdffffff02409c000000000000160014ebc0ee0b2ab9e8277a600c251475e22a3241a1c146e900000000000016001479b000887626b294a914501a4cd226b58b23598302483045022100a3221fc7f85b096e06aec2631f5579d9549b745415eb8d8fafe570caaca46f51022006da58c2382a8a111625cde38bd1c5c36b6869e1a425286270dcb50812e05cdc0121031b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f00000000",
        ),
        (
            AddressType::Taproot,
            "0200000000010107070707070707070707070707070707070707070707070707070707070707070100000000fdffffff02409c000000000000160014ebc0ee0b2ab9e8277a600c251475e22a3241a1c142e90000000000002251208c5db7f797196d6edc4dd7df6048f4ea6b883a6af6af032342088f436543790f01409a1e0d004fe36fe0c57a68552803c881eeb8ffee28f0ba9cfbc425953f92a8084743d7465bf8aa99ae41e4ada61eb401cc73280fc3f8b0ac889568013ccd5bca00000000",
        ),
    ];
    let recipient = address_of(2).await;
    for (kind, expected) in golden {
        let owner = factory(&Script::default(), kind, Vec::new())
            .from_pk(SecretBytes::new([1; 32]))
            .await
            .expect("valid key")
            .address()
            .clone();
        let script = Script::new([Reply::ok("estimatesmartfee", json!({"feerate": 0.00002}))]);
        let factory = factory(&script, kind, vec![funded(&owner, 100_000)]);
        let wallet = factory
            .from_pk(SecretBytes::new([1; 32]))
            .await
            .expect("key");
        let mut builder = wallet.transaction(&Asset::native(network())).expect("BTC");
        builder
            .transfer(recipient.clone(), 40_000_u64.into())
            .expect("valid transfer");
        let signed = builder.sign().await.expect("signed");
        let bytes = bitcoin::hex::DisplayHex::to_lower_hex_string(&signed[0].bytes[..]);
        assert_eq!(bytes, expected, "{kind:?}");
    }
}
