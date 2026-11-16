//! `Chain` over a scripted node: tips and finality, block -> legs, batched
//! fetches, linking, a reorg-shaped refetch, and the receipt fallback.

use std::sync::Arc;

use ::rpc::testing::{Faults, Reply, Script};
use base::{
    Asset, Balances as _, Chained as _, Dated as _, Decimal, Holding, InfoClient, Leg, NotServed,
    Reason, Reward, ServiceFactory as _, Tips, TransactionState,
};
use chain_ethereum::{Chain, Client, Config, Factory, Info, Network, Token};
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

/// The `eth_getLogs` filter of [`TOKEN`]'s `Transfer`s over `from..=to`.
fn filter(from: &str, to: &str) -> Value {
    json!([{"fromBlock": from, "toBlock": to, "address": [TOKEN], "topics": [TRANSFER]}])
}

/// No token logs in the range.
fn no_logs() -> Reply {
    Reply::ok("eth_getLogs", json!([]))
}

fn connect(script: &Script, config: &Config) -> Chain {
    Factory::from(config.clone()).chain(&Client::new(Arc::new(script.clone()), config.native()))
}

fn config() -> Config {
    let mut config = Config::new(Network::new(1, "mainnet"));
    config.tokens = vec![Token {
        contract: TOKEN.parse().unwrap(),
        decimals: 6,
        ticker: None,
        display_decimals: None,
    }];
    config
}

