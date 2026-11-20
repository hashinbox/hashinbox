//! `Chain` over a scripted node: tips, block legs, batched runs, skipped
//! slots and a reorg-shaped refetch.

use base::{
    Address, Asset, Balances as _, BlockHash, Chained as _, Confirmed, Confirmer as _, Dated as _,
    Decimal, Holding, Leg, Line, Network, NetworkConfig, NotServed, Reason, TransactionId,
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
fn the_tip_is_the_confirmed_slot_and_confirmed_is_confirmations_below() {
    let script = Script::new([
        Reply::ok("getSlot", json!(100)).expecting(json!([{ "commitment": "confirmed" }]))
    ]);
    let chain = chain(&script);
    let tips = block_on(chain.tips()).unwrap();
    assert_eq!(tips.position, 100);
    assert_eq!(chain.confirmations(), 32);
    assert_eq!(tips.confirmed(chain.confirmations()), 68);
    script.exhausted();
}

/// The line is the tip less `read_depth` (default 1,000; `settings` may set
/// it), not the block after the confirmed one, so confirmed blocks are read
/// and only an index further behind settles.
#[test]
fn the_line_is_the_read_depth_below_the_tip_not_after_the_final_block() {
    let script = Script::new([Reply::ok("getSlot", json!(5_000))]);
    let tips = block_on(chain(&script).tips()).unwrap();
    assert_eq!(tips.position, 5_000);
    assert_eq!(chain(&script).line(), Line::Depth(chain_solana::READ_DEPTH));
    assert_eq!(chain(&script).line().at(tips, 32), 4_000);
    script.exhausted();

    let stored = NetworkConfig {
        network: network(),
        chain_id: Some(base58(9, 32)),
        testnet: true,
        block_time: std::time::Duration::from_millis(400),
        confirmations: 32,
        max_depth: None,
        explorer_address: None,
        explorer_transaction: None,
        settings: json!({"read_depth": 50}),
        assets: Vec::new(),
    };
    let config = Config::try_from(&stored).unwrap();
    let rpc = Client::new(Arc::new(script.clone()), config.native());
    assert_eq!(Factory::from(config).chain(&rpc).line(), Line::Depth(50));
    // A line above the confirmed slot is refused: `read_depth` is at least
    // `confirmations`.
    let shallow = NetworkConfig {
        settings: json!({"read_depth": 31}),
        ..stored
    };
    let error = Config::try_from(&shallow).unwrap().validate().unwrap_err();
    assert!(
        error.to_string().contains("at least its confirmations"),
        "{error}"
    );
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
        script.push([Reply::ok("getSlot", json!(produced))]);
        block_on(chain.tips()).unwrap().position
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
        time(3, 300),
        time(0, 100),
        Reply::failure("getBlockTime", -32_007, "Slot 1 was skipped").expecting(json!([1])),
        time(2, 200),
    ]);
    let stamp = block_on(chain(&script).position_at(250)).unwrap().unwrap();
    assert_eq!((stamp.position, stamp.timestamp), (2, 200));
    script.exhausted();
}

/// A reply read at `slot`, as a node answers it.
fn at_slot(slot: u64, value: Value) -> Value {
    json!({ "context": { "apiVersion": "2.2.7", "slot": slot }, "value": value })
}

/// A `jsonParsed` SPL Token account of `owner` holding `amount` of [`MINT`].
fn token_account(account: u8, owner: &str, amount: u64) -> Value {
    let ui = amount as f64 / 1e6;
    json!({
        "pubkey": base58(account, 32),
        "account": {
            "data": {
                "parsed": {
                    "info": {
                        "isNative": false, "mint": MINT, "owner": owner, "state": "initialized",
                        "tokenAmount": {
                            "amount": amount.to_string(), "decimals": 6,
                            "uiAmount": ui, "uiAmountString": ui.to_string(),
                        },
                    },
                    "type": "account",
                },
                "program": "spl-token",
                "space": 165,
            },
            "executable": false,
            "lamports": 2_039_280,
            "owner": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "rentEpoch": u64::MAX,
            "space": 165,
        },
    })
}

