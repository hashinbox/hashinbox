//! `Chain` over a scripted node: the tip, a block's TRX, USDT and fee legs,
//! a block not produced yet, infos a node lacks, and the node's identity.

use std::{sync::Arc, time::Duration};

use ::rpc::testing::{Reply, Script};
use base::{
    Address, Asset, AssetConfig, AssetKind, Chained as _, Dated as _, Decimal, InfoClient as _,
    Leg, Line, NetworkConfig, NotServed, Reason, TransactionState,
};
use chain_tron::{
    Chain, Client, Config, Factory, Info,
    testing::fixtures::{NILE, USDT, config, network},
};
use futures_executor::block_on;
use serde_json::{Value, json};

fn chain(script: &Script) -> Chain {
    let config = config();
    Factory::from(config.clone()).chain(&Client::new(Arc::new(script.clone()), config.native()))
}

/// The watched address: Nile's `TWLmw5…`, `41df76bd…`.
const WATCHED: &str = "TWLmw5LzBxJ1M2zirtdgdhtajFVHgZz321";
const WATCHED_HEX: &str = "41df76bd95259da7466dce0ef7e53c1e754d6f1518";
/// Who pays it USDT: `TLuvqE…`, `41780aa3…`.
const PAYER: &str = "TLuvqEDKUVLbofJKNcvWbeEppMADXg61zV";
/// Who pays it TRX: `TSwiwP…`, `41ba324b…`.
const SENDER: &str = "TSwiwPhU378g7APgGrENt9D5yqYUHHHHHH";

const HEIGHT: u64 = 73_043_095;
const PARENT: &str = "00000000045a8c965df1985f32b001b40a5fa03077f0aea50b8806b74de62d85";
const ID: &str = "00000000045a8c977cb4de6651c54f1f7d0d21e31ce4b87340c6850e4a040bce";

/// The `Transfer` topic, and an account's topic word.
const TRANSFER: &str = "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

fn word(hex: &str) -> String {
    format!("000000000000000000000000{}", &hex[2..])
}

fn address(text: &str) -> Address {
    Address::new(network(), text)
}

fn head(position: u64, id: &str, parent: &str) -> Value {
    json!({
        "blockID": id,
        "block_header": {"raw_data": {
            "number": position,
            "timestamp": 1_795_673_079_000_u64,
            "parentHash": parent,
            "txTrieRoot": "886729cecbbd137c2d2b71c4df643bbf936db1aad61502fb5af655c88cd8328d",
            "witness_address": "41e7860196ad5b5718c1d6326babab039b70b8c1cd",
            "version": 40,
        }},
    })
}

/// A `TriggerSmartContract` of `owner` on `contract` (hex), as
/// `getblockbynum` writes it.
fn trigger(id: &str, owner: &str, contract: &str, ret: &str) -> Value {
    json!({
        "txID": id,
        "ret": [{"contractRet": ret}],
        "signature": ["0835b965"],
        "raw_data": {
            "contract": [{
                "type": "TriggerSmartContract",
                "parameter": {
                    "type_url": "type.googleapis.com/protocol.TriggerSmartContract",
                    "value": {"owner_address": owner, "contract_address": contract, "data": "a9059cbb"},
                },
            }],
            "fee_limit": 225_000_000,
        },
    })
}

