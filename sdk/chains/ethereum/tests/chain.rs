//! `Chain` over a scripted node: tips and finality, block -> legs, batched
//! fetches, linking, a reorg-shaped refetch, and the receipt fallback.

use std::sync::Arc;

use ::rpc::testing::{Reply, Script};
use base::{Asset, Chained as _, Decimal, Leg, Reason, Reward, Tips, TransactionState};
use chain_ethereum::{Chain, Client, Config, Network, Token};
use futures_executor::block_on;
use num_bigint::BigUint;
use serde_json::{Value, json};

const TOKEN: &str = "0x00000000000000000000000000000000000000c0";
const OTHER_TOKEN: &str = "0x00000000000000000000000000000000000000c1";
/// The block every transaction fixture sits in.
const HEIGHT: u64 = 5;
const OWN: u64 = 0x55;
const TRANSFER: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

fn hash(byte: u64) -> String {
    format!("0x{byte:064x}")
}

fn account(byte: u64) -> String {
    format!("0x{byte:040x}")
}

fn topic(address: &str) -> String {
    format!("0x{:0>64}", &address[2..])
}

fn network() -> base::Network {
    base::Network::new(chain_ethereum::CHAIN_NAME, "mainnet")
}

fn atomic(amount: u64) -> Decimal {
    Decimal::from_atomic(BigUint::from(amount), 0)
}

fn address(byte: u64) -> base::Address {
    base::Address::new(network(), account(byte))
}

fn block(
    height: u64,
    own: u64,
    parent: u64,
    transactions: Vec<Value>,
    withdrawals: Value,
) -> Value {
    json!({
        "hash": hash(own), "parentHash": hash(parent), "number": format!("{height:#x}"),
        "sha3Uncles": hash(0), "miner": account(0), "stateRoot": hash(0),
        "transactionsRoot": hash(0), "receiptsRoot": hash(0),
        "logsBloom": format!("0x{}", "0".repeat(512)), "difficulty": "0x0",
        "gasLimit": "0x1c9c380", "gasUsed": "0x0", "timestamp": "0x64",
        "extraData": "0x", "mixHash": hash(0), "nonce": "0x0000000000000000",
        "baseFeePerGas": "0x1", "uncles": [], "transactions": transactions,
        "withdrawals": withdrawals,
    })
}

/// Transaction `index` of block [`HEIGHT`]: `from` sends `value` to `to`.
fn transaction(index: u64, from: u64, to: &str, value: u64) -> Value {
    json!({
        "hash": hash(0x100 + index), "from": account(from), "to": to,
        "value": format!("{value:#x}"), "transactionIndex": format!("{index:#x}"),
        "blockHash": hash(OWN), "blockNumber": format!("{HEIGHT:#x}"),
    })
}

fn receipt(index: u64, from: u64, to: &str, status: u64, logs: Vec<Value>) -> Value {
    json!({
        "transactionHash": hash(0x100 + index), "transactionIndex": format!("{index:#x}"),
        "blockHash": hash(OWN), "blockNumber": format!("{HEIGHT:#x}"),
        "from": account(from), "to": to, "status": format!("{status:#x}"),
        "gasUsed": "0x5208", "effectiveGasPrice": "0x2", "contractAddress": null, "logs": logs,
    })
}

/// An ERC-20 `Transfer` of `amount` from `from` to `to`, log `index`.
fn log(transaction: u64, contract: &str, from: &str, to: &str, amount: u64, index: u64) -> Value {
    json!({
        "address": contract, "topics": [TRANSFER, topic(from), topic(to)],
        "data": format!("0x{amount:064x}"), "logIndex": format!("{index:#x}"),
        "transactionHash": hash(0x100 + transaction), "transactionIndex": format!("{transaction:#x}"),
        "blockHash": hash(OWN), "blockNumber": format!("{HEIGHT:#x}"), "removed": false,
    })
}

fn connect(script: &Script, config: &Config) -> Chain {
    config.chain(&Client::new(Arc::new(script.clone()), config.native()))
}

fn config() -> Config {
    let mut config = Config::new(Network::new(1, "mainnet"), Vec::new());
    config.tokens = vec![Token {
        contract: TOKEN.parse().unwrap(),
        decimals: 6,
        ticker: None,
    }];
    config
}

