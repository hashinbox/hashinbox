//! Answers rendered from recorded replies: the recorded block, transaction,
//! receipt and log objects, every field the mock knows put in its place and
//! the rest kept as recorded.

use alloy_primitives::{Address, B256, U256, hex};
use serde_json::{Value, json};

use super::{
    TRANSFER,
    model::{CHAIN_ID, Model, Tx},
    recording,
};

type Block = base::testing::Block<Model>;

fn quantity(value: impl std::fmt::LowerHex) -> Value {
    json!(format!("{value:#x}"))
}

fn hash(value: B256) -> Value {
    json!(hex::encode_prefixed(value))
}

fn address(value: Address) -> Value {
    json!(hex::encode_prefixed(value))
}

fn word(value: impl AsRef<[u8]>) -> Value {
    json!(format!("0x{:0>64}", hex::encode(value)))
}

/// Block `block`, its transactions as objects (`full`) or hashes.
pub(super) fn block(model: &Model, block: &Block, full: bool) -> Value {
    let mut out = recording("eth_getBlockByNumber.full").result().clone();
    out["number"] = quantity(block.height);
    out["hash"] = hash(block.seal.hash);
    out["parentHash"] = hash(block.seal.parent);
    out["timestamp"] = quantity(block.time);
    out["baseFeePerGas"] = quantity(model.base_fee);
    out["gasUsed"] = quantity(block.txs.iter().map(|(_, o)| o.gas_used).sum::<u64>());
    out["transactions"] = if full {
        (0..block.txs.len()).map(|at| mined(block, at)).collect()
    } else {
        block.txs.iter().map(|(tx, _)| hash(tx.hash)).collect()
    };
    out["withdrawals"] = json!([]);
    out
}

/// Transaction `at` of `block`.
pub(super) fn mined(block: &Block, at: usize) -> Value {
    let (tx, outcome) = &block.txs[at];
    let mut out = transaction(tx);
    out["blockHash"] = hash(block.seal.hash);
    out["blockNumber"] = quantity(block.height);
    out["blockTimestamp"] = quantity(block.time);
    out["transactionIndex"] = quantity(at);
    out["gasPrice"] = quantity(outcome.price);
    out
}

/// A transaction in the mempool: no block yet.
pub(super) fn pending(tx: &Tx) -> Value {
    let mut out = transaction(tx);
    for field in [
        "blockHash",
        "blockNumber",
        "blockTimestamp",
        "transactionIndex",
    ] {
        out[field] = Value::Null;
    }
    out["gasPrice"] = quantity(tx.max_fee);
    out
}

fn transaction(tx: &Tx) -> Value {
    let full = recording("eth_getBlockByNumber.full");
    let mut out = full.result()["transactions"][0].clone();
    let (r, s, parity) = tx
        .signature
        .map_or((U256::ZERO, U256::ZERO, false), |signature| {
            (signature.r(), signature.s(), signature.v())
        });
    out["chainId"] = quantity(CHAIN_ID);
    out["from"] = address(tx.from);
    out["gas"] = quantity(tx.gas);
    out["hash"] = hash(tx.hash);
    out["input"] = json!(hex::encode_prefixed(&tx.input));
    out["maxFeePerGas"] = quantity(tx.max_fee);
    out["maxPriorityFeePerGas"] = quantity(tx.priority);
    out["nonce"] = quantity(tx.nonce);
    out["to"] = address(tx.to);
    out["value"] = quantity(tx.value);
    out["r"] = quantity(r);
    out["s"] = quantity(s);
    out["v"] = quantity(u8::from(parity));
    out["yParity"] = quantity(u8::from(parity));
    out
}

/// Every receipt of `block`.
pub(super) fn receipts(block: &Block) -> Value {
    (0..block.txs.len()).map(|at| receipt(block, at)).collect()
}

/// The receipt of transaction `at` of `block`.
pub(super) fn receipt(block: &Block, at: usize) -> Value {
    let (tx, outcome) = &block.txs[at];
    let mut out = recording("eth_getBlockReceipts").result()[0].clone();
    let before = &block.txs[..at];
    let cumulative: u64 = block.txs[..=at].iter().map(|(_, o)| o.gas_used).sum();
    let first_log: usize = before.iter().map(|(_, o)| o.logs.len()).sum();
    out["blockHash"] = hash(block.seal.hash);
    out["blockNumber"] = quantity(block.height);
    out["contractAddress"] = Value::Null;
    out["cumulativeGasUsed"] = quantity(cumulative);
    out["effectiveGasPrice"] = quantity(outcome.price);
    out["from"] = address(tx.from);
    out["gasUsed"] = quantity(outcome.gas_used);
    out["logs"] = outcome
        .logs
        .iter()
        .enumerate()
        .map(|(index, moved)| log(block, at, first_log + index, *moved))
        .collect();
    out["status"] = quantity(u8::from(outcome.status));
    out["to"] = address(tx.to);
    out["transactionHash"] = hash(tx.hash);
    out["transactionIndex"] = quantity(at);
    out
}

/// Every log of `block`, as `eth_getLogs` answers them.
pub(super) fn logs(block: &Block) -> Vec<Value> {
    let template = recording("eth_getLogs");
    let mut logs = Vec::new();
    for (at, (_, outcome)) in block.txs.iter().enumerate() {
        for moved in &outcome.logs {
            let mut log = log(block, at, logs.len(), *moved);
            retain(&mut log, &template.result()[0]);
            logs.push(log);
        }
    }
    logs
}

/// Keeps only the fields `template` has (a receipt's log and an
/// `eth_getLogs` log may differ).
fn retain(value: &mut Value, template: &Value) {
    if let (Value::Object(fields), Value::Object(kept)) = (value, template) {
        fields.retain(|field, _| kept.contains_key(field));
    }
}

/// A `Transfer` log: `(contract, from, to, amount)`.
fn log(
    block: &Block,
    at: usize,
    index: usize,
    (contract, from, to, amount): (Address, Address, Address, U256),
) -> Value {
    let receipts = recording("eth_getBlockReceipts");
    let mut out = receipts.result()[1]["logs"][0].clone();
    out["address"] = address(contract);
    out["blockHash"] = hash(block.seal.hash);
    out["blockNumber"] = quantity(block.height);
    out["blockTimestamp"] = quantity(block.time);
    out["data"] = word(amount.to_be_bytes::<32>());
    out["logIndex"] = quantity(index);
    out["removed"] = json!(false);
    out["topics"] = json!([TRANSFER, word(from), word(to)]);
    out["transactionHash"] = hash(block.txs[at].0.hash);
    out["transactionIndex"] = quantity(at);
    out
}
