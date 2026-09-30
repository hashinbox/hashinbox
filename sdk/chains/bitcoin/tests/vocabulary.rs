//! The chain vocabulary (docs/CHAIN_LAYOUT.md), exercised through the public
//! API: `RawBlock` → `parse` → `Block` → `Transaction` → `movements`.
//! Every chain has this file; only the `use` line and the fixture differ.

use base::{BlockPosition, MovementKind, Networked};
use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction as NativeTransaction, TxIn, TxOut, Witness,
    absolute, consensus, hashes::Hash, hex::DisplayHex, transaction::Version,
};
use chain::{
    Network, RpcClient,
    native::block::{Block, RawBlock, Transaction},
};
use chain_bitcoin as chain;
use json_rpc::{Batch, BoxFuture, CallResult, RawJson, Request};
use serde_json::{Value, json};

/// Script of the P2WPKH output the previous transaction pays.
fn owner_script() -> ScriptBuf {
    ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([7; 20]))
}

fn transaction(input: OutPoint, value: u64, script: ScriptBuf) -> NativeTransaction {
    NativeTransaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: input,
            script_sig: ScriptBuf::from_bytes(vec![1, 1]),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value),
            script_pubkey: script,
        }],
    }
}

fn previous() -> NativeTransaction {
    transaction(OutPoint::null(), 50_000, owner_script())
}

fn spending() -> NativeTransaction {
    let input = OutPoint::new(previous().compute_txid(), 0);
    transaction(input, 49_000, ScriptBuf::new())
}

fn hash(byte: u8) -> String {
    bitcoin::BlockHash::from_byte_array([byte; 32]).to_string()
}

/// A regtest Bitcoin Core 31 node whose block 10 spends one earlier output.
struct Node;

impl Node {
    fn reply(method: &str) -> Value {
        let spending = spending();
        match method {
            "getnetworkinfo" => json!({"version": 310000}),
            "getblockchaininfo" => json!({
                "chain": "regtest", "blocks": 10, "headers": 10, "bestblockhash": hash(2),
                "initialblockdownload": false, "pruned": false,
            }),
            "getindexinfo" => json!({"txindex": {"synced": true, "best_block_height": 10}}),
            "getblockhash" => json!(hash(2)),
            "getblock" => json!({
                "hash": hash(2), "height": 10, "previousblockhash": hash(3), "time": 100, "nTx": 1,
                "tx": [{
                    "txid": spending.compute_txid().to_string(),
                    "hex": consensus::serialize(&spending).to_lower_hex_string(),
                    "vin": [{"txid": previous().compute_txid().to_string(), "vout": 0}],
                    "vout": [{"value": 0.00049, "n": 0, "scriptPubKey": {"hex": ""}}],
                }],
            }),
            "getrawtransaction" => json!({
                "txid": previous().compute_txid().to_string(),
                "hex": consensus::serialize(&previous()).to_lower_hex_string(),
                "blockhash": hash(5),
            }),
            other => panic!("unexpected {other}"),
        }
    }
}

impl json_rpc::Client for Node {
    fn request<'a>(
        &'a self,
        request: Request,
    ) -> BoxFuture<'a, Result<CallResult, json_rpc::Error>> {
        let reply = RawJson::from_serializable(&Self::reply(&request.into_call().method)).map(Ok);
        Box::pin(async move { reply })
    }

    fn batch<'a>(
        &'a self,
        _batch: Batch,
    ) -> BoxFuture<'a, Result<Vec<CallResult>, json_rpc::Error>> {
        panic!("the fixture node serves single calls")
    }
}

#[test]
fn raw_block_parses_into_transactions_and_movements() {
    let network = Network::Regtest;
    let client = futures_executor::block_on(RpcClient::connect(Node, &network)).unwrap();
    let raw: RawBlock = futures_executor::block_on(client.block(BlockPosition(10)))
        .unwrap()
        .expect("produced block");
    let block: Block = raw.parse().unwrap();
    assert_eq!(block.network(), &network.canonical());
    assert_eq!(block.reference(), raw.reference());
    assert_eq!(block.reference().height.0, 10);

    let transactions: Vec<&Transaction> = block.transactions().iter().collect();
    assert_eq!(transactions.len(), 1);
    let transaction = transactions[0];
    assert_eq!(
        transaction.id().to_string(),
        spending().compute_txid().to_string()
    );
    assert!(transaction.succeeded());
    assert_eq!(transaction.fee().map(|fee| fee.amount.units()), Some(1_000));
    assert_eq!(
        block.transactions().get(transaction.id()),
        Some(transaction)
    );

    let movements = transaction.movements();
    assert_eq!(movements.len(), 2);
    assert_eq!(movements[0].kind, MovementKind::Input);
    assert_eq!(movements[0].amount.units(), 50_000);
    assert!(movements[0].from.is_some());
    assert_eq!(movements[1].kind, MovementKind::Output);
    assert_eq!(movements[1].to, None);

    // Chain detail is always one field deep on the transaction.
    let previous = transaction.inputs[0].previous.as_ref().expect("resolved");
    assert_eq!(previous.script_pubkey, owner_script().into_bytes());
}