#[test]
fn a_balance_is_read_finalized_at_the_slot_the_node_answers_at() {
    let owner = base58(1, 32);
    let usdc = Asset {
        network: network(),
        id: MINT.to_owned(),
    };
    let script = Script::new([
        Reply::ok("getBalance", at_slot(105, json!(5_000_000_000_u64))).expecting(json!([
            owner,
            { "commitment": "finalized", "minContextSlot": 100 },
        ])),
        Reply::ok(
            "getTokenAccountsByOwner",
            at_slot(
                106,
                json!([
                    token_account(20, &owner, 1_500_000),
                    token_account(21, &owner, 250_000)
                ]),
            ),
        )
        .expecting(json!([
            owner,
            { "mint": MINT },
            { "commitment": "finalized", "minContextSlot": 100, "encoding": "jsonParsed" },
        ])),
        Reply::ok("getTokenAccountsByOwner", at_slot(107, json!([]))),
    ]);
    let chain = chain(&script);
    let sol = Asset::native(network());

    let native = block_on(chain.balance(&address(1), &sol, 100)).unwrap();
    let token = block_on(chain.balance(&address(1), &usdc, 100)).unwrap();
    let none = block_on(chain.balance(&address(1), &usdc, 100)).unwrap();

    assert_eq!(
        native,
        Some(Holding {
            position: 105,
            amount: Decimal::from(5_000_000_000_u64)
        })
    );
    assert_eq!(
        token,
        Some(Holding {
            position: 106,
            amount: Decimal::from(1_750_000)
        })
    );
    assert_eq!(
        none,
        Some(Holding {
            position: 107,
            amount: Decimal::zero()
        }),
        "an owner without an account of the mint holds 0"
    );
    script.exhausted();
}

#[test]
fn a_balance_at_a_slot_not_finalized_yet_is_not_answerable() {
    let script = Script::new([Reply::failure(
        "getBalance",
        -32_016,
        "Minimum context slot has not been reached",
    )]);
    let chain = chain(&script);
    let sol = Asset::native(network());

    let behind = block_on(chain.balance(&address(1), &sol, 100)).unwrap();

    assert_eq!(behind, None, "the node is not at 100 yet");
    script.exhausted();
}

#[test]
fn a_transaction_is_confirmed_by_its_finalized_status_at_the_finalized_slot() {
    let script = Script::new([]);
    let chain = chain(&script);
    let signature = base58(7, 64);
    let id = TransactionId::new(network(), signature.clone());
    // The tip 122: confirmed through 90 (32 confirmations).
    let tips = || [Reply::ok("getSlot", json!(122))];
    let status = |status: Value| {
        Reply::ok(
            "getSignatureStatuses",
            json!({ "context": { "slot": 100 }, "value": [status] }),
        )
        .expecting(json!([[signature], { "searchTransactionHistory": true }]))
    };
    let finalized = |slot: u64, err: Value| json!({ "slot": slot, "confirmationStatus": "finalized", "err": err });
    let failed = || json!({ "InstructionError": [0, "Custom"] });
    // Finalized at 80, succeeded or failed; finalized past the slot read
    // (failed); only confirmed; unknown.
    for (answer, found, reverted) in [
        (finalized(80, Value::Null), Some(80), false),
        (finalized(80, failed()), Some(80), true),
        (finalized(95, failed()), None, false),
        (
            json!({ "slot": 85, "confirmationStatus": "confirmed", "err": null }),
            None,
            false,
        ),
        (Value::Null, None, false),
    ] {
        script.push(tips());
        script.push([status(answer)]);
        let confirmed = block_on(chain.confirm(&id, 90)).unwrap();
        assert_eq!(
            confirmed,
            Some(Confirmed {
                position: 90,
                found,
                reverted
            })
        );
    }
    // Not finalized through 91 yet: no status asked.
    script.push(tips());
    assert_eq!(block_on(chain.confirm(&id, 91)).unwrap(), None);
    script.exhausted();
}