#[test]
fn verify_checks_the_chain_id() {
    let script = Script::new([]);
    let client = Client::new(Arc::new(script.clone()), Network::new(1, "mainnet"));
    script.push([Reply::ok("eth_chainId", json!("0x1"))]);
    block_on(client.verify()).unwrap();
    script.push([Reply::ok("eth_chainId", json!("0x2"))]);
    assert!(block_on(client.verify()).is_err());
    script.exhausted();
}

#[test]
fn tips_are_the_produced_and_finalized_blocks() {
    let script = Script::new([]);
    let chain = connect(&script, &config());
    script.push([
        Reply::ok("eth_blockNumber", json!("0x10")),
        Reply::ok("eth_getBlockByNumber", json!({"number": "0xc"}))
            .expecting(json!(["finalized", false])),
    ]);
    let tips = block_on(chain.tips()).unwrap();
    assert_eq!(
        tips,
        Tips {
            produced: 16,
            confirmed: 12
        }
    );

    script.push([
        Reply::ok("eth_blockNumber", json!("0x10")),
        Reply::ok("eth_getBlockByNumber", Value::Null),
    ]);
    assert!(block_on(chain.tips()).is_err(), "no finalized block yet");
    script.exhausted();
}

#[test]
fn a_block_becomes_double_entry_legs() {
    let script = Script::new([]);
    let chain = connect(&script, &config());
    let b = account(0xb);
    let transactions = vec![
        transaction(0, 0xa, &b, 1000),
        transaction(1, 0xc, TOKEN, 0),
        transaction(2, 0xe, &b, 9),
    ];
    let logs = vec![
        log(1, TOKEN, &account(0xc), &account(0xd), 7, 0),
        log(1, TOKEN, &account(0), &account(0xd), 3, 1),
        log(1, OTHER_TOKEN, &account(0xc), &account(0xd), 5, 2),
        log(1, TOKEN, &account(0xc), &account(0), 2, 3),
    ];
    let receipts = json!([
        receipt(0, 0xa, &b, 1, vec![]),
        receipt(1, 0xc, TOKEN, 1, logs),
        receipt(2, 0xe, &b, 0, vec![]),
    ]);
    let withdrawals = json!([{"index": "0x9", "validatorIndex": "0x1", "address": account(0xf), "amount": "0x3"}]);
    script.push([
        Reply::ok(
            "eth_getBlockByNumber",
            block(HEIGHT, OWN, 0x44, transactions, withdrawals),
        )
        .expecting(json!(["0x5", true])),
        Reply::ok("eth_getBlockReceipts", receipts).expecting(json!([hash(OWN)])),
    ]);
    let block = block_on(chain.block(HEIGHT)).unwrap().unwrap();
    script.exhausted();

    let native = Asset::native(network());
    let token = Asset {
        network: network(),
        id: TOKEN.to_owned(),
    };
    let fee = |payer| Leg::Fee {
        address: address(payer),
        asset: native.clone(),
        amount: atomic(42_000),
    };
    assert_eq!(block.header.position, 5);
    assert_eq!(
        block.header.parent.as_ref().unwrap().hash.0,
        hex(&hash(0x44))
    );
    let [ether, tokens, reverted] = block.transactions.0.as_slice() else {
        panic!("three transactions");
    };
    assert_eq!(ether.state, TransactionState::Succeeded);
    assert_eq!(ether.order, Some(0));
    assert_eq!(
        ether.legs,
        vec![
            Leg::Spent {
                address: address(0xa),
                asset: native.clone(),
                amount: atomic(1000),
                coin: None
            },
            Leg::Received {
                address: base::Address::new(network(), b.clone()),
                asset: native.clone(),
                amount: atomic(1000),
                coin: None
            },
            fee(0xa),
        ]
    );
    assert_eq!(
        tokens.legs,
        vec![
            Leg::Spent {
                address: address(0xc),
                asset: token.clone(),
                amount: atomic(7),
                coin: None
            },
            Leg::Received {
                address: address(0xd),
                asset: token.clone(),
                amount: atomic(7),
                coin: None
            },
            Leg::Minted {
                address: address(0xd),
                asset: token.clone(),
                amount: atomic(3)
            },
            // The unlisted token's transfer has no leg.
            Leg::Burned {
                address: address(0xc),
                asset: token,
                amount: atomic(2)
            },
            fee(0xc),
        ]
    );
    assert_eq!(
        reverted.state,
        TransactionState::Reverted(Some(Reason::Reverted))
    );
    assert_eq!(reverted.legs, vec![fee(0xe)]);
    assert_eq!(
        block.rewards,
        vec![Reward {
            index: 9,
            address: address(0xf),
            asset: native,
            amount: atomic(3_000_000_000),
        }]
    );
}

