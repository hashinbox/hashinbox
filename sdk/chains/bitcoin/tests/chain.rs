//! Blocks into `base` blocks: legs, tips, batched linking and reorgs.

use std::sync::Arc;

use ::rpc::testing::{Reply, Script};
use base::{Addressed as _, Asset, Chained, Coin, Leg, TransactionId, TransactionState};
use bitcoin::{
    Amount, BlockHash, Denomination, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut,
    Witness, absolute, consensus::encode::serialize_hex, hashes::Hash as _, transaction::Version,
};
use chain_bitcoin::{AddressType, Client, Config, KeyPair, Network};
use crypto::SecretBytes;
use serde_json::{Value, json};

fn key_of(seed: u8) -> KeyPair {
    KeyPair::import(
        &SecretBytes::new([seed; 32]),
        Network::Regtest,
        AddressType::SegwitV0,
    )
    .expect("valid key")
}

fn script_of(seed: u8) -> ScriptBuf {
    key_of(seed).native_address().script_pubkey()
}

fn address_of(seed: u8) -> base::Address {
    key_of(seed).address().clone()
}

fn transaction(inputs: Vec<OutPoint>, outputs: Vec<(ScriptBuf, u64)>) -> Transaction {
    Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: inputs
            .into_iter()
            .map(|previous_output| TxIn {
                previous_output,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            })
            .collect(),
        output: outputs
            .into_iter()
            .map(|(script_pubkey, sats)| TxOut {
                value: Amount::from_sat(sats),
                script_pubkey,
            })
            .collect(),
    }
}

fn coinbase(height: u8, to: ScriptBuf, sats: u64) -> Transaction {
    let mut coinbase = transaction(vec![OutPoint::null()], vec![(to, sats)]);
    coinbase.input[0].script_sig = ScriptBuf::from_bytes(vec![1, height]);
    coinbase
}

fn btc(sats: u64) -> Value {
    serde_json::from_str(&Amount::from_sat(sats).to_string_in(Denomination::Bitcoin))
        .expect("a number")
}

fn hash(seed: u8) -> BlockHash {
    BlockHash::from_byte_array([seed; 32])
}

/// `getblock <hash> 3` of a block of `transactions`, each with its prevouts.
fn block(height: u64, own: u8, parent: u8, transactions: &[(&Transaction, Vec<TxOut>)]) -> Value {
    let tx: Vec<Value> = transactions
        .iter()
        .map(|(transaction, spent)| {
            let vin: Vec<Value> = if transaction.is_coinbase() {
                vec![json!({"coinbase": "01"})]
            } else {
                spent
                    .iter()
                    .map(|output| {
                        json!({"prevout": {
                            "value": btc(output.value.to_sat()),
                            "scriptPubKey": {"hex": output.script_pubkey.to_hex_string()},
                        }})
                    })
                    .collect()
            };
            json!({
                "txid": transaction.compute_txid().to_string(),
                "hex": serialize_hex(*transaction),
                "vin": vin,
            })
        })
        .collect();
    json!({
        "hash": hash(own).to_string(),
        "height": height,
        "previousblockhash": hash(parent).to_string(),
        "time": 1_700_000_000_u64 + height,
        "tx": tx,
    })
}

fn chain(script: &Script, depth: u64) -> chain_bitcoin::Chain {
    let rpc = Client::new(Arc::new(script.clone()), Network::Regtest);
    let mut config = Config::new(Network::Regtest, Vec::new());
    config.confirmation_depth = depth;
    config.chain(&rpc).expect("valid depth")
}

#[tokio::test]
async fn confirmed_tip_is_depth_minus_one_below_produced() {
    for (count, depth, confirmed) in [(100, 6, 95), (100, 1, 100), (3, 6, 0)] {
        let script = Script::new([Reply::ok("getblockcount", json!(count))]);
        let tips = chain(&script, depth).tips().await.expect("tips");
        assert_eq!((tips.produced, tips.confirmed), (count, confirmed));
    }
    let script = Script::default();
    let rpc = Client::new(Arc::new(script), Network::Regtest);
    assert!(
        chain_bitcoin::Chain::new(rpc, 0).is_err(),
        "depth 0 is refused"
    );
}

