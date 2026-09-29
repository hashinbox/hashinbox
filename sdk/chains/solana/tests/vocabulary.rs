//! The chain vocabulary (docs/CHAIN_LAYOUT.md), exercised through the public
//! API: `RawBlock` → `parse` → `Block` → `Transaction` → `movements`.
//! Every chain has this file; only the `use` line and the fixture differ.

use base::{BlockPosition, MovementKind};
use chain::{
    Address, Network, RpcClient,
    native::block::{Block, RawBlock, Transaction},
};
use chain_solana as chain;
use json_rpc::{Batch, BoxFuture, CallResult, RawJson, Request};
use serde_json::{Value, json};
use solana_system_interface::{instruction::SystemInstruction, program::ID as SYSTEM_ID};

const GENESIS: &str = "11111111111111111111111111111111";
const MEMO: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

fn from() -> Address {
    Address::from_bytes([1; 32])
}

fn to() -> Address {
    Address::from_bytes([2; 32])
}

fn signature() -> String {
    solana_signature::Signature::from([7; 64]).to_string()
}

/// A cluster whose finalized slot 10 holds one 42-lamport System transfer.
struct Node;

impl Node {
    fn reply(method: &str) -> Value {
        match method {
            "getGenesisHash" => json!(GENESIS),
            "getSlot" => json!(41),
            "getAccountInfo" => json!({"context": {"slot": 41}, "value": {
                "lamports": 1, "owner": GENESIS, "executable": true,
                "data": ["", "base64"], "space": 0,
            }}),
            "getBlock" => Self::block(),
            other => panic!("unexpected {other}"),
        }
    }

    fn block() -> Value {
        let hash = |byte| solana_hash::Hash::new_from_array([byte; 32]).to_string();
        json!({
            "blockhash": hash(10), "previousBlockhash": hash(9),
            "parentSlot": 9, "blockTime": 100, "blockHeight": 8,
            "transactions": [{
                "transaction": {"signatures": [signature()], "message": Self::message()},
                "meta": {
                    "err": null, "fee": 5000, "innerInstructions": [],
                    "preBalances": [100_000, 0, 1], "postBalances": [94_958, 42, 1],
                },
                "version": "legacy",
            }],
        })
    }

    fn message() -> Value {
        let transfer = bincode::serialize(&SystemInstruction::Transfer { lamports: 42 }).unwrap();
        let keys = [from().to_string(), to().to_string(), SYSTEM_ID.to_string()];
        json!({
            "header": {
                "numRequiredSignatures": 1,
                "numReadonlySignedAccounts": 0,
                "numReadonlyUnsignedAccounts": 1,
            },
            "accountKeys": keys,
            "recentBlockhash": GENESIS,
            "instructions": [{
                "programIdIndex": 2, "accounts": [0, 1],
                "data": bs58::encode(transfer).into_string(),
            }],
        })
    }
}

impl json_rpc::Client for Node {
    fn request<'a>(
        &'a self,
        request: Request,
    ) -> BoxFuture<'a, Result<CallResult, json_rpc::Error>> {
        let call = request.into_call();
        if call.method == "getAccountInfo" {
            assert_eq!(call.params[0], MEMO);
        }
        let reply = RawJson::from_serializable(&Self::reply(&call.method)).map(Ok);
        Box::pin(async move { reply })
    }

    fn batch<'a>(
        &'a self,
        _batch: Batch,
    ) -> BoxFuture<'a, Result<Vec<CallResult>, json_rpc::Error>> {
        panic!("Solana never batches")
    }
}

#[test]
fn raw_block_parses_into_transactions_and_movements() {
    let network = Network::new("localnet", GENESIS.parse().unwrap());
    let client = futures_executor::block_on(RpcClient::connect(Node, &network)).unwrap();
    let raw: RawBlock = futures_executor::block_on(client.block(BlockPosition(10)))
        .unwrap()
        .expect("produced block");
    let block: Block = raw.parse().unwrap();
    assert_eq!(block.reference(), raw.reference());
    assert_eq!(block.reference().height.0, 8);

    let transactions: Vec<&Transaction> = block.transactions().iter().collect();
    assert_eq!(transactions.len(), 1);
    let transaction = transactions[0];
    assert_eq!(transaction.id().to_string(), signature());
    assert!(transaction.succeeded());
    assert_eq!(
        transaction.fee().map(|fee| fee.amount.to_string()),
        Some("5000".to_owned())
    );
    assert_eq!(
        block.transactions().get(transaction.id()),
        Some(transaction)
    );

    let movements = transaction.movements();
    assert_eq!(movements.len(), 1);
    assert_eq!(movements[0].kind, MovementKind::Transfer);
    assert_eq!(movements[0].amount.to_string(), "42");
    assert_eq!(movements[0].from.as_ref(), Some(&from()));
    assert_eq!(movements[0].to.as_ref(), Some(&to()));

    // Chain detail is always one field deep on the transaction.
    assert!(transaction.instructions[0].inner.is_empty());
    assert_eq!(transaction.accounts[1].balance.after.units(), 42);
}