#[test]
fn info_reports_the_chain_id_and_tip() {
    let script = Script::new([]);
    let info = Info::new(Arc::new(script.clone()));
    script.push([
        Reply::ok("eth_chainId", json!("0x1")),
        Reply::ok("eth_blockNumber", json!("0x10")),
        Reply::ok("eth_chainId", json!("0xaa36a7")),
        Reply::ok("eth_blockNumber", json!("0x11")),
    ]);
    let node = block_on(info.info()).unwrap();
    assert_eq!((node.chain_id.as_str(), node.tip), ("1", 16));
    let other = block_on(info.info()).unwrap();
    assert_eq!((other.chain_id.as_str(), other.tip), ("11155111", 17));
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
    // Token legs come from `eth_getLogs`, not from the receipts.
    let receipts = json!([
        receipt(0, 0xa, &b, 1, vec![]),
        receipt(1, 0xc, TOKEN, 1, vec![]),
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
        Reply::ok("eth_getLogs", json!(logs)).expecting(filter("0x5", "0x5")),
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
        no_logs(),
    ]);
    let run = block_on(chain.blocks(5, 6)).unwrap();
    assert_eq!(run.len(), 2);
    let kept = run.last().unwrap().clone();

    // The chain reorganised block 6 away: the refetched run no longer holds it.
    script.push([
        Reply::ok(
            "eth_getBlockByNumber",
            block(6, 0x67, 0x55, vec![], json!([])),
        ),
        no_logs(),
    ]);
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
        no_logs(),
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
        no_logs(),
        // Remembered: the next block asks per transaction at once.
        Reply::ok("eth_getBlockByNumber", head()),
        Reply::ok("eth_getTransactionReceipt", receipt(0, 0xa, &b, 1, vec![])),
        no_logs(),
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

#[test]
fn a_refused_log_range_is_halved_and_logs_must_be_of_the_fetched_blocks() {
    let script = Script::new([]);
    let chain = connect(&script, &config());
    let blocks = |sixth: u64| {
        [
            Reply::ok(
                "eth_getBlockByNumber",
                block(5, 0x55, 0x44, vec![], json!([])),
            ),
            Reply::ok(
                "eth_getBlockByNumber",
                block(
                    6,
                    sixth,
                    0x55,
                    vec![transaction(0, 0xc, TOKEN, 0)],
                    json!([]),
                ),
            ),
            Reply::ok(
                "eth_getBlockReceipts",
                json!([receipt(0, 0xc, TOKEN, 1, vec![])]),
            )
            .expecting(json!([hash(sixth)])),
        ]
    };
    let mut moved = log(0, TOKEN, &account(0xc), &account(0xd), 7, 0);
    moved["blockNumber"] = json!("0x6");
    moved["blockHash"] = json!(hash(0x66));

    // Too wide a range for the node (Alchemy's answer): one block at a time.
    script.push(blocks(0x66));
    script.push([
        Reply::failure(
            "eth_getLogs",
            -32_602,
            "Log response size exceeded. You can make eth_getLogs requests with up to a 2K block range and no limit on the response size, or you can request any block range with a cap of 10K logs in the response.",
        )
        .expecting(filter("0x5", "0x6")),
        no_logs().expecting(filter("0x5", "0x5")),
        Reply::ok("eth_getLogs", json!([moved])).expecting(filter("0x6", "0x6")),
    ]);
    let run = block_on(chain.blocks(5, 6)).unwrap();
    let token = Asset {
        network: network(),
        id: TOKEN.to_owned(),
    };
    assert_eq!(
        run[1].transactions.0[0].legs[..2],
        [
            Leg::Spent {
                address: address(0xc),
                asset: token.clone(),
                amount: atomic(7),
                coin: None
            },
            Leg::Received {
                address: address(0xd),
                asset: token,
                amount: atomic(7),
                coin: None
            },
        ]
    );

    // An answer over the response bound is halved the same way.
    script.push(blocks(0x66));
    script.push([
        Reply::result(
            "eth_getLogs",
            Err(::rpc::Error::Transport {
                kind: ::rpc::ErrorKind::ResponseTooLarge,
                message: "JSON-RPC response exceeded its configured limit".to_owned(),
            }),
        ),
        no_logs().expecting(filter("0x5", "0x5")),
        no_logs().expecting(filter("0x6", "0x6")),
    ]);
    assert_eq!(block_on(chain.blocks(5, 6)).unwrap().len(), 2);

    // Logs of a block the chain replaced since: retry.
    script.push(blocks(0x67));
    script.push([Reply::ok("eth_getLogs", json!([moved]))]);
    let error = block_on(chain.blocks(5, 6)).unwrap_err();
    assert!(format!("{error:#}").contains("another block"), "{error:#}");

    // One block the node still refuses is an error.
    script.push(blocks(0x66).into_iter().skip(1));
    script.push([Reply::failure("eth_getLogs", -32_602, "too large")]);
    assert!(block_on(chain.blocks(6, 6)).is_err());

    // The logs' node trails the blocks' one (one URL, several nodes): the
    // run ends before the block it has not reached, without an error.
    let ahead = |head: &str| {
        Reply::failure(
            "eth_getLogs",
            -32_602,
            &format!("block range extends beyond current head block: requested 6, head {head}"),
        )
    };
    script.push(blocks(0x66));
    script.push([
        ahead("5").expecting(filter("0x5", "0x6")),
        no_logs().expecting(filter("0x5", "0x5")),
        ahead("5").expecting(filter("0x6", "0x6")),
    ]);
    let run = block_on(chain.blocks(5, 6)).unwrap();
    assert_eq!(run.len(), 1, "block 6 waits for the next step");
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
    let chain = Factory::from(config.clone()).chain(&Client::new(swap.clone(), config.native()));
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

#[test]
fn a_block_time_is_a_header_read_and_expired_history_says_so() {
    let script = Script::new([Reply::ok(
        "eth_getBlockByNumber",
        json!({"number": "0x5", "timestamp": "0x64"}),
    )
    .expecting(json!(["0x5", false]))]);
    let chain = connect(&script, &config());
    assert_eq!(block_on(chain.time(5)).unwrap(), Some(100));
    script.push([Reply::failure(
        "eth_getBlockByNumber",
        4444,
        "pruned history unavailable",
    )]);
    let error = block_on(chain.time(3)).unwrap_err();
    assert_eq!(NotServed::find(&error), Some(NotServed { position: 3 }));
    script.push([Reply::failure(
        "eth_getBlockByNumber",
        4444,
        "pruned history unavailable",
    )]);
    let error = block_on(chain.block(3)).unwrap_err();
    assert_eq!(NotServed::find(&error), Some(NotServed { position: 3 }));
    script.exhausted();
}

/// Storage for an indexer that has never kept a block and indexes from
/// block 13; every other store call fails the test.
struct Fresh;

impl base::BlockStore for Fresh {
    fn insert<'a>(&'a self, _: &'a base::Blocks) -> base::Future<'a, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn find_last<'a>(
        &'a self,
        _: &'a base::Network,
    ) -> base::Future<'a, anyhow::Result<Option<base::BlockHeader>>> {
        Box::pin(async { Ok(None) })
    }

    fn delete<'a>(&'a self, _: &'a base::Network, _: u64) -> base::Future<'a, anyhow::Result<()>> {
        unreachable!("nothing to archive")
    }
}