/// Block 73,043,095 of Nile, its transactions as the node wrote them:
/// USDT to the watched address (Nile's da7b85…), TRX to it creating its
/// account, a USDT transfer out of energy, and a transfer of a token not
/// allowlisted.
fn block() -> (Value, Value) {
    let usdt_hex = "41eca9bc828a3005b9a3b909f2cc5c2a54794de05f";
    let payer_hex = "41780aa35c3d216844f2f3d7bdfdae2926b09647ae";
    let other_hex = "41a614f803b6fd780986a42c78ec9c7f77e6ded13c";
    let mut block = head(HEIGHT, ID, PARENT);
    block["transactions"] = json!([
        trigger("da7b851f39e4d2271ec579987f1fdeae3f92df367f9ed012744d65be41f33ecb", payer_hex, usdt_hex, "SUCCESS"),
        {
            "txID": "be3074e5348f857989c742b15a3bbbeed01dfe4e53d78f5eb36e5a4ba5698780",
            "ret": [{"contractRet": "SUCCESS"}],
            "raw_data": {"contract": [{
                "type": "TransferContract",
                "parameter": {
                    "type_url": "type.googleapis.com/protocol.TransferContract",
                    "value": {
                        "owner_address": "41ba324bde91f75574cf1cc7093f28a9fca6a20409",
                        "to_address": WATCHED_HEX,
                        "amount": 986_000,
                    },
                },
            }]},
        },
        trigger("21d4d90b6ad2c406baa38359549c1d68193b84556323c52da1b80d1ae2987060", WATCHED_HEX, usdt_hex, "OUT_OF_ENERGY"),
        trigger("53c27c073a3e4a37018013fb883d348455f4801dfceaea8fdfe4a53dca9b6f32", payer_hex, other_hex, "SUCCESS"),
    ]);
    let infos = json!([
        {
            "id": "da7b851f39e4d2271ec579987f1fdeae3f92df367f9ed012744d65be41f33ecb",
            "fee": 345_000,
            "blockNumber": HEIGHT,
            "contractResult": ["0000000000000000000000000000000000000000000000000000000000000000"],
            "contract_address": usdt_hex,
            "receipt": {"origin_energy_usage": 14_650, "energy_usage_total": 14_650, "net_fee": 345_000, "result": "SUCCESS"},
            "log": [{
                "address": &usdt_hex[2..],
                "topics": [TRANSFER, word(payer_hex), word(WATCHED_HEX)],
                "data": "00000000000000000000000000000000000000000000000000000000000f4240",
            }],
        },
        {
            "id": "be3074e5348f857989c742b15a3bbbeed01dfe4e53d78f5eb36e5a4ba5698780",
            "fee": 1_100_000,
            "blockNumber": HEIGHT,
            "contractResult": [""],
            "receipt": {"net_fee": 100_000},
        },
        {
            "id": "21d4d90b6ad2c406baa38359549c1d68193b84556323c52da1b80d1ae2987060",
            "fee": 345_000,
            "blockNumber": HEIGHT,
            "contract_address": usdt_hex,
            "receipt": {"net_fee": 345_000, "result": "OUT_OF_ENERGY"},
            "result": "FAILED",
            "resMessage": "4e6f7420656e6f75676820656e65726779",
        },
        {
            "id": "53c27c073a3e4a37018013fb883d348455f4801dfceaea8fdfe4a53dca9b6f32",
            "blockNumber": HEIGHT,
            "receipt": {"energy_usage_total": 64_285, "net_usage": 345, "result": "SUCCESS"},
            "log": [{
                "address": &other_hex[2..],
                "topics": [TRANSFER, word(payer_hex), word(WATCHED_HEX)],
                "data": "0000000000000000000000000000000000000000000000000000000000000005",
            }],
        },
    ]);
    (block, infos)
}

fn get_block(reply: Value, position: u64) -> Reply {
    Reply::ok("getblockbynum", reply).expecting(json!({ "num": position }))
}

fn get_infos(reply: Value, position: u64) -> Reply {
    Reply::ok("gettransactioninfobyblocknum", reply).expecting(json!({ "num": position }))
}

#[test]
fn the_tip_is_the_newest_block_and_confirmed_is_19_below() {
    let script = Script::new([
        Reply::ok("getblock", head(HEIGHT, ID, PARENT)).expecting(json!({"detail": false}))
    ]);
    let chain = chain(&script);
    let tips = block_on(chain.tips()).unwrap();
    assert_eq!(tips.position, HEIGHT);
    assert_eq!(tips.confirmed(chain.confirmations()), HEIGHT - 19);
    assert_eq!(chain.line(), Line::Depth(u64::MAX));
    script.exhausted();
}

