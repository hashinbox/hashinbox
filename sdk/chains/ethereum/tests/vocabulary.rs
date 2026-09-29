//! The chain vocabulary (docs/CHAIN_LAYOUT.md), exercised through the public
//! API: `RawBlock` → `parse` → `Block` → `Transaction` → `movements`.
//! Every chain has this file; only the `use` line and the fixture differ.

use base::{BlockPosition, MovementKind};
use chain::{
    Network, RpcClient,
    native::block::{Block, RawBlock, Transaction},
};
use chain_ethereum as chain;
use json_rpc::{Batch, BoxFuture, CallResult, RawJson, Request};
use serde_json::{Value, json};

const BLOCK_HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TX_HASH: &str = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const FROM: &str = "0x1111111111111111111111111111111111111111";
const TO: &str = "0x2222222222222222222222222222222222222222";

/// A node that knows one block at height 10 with one ether transfer.
struct Node;

impl Node {
    fn reply(method: &str) -> Value {
        match method {
            "eth_chainId" => json!("0x7a69"),
            "eth_getBlockByNumber" => json!({
                "hash": BLOCK_HASH,
                "parentHash": format!("0x{}", "bb".repeat(32)),
                "sha3Uncles": format!("0x{}", "dd".repeat(32)),
                "miner": format!("0x{}", "00".repeat(20)),
                "stateRoot": format!("0x{}", "ee".repeat(32)),
                "transactionsRoot": format!("0x{}", "ff".repeat(32)),
                "receiptsRoot": format!("0x{}", "ab".repeat(32)),
                "logsBloom": format!("0x{}", "00".repeat(256)),
                "difficulty": "0x0", "number": "0xa", "gasLimit": "0x1c9c380",
                "gasUsed": "0x5208", "timestamp": "0x64", "extraData": "0x",
                "mixHash": format!("0x{}", "cd".repeat(32)), "nonce": "0x0000000000000000",
                "uncles": [],
                "transactions": [{
                    "hash": TX_HASH, "from": FROM, "to": TO, "value": "0x2a",
                    "transactionIndex": "0x0", "blockHash": BLOCK_HASH, "blockNumber": "0xa"
                }]
            }),
            // The sender pays 42 wei of value plus 63,000 wei of gas, and the
            // recipient receives the 42: nothing is left for a residual.
            "debug_traceBlockByHash" => json!([{
                "txHash": TX_HASH,
                "result": {
                    "pre": {FROM: {"balance": "0xf642"}, TO: {"balance": "0x0"}},
                    "post": {FROM: {"balance": "0x0"}, TO: {"balance": "0x2a"}}
                }
            }]),
            "eth_getBlockReceipts" => json!([{
                "transactionHash": TX_HASH, "transactionIndex": "0x0",
                "blockHash": BLOCK_HASH, "blockNumber": "0xa", "from": FROM, "to": TO,
                "contractAddress": null, "status": "0x1", "gasUsed": "0x5208",
                "effectiveGasPrice": "0x3", "logs": []
            }]),
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
        panic!("the fixture node serves eth_getBlockReceipts")
    }
}

#[test]
fn raw_block_parses_into_transactions_and_movements() {
    let network = Network::new(31_337, "anvil");
    let client = futures_executor::block_on(RpcClient::connect(Node, &network)).unwrap();
    let raw: RawBlock = futures_executor::block_on(client.block(BlockPosition(10)))
        .unwrap()
        .expect("produced block");
    let block: Block = raw.parse().unwrap();
    assert_eq!(block.reference(), raw.reference());
    assert_eq!(block.reference().height.0, 10);

    let transactions: Vec<&Transaction> = block.transactions().iter().collect();
    assert_eq!(transactions.len(), 1);
    let transaction = transactions[0];
    assert_eq!(transaction.id().to_string(), TX_HASH);
    assert!(transaction.succeeded());
    assert_eq!(
        transaction.fee().map(|fee| fee.amount.to_string()),
        Some("63000".to_owned())
    );
    assert_eq!(
        block.transactions().get(transaction.id()),
        Some(transaction)
    );

    let movements = transaction.movements();
    assert_eq!(movements.len(), 1);
    assert_eq!(movements[0].kind, MovementKind::Transfer);
    assert_eq!(movements[0].amount.to_string(), "42");
    assert_eq!(
        movements[0]
            .from
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some(FROM)
    );
    assert_eq!(
        movements[0].to.as_ref().map(ToString::to_string).as_deref(),
        Some(TO)
    );

    // Chain detail is always one field deep on the transaction.
    assert!(transaction.receipt.logs.is_empty());
}
