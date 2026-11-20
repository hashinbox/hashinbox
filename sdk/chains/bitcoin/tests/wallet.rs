//! Wallets: keys and addresses, building and signing, broadcasting.

use std::sync::Arc;

use ::rpc::testing::{Reply, Script};
use base::{
    Address, Asset, Broadcaster as _, Coin, Input, Leg, Reason, Signer, TransactionErrorKind,
    TransactionId, TransactionState, Transfer, WalletError, WalletErrorKind, WalletFactory as _,
    testing::Memory,
};
use bitcoin::{Transaction, consensus};
use chain_bitcoin::{
    AddressType, Client, Config, Factory, Network, WalletFactory, testing::fixtures::address_of,
};
use crypto::SecretBytes;
use serde_json::json;

/// `transfer`, signed by `builder`.
async fn pay(
    mut builder: Box<dyn base::Builder>,
    transfer: Transfer,
) -> Result<base::Transaction, base::TransactionError> {
    builder.from_transfer(transfer)?;
    builder.sign().await
}

/// The node's mempool minimum: 1 sat/vB.
fn minimum() -> Reply {
    Reply::ok("getmempoolinfo", json!({"mempoolminfee": 0.00001}))
}

fn network() -> base::Network {
    Network::Regtest.into()
}

fn factory(script: &Script, kind: AddressType, inputs: Vec<Input>) -> WalletFactory {
    let rpc = Client::new(Arc::new(script.clone()), Network::Regtest);
    let mut config = Config::new(Network::Regtest);
    config.address_type = kind;
    let store = Memory::new();
    store.unspent(inputs);
    Factory::from(config).wallets(&rpc, Arc::new(store))
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

fn kind(error: &anyhow::Error) -> Option<WalletErrorKind> {
    error.downcast_ref::<WalletError>().map(|error| error.kind)
}

/// `sources` as spends to new addresses.
fn spends(sources: &[(base::Address, Asset)]) -> base::Spends {
    sources.iter().cloned().collect()
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
    let recipient = address_of(2);
    for kind in [AddressType::SegwitV0, AddressType::Taproot] {
        let owner = factory(&Script::default(), kind, Vec::new())
            .from_pk(SecretBytes::new([1; 32]))
            .await
            .expect("valid key")
            .address()
            .clone();
        let script = Script::new([
            minimum(),
            Reply::ok("estimatesmartfee", json!({"feerate": 0.00002}))
                .expecting(json!([1, "conservative"])),
        ]);
        let factory = factory(&script, kind, vec![funded(&owner, 100_000)]);
        let wallet = factory
            .from_pk(SecretBytes::new([1; 32]))
            .await
            .expect("key");
        let builder = wallet.transaction(&Asset::native(network())).expect("BTC");
        let transfer = Transfer::new(recipient.clone(), 40_000_u64.into());
        let signed = pay(builder, transfer).await.expect("signed");
        script.exhausted();
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
    let owner = address_of(1);
    let script = Script::new([
        minimum(),
        Reply::ok("estimatesmartfee", json!({"errors": ["few data"]})),
    ]);
    let factory = factory(
        &script,
        AddressType::SegwitV0,
        vec![funded(&owner, 100_000)],
    );
    let wallet = factory
        .from_pk(SecretBytes::new([1; 32]))
        .await
        .expect("key");
    let builder = wallet.transaction(&Asset::native(network())).expect("BTC");
    let transfer = Transfer::new(address_of(2), 40_000_u64.into());
    let signed = pay(builder, transfer).await.expect("signed");
    let native: Transaction = consensus::deserialize(&signed.bytes).expect("decodes");
    let fee = 100_000 - 40_000 - native.output[1].value.to_sat();
    let vsize = u64::try_from(native.vsize()).expect("small");
    assert!(
        (vsize..=vsize + 2).contains(&fee),
        "1 sat/vB: {fee} for {vsize}"
    );
}

#[tokio::test]
async fn refuses_what_it_cannot_send() {
    let owner = address_of(1);
    let script = Script::new([
        minimum(),
        Reply::ok("estimatesmartfee", json!({"feerate": 0.00001})),
    ]);
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

    // Each refused before any I/O, then a valid one too large.
    let sign = |to: Address, sats: u64| {
        let builder = wallet.transaction(&Asset::native(network())).expect("BTC");
        pay(builder, Transfer::new(to, sats.into()))
    };
    let zero = sign(address_of(2), 0).await;
    assert_eq!(
        zero.err().map(|error| error.kind),
        Some(TransactionErrorKind::Failed)
    );
    let foreign = Address::new(network(), "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4");
    assert!(sign(foreign, 1_000).await.is_err());
    let error = sign(address_of(2), 40_000).await.expect_err("too poor");
    assert_eq!(error.kind, TransactionErrorKind::Failed);
    assert_eq!(error.reason, Some(Reason::InsufficientFunds));
    script.exhausted();
}

#[tokio::test]
async fn broadcasts_the_exact_bytes_once() {
    let bytes = hex_transaction();
    let node = Script::new([
        Reply::ok("sendrawtransaction", json!("txid")).expecting(json!([
            bitcoin::hex::DisplayHex::to_lower_hex_string(&bytes[..])
        ])),
    ])
    .once_only("sendrawtransaction");
    let broadcaster = Client::new(Arc::new(node.clone()), Network::Regtest);
    broadcaster
        .broadcast(&signed(&bytes))
        .await
        .expect("accepted");
    node.exhausted();
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
        let outcome = broadcaster.broadcast(&signed(&hex_transaction())).await;
        match reason {
            None => assert_eq!(outcome, Ok(()), "{message}"),
            Some(reason) => {
                let error = outcome.expect_err(message);
                assert_eq!(error.kind, TransactionErrorKind::Rejected);
                assert_eq!(error.reason, Some(reason));
                assert_eq!(error.message, message, "the node's text");
            }
        }
    }
    let script = Script::new([Reply::failure("sendrawtransaction", -28, "warming up")]);
    let rpc = Client::new(Arc::new(script), Network::Regtest);
    let broadcaster = rpc;
    let error = broadcaster
        .broadcast(&signed(&hex_transaction()))
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
    let recipient = address_of(2);
    for (kind, expected) in golden {
        let owner = factory(&Script::default(), kind, Vec::new())
            .from_pk(SecretBytes::new([1; 32]))
            .await
            .expect("valid key")
            .address()
            .clone();
        let script = Script::new(rate());
        let factory = factory(&script, kind, vec![funded(&owner, 100_000)]);
        let wallet = factory
            .from_pk(SecretBytes::new([1; 32]))
            .await
            .expect("key");
        let builder = wallet.transaction(&Asset::native(network())).expect("BTC");
        let transfer = Transfer::new(recipient.clone(), 40_000_u64.into());
        let signed = pay(builder, transfer).await.expect("signed");
        let bytes = bitcoin::hex::DisplayHex::to_lower_hex_string(&signed.bytes[..]);
        assert_eq!(bytes, expected, "{kind:?}");
    }
}

fn funded_at(owner: &Address, sats: u64, index: u32) -> Input {
    Input {
        coin: Coin {
            transaction: TransactionId::new(network(), "07".repeat(32)),
            index,
        },
        ..funded(owner, sats)
    }
}

#[tokio::test]
async fn a_transfer_from_its_own_source_spends_its_coins_with_change_to_the_default() {
    let segwit = factory(&Script::default(), AddressType::SegwitV0, Vec::new())
        .from_pk(SecretBytes::new([1; 32]))
        .await
        .expect("key");
    let taproot = factory(&Script::default(), AddressType::Taproot, Vec::new())
        .from_pk(SecretBytes::new([3; 32]))
        .await
        .expect("key");
    let script = Script::new(rate());
    let factory = factory(
        &script,
        AddressType::SegwitV0,
        vec![
            funded_at(segwit.address(), 30_000, 1),
            funded_at(taproot.address(), 60_000, 2),
        ],
    );
    // Segwit is the default source; the transfer is paid by taproot.
    let mut builder = factory.transaction(&Asset::native(network())).expect("BTC");
    builder.with_default_source(Signer::from(&segwit));
    let transfer = Transfer::new(address_of(2), 40_000_u64.into()).from(&taproot);
    let signed = pay(builder, transfer).await.expect("signed");
    script.exhausted();
    let native: Transaction = consensus::deserialize(&signed.bytes).expect("decodes");
    let [input] = &native.input[..] else {
        panic!("the source's one coin: {:?}", native.input);
    };
    assert_eq!(input.previous_output.vout, 2, "taproot's coin");
    assert_eq!(input.witness.len(), 1, "P2TR key path: one signature");
    let first = input.witness.nth(0).expect("a signature");
    assert_eq!(first.len(), 64, "Schnorr, default sighash");
    let change = native.output.last().expect("change");
    let owner =
        chain_bitcoin::Address::parse(&segwit.address().value, Network::Regtest).expect("address");
    assert_eq!(
        change.script_pubkey,
        owner.script_pubkey(),
        "the default source"
    );
}

#[tokio::test]
async fn a_factory_builder_needs_a_source_whose_key_signs_for_it() {
    let owner = address_of(1);
    let script = Script::new([
        minimum(),
        Reply::ok("estimatesmartfee", json!({"feerate": 0.00001})),
        minimum(),
        Reply::ok("estimatesmartfee", json!({"feerate": 0.00001})),
    ]);
    let factory = factory(
        &script,
        AddressType::SegwitV0,
        vec![funded(&owner, 100_000)],
    );
    let builder = factory.transaction(&Asset::native(network())).expect("BTC");
    let transfer = Transfer::new(address_of(2), 1_000_u64.into());
    let error = pay(builder, transfer).await.expect_err("no source");
    assert_eq!(error.kind, TransactionErrorKind::Failed);

    let builder = factory.transaction(&Asset::native(network())).expect("BTC");
    let wrong = Signer::new(owner, crypto::Curve::Secp256k1, SecretBytes::new([3; 32]));
    let transfer = Transfer::new(address_of(2), 1_000_u64.into()).from(wrong);
    let error = pay(builder, transfer).await.expect_err("mismatch");
    assert_eq!(error.kind, TransactionErrorKind::Failed);
}

/// The rate now: a mempool minimum of 1 sat/vB, an estimate of 2.
fn rate() -> [Reply; 2] {
    [
        minimum(),
        Reply::ok("estimatesmartfee", json!({"feerate": 0.00002})),
    ]
}

#[tokio::test]
async fn an_estimate_prices_every_source_in_one_transaction() {
    let segwit = address_of(1);
    let taproot = factory(&Script::default(), AddressType::Taproot, Vec::new())
        .from_pk(SecretBytes::new([3; 32]))
        .await
        .expect("key")
        .address()
        .clone();
    let script = Script::new(rate());
    let coins = vec![
        funded_at(&segwit, 30_000, 1),
        funded_at(&taproot, 30_000, 2),
    ];
    let fees = factory(&script, AddressType::SegwitV0, coins).fees();
    let btc = Asset::native(network());

    // 2 sat/vB: 122 vB for one P2WPKH input and a 34-byte output.
    let one = fees
        .estimate(&spends(&[(segwit.clone(), btc.clone())]))
        .await;
    assert_eq!(one.expect("priced"), 244_u64.into());
    // One transaction of 180 vB, not the sum of two.
    let both = fees
        .estimate(&spends(&[(segwit, btc.clone()), (taproot, btc)]))
        .await;
    assert_eq!(both.expect("priced"), 360_u64.into());
    script.exhausted();
}

#[tokio::test]
async fn the_fee_rate_refreshes_once_per_time_to_live() {
    let owner = address_of(1);
    let source = [(owner.clone(), Asset::native(network()))];
    let script = Script::new(rate());
    let fees = factory(&script, AddressType::SegwitV0, vec![funded(&owner, 1_000)]).fees();
    fees.estimate(&spends(&source)).await.expect("priced");
    fees.estimate(&spends(&source)).await.expect("priced");
    script.exhausted();

    let script = Script::new(rate().into_iter().chain(rate()));
    let rpc = Client::new(Arc::new(script.clone()), Network::Regtest);
    let mut config = Config::new(Network::Regtest);
    config.fee_ttl_ms = 0;
    let store = Memory::new();
    store.unspent([funded(&owner, 1_000)]);
    let fees = Factory::from(config).wallets(&rpc, Arc::new(store)).fees();
    fees.estimate(&spends(&source)).await.expect("priced");
    fees.estimate(&spends(&source)).await.expect("priced");
    script.exhausted();
}

#[tokio::test]
async fn a_sweep_of_balance_less_estimate_pays_the_estimate_without_change() {
    let script = Script::new(rate());
    let owner = address_of(1);
    let factory = factory(
        &script,
        AddressType::SegwitV0,
        vec![funded(&owner, 100_000)],
    );
    let wallet = factory
        .from_pk(SecretBytes::new([1; 32]))
        .await
        .expect("key");
    let fee = factory
        .fees()
        .estimate(&spends(&[(owner, Asset::native(network()))]))
        .await
        .expect("priced");
    let builder = wallet.transaction(&Asset::native(network())).expect("BTC");
    let amount = base::Decimal::from(100_000_u64) - fee.clone();
    let signed = pay(builder, Transfer::new(address_of(2), amount))
        .await
        .expect("signed");
    script.exhausted();
    let native: Transaction = consensus::deserialize(&signed.bytes).expect("decodes");
    assert_eq!(native.output.len(), 1, "no change");
    let paid = signed.legs.iter().find_map(|leg| match leg {
        Leg::Fee { amount, .. } => Some(amount.clone()),
        _ => None,
    });
    assert_eq!(paid, Some(fee));
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

#[tokio::test]
async fn a_compressed_wif_of_its_network_imports() {
    let factory = factory(&Script::default(), AddressType::SegwitV0, Vec::new());
    let wif = |compressed, network| {
        bitcoin::PrivateKey {
            compressed,
            network,
            inner: bitcoin::secp256k1::SecretKey::from_slice(&[7; 32]).unwrap(),
        }
        .to_wif()
    };
    let wallet = factory
        .from_pk_text(&wif(true, bitcoin::NetworkKind::Test))
        .await
        .unwrap();
    assert_eq!(wallet.private_key().1.as_bytes(), [7; 32]);
    for bad in [
        wif(false, bitcoin::NetworkKind::Test),
        wif(true, bitcoin::NetworkKind::Main),
        "not a key".to_owned(),
    ] {
        let error = factory.from_pk_text(&bad).await.err().unwrap();
        assert_eq!(
            kind(&error),
            Some(WalletErrorKind::InvalidSecret),
            "{bad:?}"
        );
    }
}

#[tokio::test]
async fn a_bump_spends_the_same_coins_its_change_paying_the_higher_fee() {
    let owner = address_of(1);
    let script = Script::new(rate());
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
        .from_transfer(Transfer::new(address_of(2), 40_000_u64.into()))
        .expect("valid");
    let first = builder.sign().await.expect("signed");
    let state = builder.save().expect("saved");

    // A new builder, at the rate kept: no node call.
    let mut bump = wallet.transaction(&Asset::native(network())).expect("BTC");
    bump.load(&state).expect("loaded");
    assert_eq!(bump.save().expect("saved"), state, "the round trip");
    bump.with_fee(base::Fee::Times(1.1));
    let next = bump.sign().await.expect("signed");
    script.exhausted();
    let (old, new): (Transaction, Transaction) = (
        consensus::deserialize(&first.bytes).expect("decodes"),
        consensus::deserialize(&next.bytes).expect("decodes"),
    );
    assert_ne!(first.id, next.id);
    assert_eq!(old.input[0].previous_output, new.input[0].previous_output);
    assert_eq!(old.output[0], new.output[0], "the same payment");
    let fee = |native: &Transaction| 100_000 - 40_000 - native.output[1].value.to_sat();
    let vsize = u64::try_from(new.vsize()).expect("small");
    // The old fee, plus the bumped base (1.1 sat/vB) over its size.
    assert!(fee(&new) >= fee(&old) + vsize * 11 / 10, "{}", fee(&new));
}

#[tokio::test]
async fn a_cancel_spends_the_same_coins_back_to_its_source() {
    let owner = address_of(1);
    let script = Script::new(rate());
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
        .from_transfer(Transfer::new(address_of(2), 40_000_u64.into()))
        .expect("valid");
    let first = builder.sign().await.expect("signed");
    let state = builder.save().expect("saved");

    let mut cancel = wallet.transaction(&Asset::native(network())).expect("BTC");
    cancel.load(&state).expect("loaded");
    let source = wallet.address().clone();
    let replace = cancel.replaceable().expect("replaces");
    replace
        .replace(Transfer::new(source, base::Decimal::zero()))
        .expect("cancelled");
    let next = cancel.sign().await.expect("signed");
    let (old, new): (Transaction, Transaction) = (
        consensus::deserialize(&first.bytes).expect("decodes"),
        consensus::deserialize(&next.bytes).expect("decodes"),
    );
    assert_eq!(old.input[0].previous_output, new.input[0].previous_output);
    assert_eq!(new.output.len(), 1, "one output");
    assert_eq!(
        new.output[0].script_pubkey, old.output[1].script_pubkey,
        "back to the source"
    );
    let old_fee = 100_000 - 40_000 - old.output[1].value.to_sat();
    let fee = 100_000 - new.output[0].value.to_sat();
    let vsize = u64::try_from(new.vsize()).expect("small");
    assert!(fee >= old_fee + vsize * 11 / 10, "{fee}");

    // Its state loads as a cancel: a cancel again, higher still.
    let mut again = wallet.transaction(&Asset::native(network())).expect("BTC");
    again.load(&cancel.save().expect("saved")).expect("loaded");
    again.with_fee(base::Fee::Times(1.1));
    let last: Transaction =
        consensus::deserialize(&again.sign().await.expect("signed").bytes).expect("decodes");
    script.exhausted();
    assert_eq!(last.output.len(), 1);
    assert!(last.output[0].value < new.output[0].value, "a higher fee");
}

#[tokio::test]
async fn a_bump_its_change_cannot_pay_is_short_of_funds() {
    let owner = address_of(1);
    let script = Script::new(rate());
    // 40 000, a 282 sat fee and 318 of change, just above dust.
    let factory = factory(&script, AddressType::SegwitV0, vec![funded(&owner, 40_600)]);
    let wallet = factory
        .from_pk(SecretBytes::new([1; 32]))
        .await
        .expect("key");
    let mut builder = wallet.transaction(&Asset::native(network())).expect("BTC");
    builder
        .from_transfer(Transfer::new(address_of(2), 40_000_u64.into()))
        .expect("valid");
    let first = builder.sign().await.expect("signed");
    let native: Transaction = consensus::deserialize(&first.bytes).expect("decodes");
    assert_eq!(native.output.len(), 2, "with change");
    builder.with_fee(base::Fee::Times(1.1));
    let error = builder.sign().await.expect_err("too little change");
    assert_eq!(error.kind, TransactionErrorKind::Failed);
    assert_eq!(error.reason, Some(Reason::InsufficientFunds));
}