#[tokio::test]
async fn blocks_are_batched_linked_and_carry_every_leg() {
    let (alice, bob, carol) = (script_of(1), script_of(2), script_of(3));
    let mined = coinbase(1, alice.clone(), 50_000);
    let other = coinbase(9, carol.clone(), 7_000);
    let alice_coin = TxOut {
        value: Amount::from_sat(50_000),
        script_pubkey: alice.clone(),
    };
    let carol_coin = TxOut {
        value: Amount::from_sat(7_000),
        script_pubkey: carol.clone(),
    };
    // Alice's 50,000 fills Bob's 30,000 and 20,000 of Carol's 26,000; Carol's
    // 7,000 fills the last 6,000 and leaves the 1,000 fee.
    let spend = transaction(
        vec![
            OutPoint::new(mined.compute_txid(), 0),
            OutPoint::new(other.compute_txid(), 0),
        ],
        vec![(bob.clone(), 30_000), (carol.clone(), 26_000)],
    );
    let script = Script::new([
        Reply::ok("getblockhash", json!(hash(1).to_string())).expecting(json!([1])),
        Reply::ok("getblockhash", json!(hash(2).to_string())).expecting(json!([2])),
        Reply::failure("getblockhash", -8, "Block height out of range"),
        Reply::ok("getblock", block(1, 1, 0, &[(&mined, Vec::new())]))
            .expecting(json!([hash(1).to_string(), 3])),
        Reply::ok(
            "getblock",
            block(2, 2, 1, &[(&spend, vec![alice_coin, carol_coin])]),
        ),
    ]);
    let run = chain(&script, 6).blocks(1, 3).await.expect("blocks");
    script.exhausted();
    assert_eq!(run.len(), 2, "cut at the tip");
    let header = &run[1].header;
    assert_eq!((header.position, header.height), (2, 2));
    assert_eq!(
        header.parent.as_ref().map(|parent| parent.position),
        Some(1)
    );
    assert_eq!(header.timestamp, Some(1_700_000_002));

    let network: base::Network = Network::Regtest.into();
    let asset = Asset::native(network.clone());
    let coin = |transaction: &Transaction, index| Coin {
        transaction: TransactionId::new(network.clone(), transaction.compute_txid().to_string()),
        index,
    };
    let coinbase = &run[0].transactions.0[0];
    assert_eq!(
        coinbase.legs,
        vec![Leg::Received {
            address: address_of(1),
            asset: asset.clone(),
            amount: 50_000_u64.into(),
            coin: Some(coin(&mined, 0))
        }]
    );
    let spent = &run[1].transactions.0[0];
    assert_eq!(spent.state, TransactionState::Succeeded);
    assert_eq!(spent.order, Some(0));
    assert_eq!(spent.id.id, spend.compute_txid().to_string());
    assert_eq!(spent.bytes, bitcoin::consensus::serialize(&spend));
    assert_eq!(
        spent.legs,
        vec![
            Leg::Received {
                address: address_of(2),
                asset: asset.clone(),
                amount: 30_000_u64.into(),
                coin: Some(coin(&spend, 0))
            },
            Leg::Received {
                address: address_of(3),
                asset: asset.clone(),
                amount: 26_000_u64.into(),
                coin: Some(coin(&spend, 1))
            },
            Leg::Spent {
                address: address_of(1),
                asset: asset.clone(),
                amount: 50_000_u64.into(),
                coin: Some(coin(&mined, 0)),
            },
            Leg::Spent {
                address: address_of(3),
                asset: asset.clone(),
                amount: 6_000_u64.into(),
                coin: Some(coin(&other, 0)),
            },
            Leg::Fee {
                address: address_of(3),
                asset,
                amount: 1_000_u64.into()
            },
        ]
    );
}

