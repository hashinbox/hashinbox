//! `Chain` over a scripted node: tips, block legs, batched runs, skipped
//! slots and a reorg-shaped refetch.

use base::{
    Address, Asset, BlockHash, Chained as _, Decimal, Leg, Network, Reason, TransactionState,
};
use std::sync::Arc;

use chain_solana::{Chain, Client, Config};
use futures_executor::block_on;
use json_rpc::testing::{Reply, Script};
use serde_json::{Value, json};

const MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

fn base58(byte: u8, length: usize) -> String {
    bs58::encode(vec![byte; length]).into_string()
}

fn config() -> Config {
    serde_json::from_value(json!({
        "network": "localnet",
        "genesis_hash": base58(9, 32),
        "endpoints": ["http://127.0.0.1:8899"],
        "tokens": [{ "mint": MINT, "decimals": 6 }],
    }))
    .unwrap()
}

fn chain(script: &Script) -> Chain {
    let config = config();
    config.chain(&Client::new(Arc::new(script.clone()), config.native()))
}

fn network() -> Network {
    Network::from(&config().native())
}

fn address(byte: u8) -> Address {
    Address::new(network(), base58(byte, 32))
}

/// A `getBlock` reply: height `height`, hashed `hash`, on parent `parent`.
fn block(height: u64, hash: u8, parent: (u64, u8), transactions: Value) -> Value {
    json!({
        "blockhash": base58(hash, 32),
        "previousBlockhash": base58(parent.1, 32),
        "parentSlot": parent.0,
        "blockHeight": height,
        "blockTime": 1_700_000_000,
        "transactions": transactions,
        "rewards": [],
    })
}

fn get_block(reply: Value) -> Reply {
    Reply::ok("getBlock", reply)
}

#[test]
fn tips_are_confirmed_produced_and_rooted_final() {
    let script = Script::new([
        Reply::ok("getSlot", json!(100)).expecting(json!([{ "commitment": "confirmed" }])),
        Reply::ok("getSlot", json!(90)).expecting(json!([{ "commitment": "finalized" }])),
    ]);
    let tips = block_on(chain(&script).tips()).unwrap();
    assert_eq!((tips.produced, tips.confirmed), (100, 90));
    script.exhausted();
}

