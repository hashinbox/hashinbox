//! `Chain` over a scripted node: tips, block legs, batched runs, skipped
//! slots and a reorg-shaped refetch.

use base::{
    Address, Asset, BlockHash, Chained as _, Dated as _, Decimal, Leg, Network, NotServed, Reason,
    TransactionState,
};
use std::sync::Arc;

use ::rpc::testing::{Reply, Script};
use chain_solana::{Chain, Client, Config, Factory};
use futures_executor::block_on;
use serde_json::{Value, json};

const MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

fn base58(byte: u8, length: usize) -> String {
    bs58::encode(vec![byte; length]).into_string()
}

fn config() -> Config {
    let genesis_hash = base58(9, 32).parse().unwrap();
    let network = chain_solana::Network::new(genesis_hash, "localnet");
    let mut config = Config::new(network);
    config.tokens = vec![chain_solana::Token {
        mint: MINT.parse().unwrap(),
        decimals: 6,
        ticker: None,
        display_decimals: None,
    }];
    config
}

fn chain(script: &Script) -> Chain {
    let config = config();
    Factory::from(config.clone()).chain(&Client::new(Arc::new(script.clone()), config.native()))
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
        "transaction": { "signatures": [base58(4, 64)], "accountKeys": [{ "pubkey": payer }, { "pubkey": recipient }, { "pubkey": program }] },
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
        "transaction": { "signatures": [base58(5, 64)], "accountKeys": [{ "pubkey": payer }, { "pubkey": recipient }] },
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

/// A real devnet block (slot 520749165) as `transactionDetails: "accounts"`
/// gives it, trimmed to one legacy, one v0 and one v1 transaction.
const DEVNET_BLOCK: &str = include_str!("fixtures/devnet-520749165.json");

#[test]
fn a_block_with_version_1_transactions_is_read() {
    let fixture: Value = serde_json::from_str(DEVNET_BLOCK).unwrap();
    let slot = 520_749_165;
    let script = Script::new([
        Reply::ok("getBlocks", json!([slot])),
        get_block(fixture.clone()).expecting(json!([slot, {
            "commitment": "confirmed",
            "encoding": "json",
            "transactionDetails": "accounts",
            "maxSupportedTransactionVersion": 1,
            "rewards": true,
        }])),
    ]);
    let block = block_on(chain(&script).block(slot)).unwrap().unwrap();
    let sent = fixture["transactions"].as_array().unwrap();
    let ids: Vec<&str> = block
        .transactions
        .0
        .iter()
        .map(|one| one.id.id.as_str())
        .collect();
    let expected: Vec<&str> = sent
        .iter()
        .map(|one| one["transaction"]["signatures"][0].as_str().unwrap())
        .collect();
    assert_eq!(ids, expected, "legacy, v0 and v1, in block order");
    // The v1 transaction only paid its fee.
    let payer = sent[2]["transaction"]["accountKeys"][0]["pubkey"]
        .as_str()
        .unwrap();
    assert_eq!(
        block.transactions.0[2].legs,
        [Leg::Fee {
            address: Address::new(network(), payer),
            asset: Asset::native(network()),
            amount: Decimal::from(5000),
        }]
    );
    script.exhausted();
}

#[test]
fn a_skipped_slot_has_no_block() {
    let script = Script::new([
        Reply::ok("getBlocks", json!([])).expecting(json!([6, 6, { "commitment": "confirmed" }])),
        Reply::ok("getFirstAvailableBlock", json!(0)),
    ]);
    assert_eq!(block_on(chain(&script).block(6)).unwrap(), None);
    script.exhausted();
}

#[test]
fn a_slot_below_the_first_kept_block_is_not_served() {
    let script = Script::new([
        Reply::ok("getBlocks", json!([9])),
        Reply::ok("getFirstAvailableBlock", json!(9)),
    ]);
    let error = block_on(chain(&script).blocks(6, 9)).unwrap_err();
    assert_eq!(NotServed::find(&error), Some(NotServed { position: 6 }));
    let script = Script::new([
        Reply::ok("getBlocks", json!([6])),
        Reply::failure(
            "getBlock",
            -32_001,
            "Block 6 cleaned up, does not exist on node",
        ),
    ]);
    let error = block_on(chain(&script).block(6)).unwrap_err();
    assert_eq!(NotServed::find(&error), Some(NotServed { position: 6 }));
    script.exhausted();
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

#[test]
fn a_chain_over_a_swap_follows_its_replacement() {
    let old = Script::new([]);
    let swap = Arc::new(::rpc::Swap::new(Arc::new(old.clone()), Vec::new()));
    let config = config();
    let chain = Factory::from(config.clone()).chain(&Client::new(swap.clone(), config.native()));
    let tips = |script: &Script, produced: u64| {
        script.push([
            Reply::ok("getSlot", json!(produced)),
            Reply::ok("getSlot", json!(1)),
        ]);
        block_on(chain.tips()).unwrap().produced
    };
    assert_eq!(tips(&old, 10), 10);

    let next = Script::new([]);
    swap.replace(Arc::new(next.clone()), Vec::new());
    assert_eq!(tips(&next, 20), 20);
    old.exhausted();
    next.exhausted();
}

#[test]
fn a_search_by_time_reads_block_times_and_steps_over_skipped_slots() {
    let time = |slot: u64, seconds: u64| {
        Reply::ok("getBlockTime", json!(seconds)).expecting(json!([slot]))
    };
    let script = Script::new([
        Reply::ok("getSlot", json!(3)),
        Reply::ok("getSlot", json!(3)),
        time(3, 300),
        time(0, 100),
        Reply::failure("getBlockTime", -32_007, "Slot 1 was skipped").expecting(json!([1])),
        time(2, 200),
    ]);
    let stamp = block_on(chain(&script).position_at(250)).unwrap().unwrap();
    assert_eq!((stamp.position, stamp.timestamp), (2, 200));
    script.exhausted();
}