#[tokio::test]
async fn non_standard_outputs_have_no_leg() {
    let data = ScriptBuf::new_op_return([7_u8; 4]);
    let mut mined = coinbase(1, script_of(1), 50_000);
    mined.output.push(TxOut {
        value: Amount::ZERO,
        script_pubkey: data,
    });
    let script = Script::new([
        Reply::ok("getblockhash", json!(hash(1).to_string())),
        Reply::ok("getblock", block(1, 1, 0, &[(&mined, Vec::new())])),
    ]);
    let block = chain(&script, 6)
        .block(1)
        .await
        .expect("read")
        .expect("a block");
    assert_eq!(block.transactions.0[0].legs.len(), 1);
}

#[tokio::test]
async fn a_position_above_the_tip_is_none() {
    let script = Script::new([Reply::failure("getblockhash", -8, "out of range")]);
    assert_eq!(chain(&script, 6).block(9).await.expect("read"), None);
}

#[tokio::test]
async fn a_run_that_does_not_link_is_refused() {
    let one = coinbase(1, script_of(1), 1_000);
    let two = coinbase(2, script_of(1), 1_000);
    let script = Script::new([
        Reply::ok("getblockhash", json!(hash(1).to_string())),
        Reply::ok("getblockhash", json!(hash(2).to_string())),
        Reply::ok("getblock", block(1, 1, 0, &[(&one, Vec::new())])),
        // Block 2 names another parent: the chain changed between the batches.
        Reply::ok("getblock", block(2, 2, 7, &[(&two, Vec::new())])),
    ]);
    let error = chain(&script, 6).blocks(1, 2).await.expect_err("unlinked");
    assert!(error.to_string().contains("does not link"), "{error}");
}

#[tokio::test]
async fn a_refetch_after_a_reorg_no_longer_holds_the_old_tip() {
    let one = coinbase(1, script_of(1), 1_000);
    let two = coinbase(2, script_of(1), 1_000);
    let replaced = coinbase(3, script_of(2), 1_000);
    let script = Script::new([
        Reply::ok("getblockhash", json!(hash(1).to_string())),
        Reply::ok("getblockhash", json!(hash(2).to_string())),
        Reply::ok("getblock", block(1, 1, 0, &[(&one, Vec::new())])),
        Reply::ok("getblock", block(2, 2, 1, &[(&two, Vec::new())])),
        Reply::ok("getblockhash", json!(hash(1).to_string())),
        Reply::ok("getblockhash", json!(hash(3).to_string())),
        Reply::ok("getblock", block(1, 1, 0, &[(&one, Vec::new())])),
        Reply::ok("getblock", block(2, 3, 1, &[(&replaced, Vec::new())])),
    ]);
    let chain = chain(&script, 6);
    let before = chain.blocks(1, 2).await.expect("first branch");
    let old = before.last().expect("a tip").clone();
    let after = chain.blocks(1, 2).await.expect("second branch");
    assert!(!after.contains(&old), "the old tip was taken back");
    assert!(after.contains(&before[0].header), "the common parent stays");
}

#[tokio::test]
async fn a_chain_over_a_swap_follows_its_replacement() {
    let old = Script::new([Reply::ok("getblockcount", json!(10))]);
    let swap = Arc::new(::rpc::Swap::new(Arc::new(old.clone()), Vec::new()));
    let rpc = Client::new(swap.clone(), Network::Regtest);
    let chain = Config::new(Network::Regtest, Vec::new())
        .chain(&rpc)
        .expect("chain");
    assert_eq!(chain.tips().await.expect("old node").produced, 10);

    let next = Script::new([Reply::ok("getblockcount", json!(20))]);
    swap.replace(Arc::new(next.clone()), Vec::new());
    assert_eq!(chain.tips().await.expect("new node").produced, 20);
    old.exhausted();
    next.exhausted();
}