impl base::WalletIndexStore for Fresh {
    fn insert<'a>(&'a self, _: &'a base::WalletIndex) -> base::Future<'a, anyhow::Result<()>> {
        unreachable!("no watch")
    }

    fn find_next<'a>(
        &'a self,
        _: &'a base::Network,
        _: u64,
        _: Option<u64>,
    ) -> base::Future<'a, anyhow::Result<Option<u64>>> {
        Box::pin(async { Ok(Some(13)) })
    }

    /// None: nothing to settle.
    fn find_all<'a>(
        &'a self,
        _: &'a base::Network,
    ) -> base::Future<'a, anyhow::Result<Vec<base::WalletIndex>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn move_birthday<'a>(
        &'a self,
        _: &'a base::WalletIndex,
        _: u64,
    ) -> base::Future<'a, anyhow::Result<()>> {
        unreachable!("nothing pruned")
    }

    fn set_state<'a>(
        &'a self,
        _: &'a base::WalletIndex,
        _: base::WalletIndexState,
        _: Option<&'a str>,
    ) -> base::Future<'a, anyhow::Result<()>> {
        unreachable!("nothing pruned")
    }

    fn settle<'a>(
        &'a self,
        _: &'a base::WalletIndex,
        _: &'a base::Holding,
        _: &'a base::BlockHeader,
    ) -> base::Future<'a, anyhow::Result<bool>> {
        unreachable!("nothing to settle")
    }

    fn restart<'a>(
        &'a self,
        _: &'a base::WalletIndex,
        _: u64,
    ) -> base::Future<'a, anyhow::Result<()>> {
        unreachable!("no jump")
    }
}

impl base::InputStore for Fresh {
    fn find_unspent<'a>(
        &'a self,
        _: &'a [base::Address],
    ) -> base::Future<'a, anyhow::Result<base::Inputs>> {
        unreachable!("no spend")
    }
}

impl base::WalletStore for Fresh {
    fn insert<'a>(&'a self, _: &'a dyn base::Wallet) -> base::Future<'a, anyhow::Result<()>> {
        unreachable!("no wallet")
    }

    fn find_all<'a>(
        &'a self,
        _: &'a base::WalletFactories,
    ) -> base::Future<'a, anyhow::Result<Vec<Arc<dyn base::Wallet>>>> {
        unreachable!("no wallet")
    }
}