#[test]
fn blocks_are_fetched_in_batches_and_must_link() {
    let script = Script::new([]);
    let chain = connect(&script, &config());
    script.push([
        Reply::ok(
            "eth_getBlockByNumber",
            block(5, 0x55, 0x44, vec![], json!([])),
        ),
        Reply::ok(
            "eth_getBlockByNumber",
            block(6, 0x66, 0x55, vec![], json!([])),
        ),
    ]);
    let run = block_on(chain.blocks(5, 6)).unwrap();
    assert_eq!(run.len(), 2);
    let kept = run.last().unwrap().clone();

    // The chain reorganised block 6 away: the refetched run no longer holds it.
    script.push([Reply::ok(
        "eth_getBlockByNumber",
        block(6, 0x67, 0x55, vec![], json!([])),
    )]);
    let refetched = block_on(chain.blocks(6, 6)).unwrap();
    assert!(!refetched.contains(&kept));

    // A block that names another parent does not link.
    script.push([
        Reply::ok(
            "eth_getBlockByNumber",
            block(5, 0x55, 0x44, vec![], json!([])),
        ),
        Reply::ok(
            "eth_getBlockByNumber",
            block(6, 0x66, 0x99, vec![], json!([])),
        ),
    ]);
    let error = block_on(chain.blocks(5, 6)).unwrap_err();
    assert!(format!("{error:#}").contains("does not link"), "{error:#}");

    // A block the node does not hold yet is an error, never a skipped height.
    script.push([Reply::ok("eth_getBlockByNumber", Value::Null)]);
    assert!(block_on(chain.block(7)).is_err());
    script.exhausted();
}

#[test]
fn receipts_fall_back_to_one_call_per_transaction() {
    let script = Script::new([]);
    let chain = connect(&script, &config());
    let b = account(0xb);
    let head = || block(5, 0x55, 0x44, vec![transaction(0, 0xa, &b, 1)], json!([]));
    script.push([
        Reply::ok("eth_getBlockByNumber", head()),
        Reply::failure("eth_getBlockReceipts", -32_601, "the method does not exist"),
        Reply::ok("eth_getTransactionReceipt", receipt(0, 0xa, &b, 1, vec![]))
            .expecting(json!([hash(0x100)])),
        // Remembered: the next block asks per transaction at once.
        Reply::ok("eth_getBlockByNumber", head()),
        Reply::ok("eth_getTransactionReceipt", receipt(0, 0xa, &b, 1, vec![])),
    ]);
    assert_eq!(
        block_on(chain.block(5))
            .unwrap()
            .unwrap()
            .transactions
            .0
            .len(),
        1
    );
    assert_eq!(
        block_on(chain.block(5))
            .unwrap()
            .unwrap()
            .transactions
            .0
            .len(),
        1
    );
    script.exhausted();
}

fn hex(text: &str) -> Vec<u8> {
    alloy_primitives::hex::decode(text).unwrap()
}

#[test]
fn a_chain_over_a_swap_follows_its_replacement() {
    let old = Script::new([]);
    let swap = Arc::new(::rpc::Swap::new(Arc::new(old.clone()), Vec::new()));
    let config = config();
    let chain = config.chain(&Client::new(swap.clone(), config.native()));
    let tips = |script: &Script, produced: &str| {
        script.push([
            Reply::ok("eth_blockNumber", json!(produced)),
            Reply::ok("eth_getBlockByNumber", json!({"number": "0x1"})),
        ]);
        block_on(chain.tips()).unwrap().produced
    };
    assert_eq!(tips(&old, "0xa"), 10);

    let next = Script::new([]);
    swap.replace(Arc::new(next.clone()), Vec::new());
    assert_eq!(tips(&next, "0x14"), 20);
    old.exhausted();
    next.exhausted();
}