#[test]
fn a_block_becomes_balance_legs_fees_and_rewards() {
    let (payer, recipient, program) = (base58(1, 32), base58(2, 32), base58(3, 32));
    let transfer = json!({
        "transaction": { "signatures": [base58(4, 64)], "message": { "accountKeys": [payer, recipient, program] } },
        "meta": {
            "err": null, "fee": 5000,
            "preBalances": [1_000_000, 0, 1], "postBalances": [994_900, 100, 1],
            "preTokenBalances": [
                { "accountIndex": 1, "mint": MINT, "owner": payer, "uiTokenAmount": { "amount": "50" } },
                { "accountIndex": 2, "mint": base58(8, 32), "owner": payer, "uiTokenAmount": { "amount": "7" } },
            ],
            "postTokenBalances": [
                { "accountIndex": 1, "mint": MINT, "owner": payer, "uiTokenAmount": { "amount": "30" } },
                { "accountIndex": 2, "mint": MINT, "owner": recipient, "uiTokenAmount": { "amount": "20" } },
            ],
        },
    });
    let failed = json!({
        "transaction": { "signatures": [base58(5, 64)], "message": { "accountKeys": [payer, recipient] } },
        "meta": { "err": { "InstructionError": [0, "Custom"] }, "fee": 5000,
                  "preBalances": [994_900, 100], "postBalances": [989_900, 100] },
    });
    let mut reply = block(7, 11, (10, 10), json!([transfer, failed]));
    reply["rewards"] = json!([
        { "pubkey": recipient, "lamports": -3, "postBalance": 97, "rewardType": "Rent" },
        { "pubkey": payer, "lamports": 2500, "postBalance": 1, "rewardType": "Fee" },
    ]);
    let script = Script::new([Reply::ok("getBlocks", json!([11])), get_block(reply)]);

    let block = block_on(chain(&script).block(11)).unwrap().unwrap();

    assert_eq!(block.header.position, 11);
    assert_eq!(block.header.height, 7);
    assert_eq!(block.header.hash, BlockHash(vec![11; 32]));
    let parent = block.header.parent.clone().unwrap();
    assert_eq!(
        (parent.position, parent.hash),
        (10, BlockHash(vec![10; 32]))
    );
    let sol = Asset::native(network());
    let usdc = Asset {
        network: network(),
        id: MINT.to_owned(),
    };
    let [first, second] = block.transactions.0.as_slice() else {
        panic!("two transactions");
    };
    assert_eq!(first.id.id, base58(4, 64));
    assert_eq!(
        (first.state.clone(), first.order),
        (TransactionState::Succeeded, Some(0))
    );
    assert_eq!(
        first.legs,
        vec![
            Leg::Fee {
                address: address(1),
                asset: sol.clone(),
                amount: Decimal::from(5000)
            },
            Leg::Spent {
                address: address(1),
                asset: sol.clone(),
                amount: Decimal::from(100),
                coin: None
            },
            Leg::Received {
                address: address(2),
                asset: sol.clone(),
                amount: Decimal::from(100),
                coin: None
            },
            Leg::Spent {
                address: address(1),
                asset: usdc.clone(),
                amount: Decimal::from(20),
                coin: None
            },
            Leg::Received {
                address: address(2),
                asset: usdc,
                amount: Decimal::from(20),
                coin: None
            },
        ],
        "the unconfigured mint is left out"
    );
    assert_eq!(
        second.state,
        TransactionState::Reverted(Some(Reason::Reverted))
    );
    assert_eq!(
        second.legs,
        vec![Leg::Fee {
            address: address(1),
            asset: sol.clone(),
            amount: Decimal::from(5000)
        }]
    );
    assert_eq!(block.rewards.len(), 2);
    assert_eq!(block.rewards[0].amount, "-3".parse().unwrap());
    assert_eq!(
        (block.rewards[1].index, &block.rewards[1].address),
        (1, &address(1))
    );
    script.exhausted();
}

#[test]
fn a_skipped_slot_has_no_block() {
    let script =
        Script::new([Reply::ok("getBlocks", json!([]))
            .expecting(json!([6, 6, { "commitment": "confirmed" }]))]);
    assert_eq!(block_on(chain(&script).block(6)).unwrap(), None);
}

#[test]
fn blocks_lists_produced_slots_then_batches_them_linked() {
    let script = Script::new([
        Reply::ok("getBlocks", json!([5, 7]))
            .expecting(json!([5, 8, { "commitment": "confirmed" }])),
        get_block(block(3, 5, (4, 4), json!([]))),
        get_block(block(4, 7, (5, 5), json!([]))),
    ]);
    let run = block_on(chain(&script).blocks(5, 8)).unwrap();
    let positions: Vec<u64> = run.iter().map(|block| block.header.position).collect();
    assert_eq!(positions, [5, 7]);
    script.exhausted();
}

#[test]
fn a_refetch_on_another_branch_does_not_link() {
    let script = Script::new([
        Reply::ok("getBlocks", json!([5, 7])),
        get_block(block(3, 5, (4, 4), json!([]))),
        get_block(block(4, 7, (6, 6), json!([]))),
    ]);
    let error = block_on(chain(&script).blocks(5, 8)).unwrap_err();
    assert!(error.to_string().contains("does not link"), "{error}");
}

#[test]
fn a_listed_slot_that_turns_out_skipped_asks_for_a_retry() {
    let script = Script::new([
        Reply::ok("getBlocks", json!([5])),
        Reply::failure("getBlock", -32_007, "skipped"),
    ]);
    assert!(block_on(chain(&script).blocks(5, 5)).is_err());
}