/// USDT and TRX in, each with its payer's fee (the TRX one creating the
/// account: 1.1 TRX); the failed transfer only its fee; the other token
/// nothing but its zero fee, none.
#[test]
fn a_block_holds_trx_usdt_and_the_trx_each_transaction_burned() {
    let (wire, infos) = block();
    let script = Script::new([get_block(wire, HEIGHT), get_infos(infos, HEIGHT)]);
    let chain = chain(&script);
    let block = block_on(chain.block(HEIGHT)).unwrap().unwrap();
    script.exhausted();
    assert_eq!(block.header.position, HEIGHT);
    assert_eq!(block.header.height, HEIGHT);
    assert_eq!(hex(&block.header.hash.0), ID);
    let parent = block.header.parent.as_ref().unwrap();
    assert_eq!(
        (parent.position, hex(&parent.hash.0)),
        (HEIGHT - 1, PARENT.to_owned())
    );
    assert_eq!(block.header.timestamp, Some(1_795_673_079));

    let trx = Asset::native(network());
    let usdt = Asset {
        network: network(),
        id: USDT.to_owned(),
    };
    let [paid, sent, failed, other] = &block.transactions[..] else {
        panic!("four transactions");
    };
    assert_eq!(
        paid.id.id,
        "da7b851f39e4d2271ec579987f1fdeae3f92df367f9ed012744d65be41f33ecb"
    );
    assert_eq!(
        (paid.state.clone(), paid.order),
        (TransactionState::Succeeded, Some(0))
    );
    assert_eq!(
        paid.legs,
        vec![
            Leg::Spent {
                address: address(PAYER),
                asset: usdt.clone(),
                amount: Decimal::from(1_000_000),
                coin: None,
            },
            Leg::Received {
                address: address(WATCHED),
                asset: usdt,
                amount: Decimal::from(1_000_000),
                coin: None,
            },
            Leg::Fee {
                address: address(PAYER),
                asset: trx.clone(),
                amount: Decimal::from(345_000),
            },
        ]
    );
    assert_eq!(
        sent.legs,
        vec![
            Leg::Spent {
                address: address(SENDER),
                asset: trx.clone(),
                amount: Decimal::from(986_000),
                coin: None,
            },
            Leg::Received {
                address: address(WATCHED),
                asset: trx.clone(),
                amount: Decimal::from(986_000),
                coin: None,
            },
            Leg::Fee {
                address: address(SENDER),
                asset: trx.clone(),
                amount: Decimal::from(1_100_000),
            },
        ]
    );
    assert_eq!(
        failed.state,
        TransactionState::Reverted(Some(Reason::Reverted))
    );
    assert_eq!(
        failed.legs,
        vec![Leg::Fee {
            address: address(WATCHED),
            asset: trx,
            amount: Decimal::from(345_000),
        }]
    );
    assert!(other.legs.is_empty(), "{:?}", other.legs);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A block not produced yet (`{}`) ends the run; one alone is an error.
#[test]
fn a_block_not_produced_yet_ends_the_run() {
    let script = Script::new([get_block(json!({}), HEIGHT)]);
    let chain = chain(&script);
    assert!(
        block_on(chain.blocks(HEIGHT, HEIGHT + 5))
            .unwrap()
            .is_empty()
    );
    script.push([get_block(json!({}), HEIGHT)]);
    assert!(block_on(chain.block(HEIGHT)).is_err());
    script.exhausted();
}

/// No infos for a block with transactions: near the tip the run ends
/// before it; 19 or more below, the node does not serve it.
#[test]
fn a_block_without_its_infos_waits_near_the_tip_and_is_not_served_below() {
    let (wire, _) = block();
    let tip = |position: u64| Reply::ok("getblock", head(position, ID, PARENT));
    let script = Script::new([
        get_block(wire.clone(), HEIGHT),
        get_infos(json!([]), HEIGHT),
        tip(HEIGHT + 2),
    ]);
    let chain = chain(&script);
    assert!(block_on(chain.blocks(HEIGHT, HEIGHT)).unwrap().is_empty());
    script.push([
        get_block(wire.clone(), HEIGHT),
        get_infos(json!([]), HEIGHT),
        tip(HEIGHT + 19),
    ]);
    let error = block_on(chain.blocks(HEIGHT, HEIGHT)).unwrap_err();
    assert_eq!(
        NotServed::find(&error),
        Some(NotServed { position: HEIGHT })
    );
    script.push([
        get_block(wire, HEIGHT),
        get_infos(json!([{"id": "aa"}]), HEIGHT),
    ]);
    assert!(
        block_on(chain.blocks(HEIGHT, HEIGHT)).is_err(),
        "1 info for 4"
    );
    script.exhausted();
}

#[test]
fn a_blocks_time_is_its_header_and_a_node_is_its_genesis_id() {
    let script = Script::new([Reply::ok("getblock", head(HEIGHT, ID, PARENT))
        .expecting(json!({"id_or_num": HEIGHT.to_string(), "detail": false}))]);
    assert_eq!(
        block_on(chain(&script).time(HEIGHT)).unwrap(),
        Some(1_795_673_079)
    );

    // Genesis has no number (zero is left out).
    let genesis = json!({"blockID": NILE, "block_header": {"raw_data": {
        "txTrieRoot": "6556a96828248d6b89cfd0487d4cef82b134b5544dc428c8a218beb2db85ab24",
        "parentHash": "e58f33f9baf9305dc6f82b9f1934ea8f0ade2defb951258d50167028c780351f",
    }}});
    script.push([
        Reply::ok("getblock", genesis).expecting(json!({"id_or_num": "0", "detail": false})),
        Reply::ok("getblock", head(HEIGHT, ID, PARENT)),
    ]);
    let info = block_on(Info::new(Arc::new(script.clone())).info()).unwrap();
    assert_eq!((info.chain_id.as_str(), info.tip), (NILE, HEIGHT));
    script.exhausted();
}

/// A stored network: its genesis id, TRX at 6 decimals, USDT by its `T…`
/// text; `settings` may set the limits.
#[test]
fn a_stored_network_is_read_and_checked() {
    let asset = |kind, contract: Option<&str>, decimals| AssetConfig {
        asset: contract.unwrap_or("native").to_owned(),
        kind,
        contract: contract.map(str::to_owned),
        ticker: "X".to_owned(),
        decimals,
        display_decimals: None,
    };
    let stored = |assets, settings| NetworkConfig {
        network: network(),
        chain_id: Some(NILE.to_owned()),
        testnet: true,
        block_time: Duration::from_secs(3),
        confirmations: 19,
        max_depth: None,
        explorer_address: None,
        explorer_transaction: None,
        settings,
        assets,
    };
    let good = vec![
        asset(AssetKind::Native, None, 6),
        asset(AssetKind::Contract, Some(USDT), 6),
    ];
    let read = Config::try_from(&stored(good.clone(), json!({"limits": {"energy": 1}}))).unwrap();
    assert_eq!(read.tokens[0].contract.to_string(), USDT);
    assert_eq!(read.limits.energy, 1);
    assert_eq!(read.limits.max_fee_limit, 100_000_000);
    read.validate().unwrap();
    for bad in [
        stored(vec![asset(AssetKind::Native, None, 18)], json!({})),
        stored(
            vec![asset(
                AssetKind::Contract,
                Some("41eca9bc828a3005b9a3b909f2cc5c2a54794de05f"),
                6,
            )],
            json!({}),
        ),
        stored(good, json!({"nope": 1})),
    ] {
        assert!(Config::try_from(&bad).is_err());
    }
}