#[test]
fn the_settings_batch_is_the_indexers_step() {
    let stored = base::NetworkConfig {
        network: network(),
        chain_id: Some("1".to_owned()),
        testnet: false,
        block_time: std::time::Duration::from_secs(12),
        confirmations: 12,
        max_depth: None,
        explorer_address: None,
        explorer_transaction: None,
        settings: json!({"batch": 2}),
        assets: Vec::new(),
    };
    let config = Config::try_from(&stored).unwrap();
    assert_eq!(config.batch, 2);
    let defaults = base::NetworkConfig {
        settings: json!({}),
        ..stored
    };
    assert_eq!(
        Config::try_from(&defaults).unwrap().batch,
        base::DEFAULT_BATCH
    );

    let fresh = Arc::new(Fresh);
    let storage = base::Storage {
        blocks: fresh.clone(),
        indexes: fresh.clone(),
        inputs: fresh.clone(),
        wallets: fresh,
        holdings: Arc::new(base::NoHoldings),
    };
    let script = Script::new([]);
    let service = Factory::from(config)
        .service(&storage, Arc::new(script.clone()))
        .unwrap();
    // From block 13 with the tip at 16: two blocks a step, not the default 100.
    script.push([
        Reply::ok("eth_blockNumber", json!("0x10")),
        Reply::ok("eth_getBlockByNumber", json!({"number": "0xc"})),
        Reply::ok(
            "eth_getBlockByNumber",
            block(13, 0xdd, 0xcc, vec![], json!([])),
        )
        .expecting(json!(["0xd", true])),
        Reply::ok(
            "eth_getBlockByNumber",
            block(14, 0xee, 0xdd, vec![], json!([])),
        )
        .expecting(json!(["0xe", true])),
    ]);
    let next = block_on(service.indexer.next()).unwrap();
    assert!(matches!(next, base::Next::Indexed(header) if header.position == 14));
    script.exhausted();
}

#[test]
fn a_network_builds_with_its_node_down_and_asks_it_nothing() {
    let script = Script::new([]);
    let node = Faults::new(script.clone());
    let switch = node.switch();
    switch.down(true);
    let fresh = Arc::new(Fresh);
    let storage = base::Storage {
        blocks: fresh.clone(),
        indexes: fresh.clone(),
        inputs: fresh.clone(),
        wallets: fresh,
        holdings: Arc::new(base::NoHoldings),
    };
    let service = Factory::from(config())
        .service(&storage, Arc::new(node))
        .unwrap();
    let chain = service.dated.unwrap();
    script.exhausted();

    switch.down(false);
    script.push([
        Reply::ok("eth_blockNumber", json!("0x10")),
        Reply::ok("eth_getBlockByNumber", json!({"number": "0xc"})),
    ]);
    let expected = Tips {
        produced: 16,
        confirmed: 12,
    };
    assert_eq!(block_on(chain.tips()).unwrap(), expected);
    script.exhausted();
}

/// Tips with the finalized block at 12.
fn finalized_at_12() -> [Reply; 2] {
    [
        Reply::ok("eth_blockNumber", json!("0x10")),
        Reply::ok("eth_getBlockByNumber", json!({"number": "0xc"})),
    ]
}

#[test]
fn a_balance_is_read_at_the_finalized_block() {
    let owner = account(0xb1);
    let usdc = Asset {
        network: network(),
        id: TOKEN.to_owned(),
    };
    let script = Script::new([]);
    let chain = connect(&script, &config());
    script.push(finalized_at_12());
    script
        .push([Reply::ok("eth_getBalance", json!("0xde0b6b3a7640000"))
            .expecting(json!([owner, "0xc"]))]);
    script.push(finalized_at_12());
    script.push([Reply::ok(
        "eth_call",
        json!("0x00000000000000000000000000000000000000000000000000000000004c4b40"),
    )
    .expecting(json!([
        { "to": TOKEN, "data": format!("0x70a08231{:0>64}", &owner[2..]) },
        "0xc",
    ]))]);

    let native = block_on(chain.balance(&address(0xb1), &Asset::native(network()), 5));
    let token = block_on(chain.balance(&address(0xb1), &usdc, 12));

    assert_eq!(
        native.unwrap(),
        Some(Holding {
            position: 12,
            amount: Decimal::from(1_000_000_000_000_000_000_u64)
        })
    );
    assert_eq!(
        token.unwrap(),
        Some(Holding {
            position: 12,
            amount: Decimal::from(5_000_000)
        })
    );
    script.exhausted();
}

#[test]
fn a_balance_at_a_block_not_finalized_yet_is_not_answerable() {
    let script = Script::new(finalized_at_12());
    let chain = connect(&script, &config());

    let read = block_on(chain.balance(&address(0xb1), &Asset::native(network()), 13));

    assert_eq!(read.unwrap(), None, "finalized 12 is below 13");
    script.exhausted();
}
