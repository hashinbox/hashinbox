//! BSC over scripted nodes: a stored network is `bnb` with BNB and 18-decimal
//! BEP-20 tokens, a block's BNB and BEP-20 transfers become legs, and a send
//! signs for chain 56 at BSC's fees.

use std::{sync::Arc, time::Duration};

use ::rpc::testing::{Reply, Script};
use alloy_consensus::{Transaction as _, TxEnvelope};
use alloy_eips::Decodable2718 as _;
use alloy_primitives::U256;
use base::{
    Asset, AssetConfig, AssetKind, Chained as _, Decimal, Leg, NetworkConfig, ServiceFactory as _,
    Transfer, WalletFactory as _,
};
use chain_bnb::{BNB, CHAIN_NAME, Client, Factory, MAINNET_CHAIN_ID, Network};
use crypto::SecretBytes;
use futures_executor::block_on;
use serde_json::{Value, json};

/// BSC-USD (Tether's USDT on BSC), as stored: 18 decimals on this chain.
const USDT: &str = "0x55d398326f99059ff775485246999027b3197955";
/// 100 USDT at 18 decimals: above `u64::MAX` atomic units.
const HUNDRED: &str = "100000000000000000000";
const TRANSFER: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
/// The well-known address of secret key 1.
const KEY_ONE: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";

fn network() -> base::Network {
    base::Network::new(CHAIN_NAME, "mainnet")
}

fn units(text: &str) -> Decimal {
    text.parse().unwrap()
}

fn hash(byte: u64) -> String {
    format!("0x{byte:064x}")
}

fn account(byte: u64) -> String {
    format!("0x{byte:040x}")
}

fn address(byte: u64) -> base::Address {
    base::Address::new(network(), account(byte))
}

fn usdt() -> Asset {
    Asset {
        network: network(),
        id: USDT.to_owned(),
    }
}

/// BSC mainnet as `seed.sql` stores it, `settings` as given.
fn stored(settings: Value) -> NetworkConfig {
    let asset = |asset: &str, kind, contract: Option<&str>, ticker: &str| AssetConfig {
        asset: asset.to_owned(),
        kind,
        contract: contract.map(str::to_owned),
        ticker: ticker.to_owned(),
        decimals: 18,
        display_decimals: None,
    };
    NetworkConfig {
        network: network(),
        chain_id: Some(MAINNET_CHAIN_ID.to_string()),
        testnet: false,
        block_time: Duration::from_millis(450),
        confirmations: 15,
        max_depth: None,
        explorer_address: None,
        explorer_transaction: None,
        settings,
        assets: vec![
            asset("native", AssetKind::Native, None, "BNB"),
            asset(USDT, AssetKind::Contract, Some(USDT), "USDT"),
        ],
    }
}

fn factory() -> chain_ethereum::Factory {
    Factory::new(&stored(json!({"batch": 10}))).unwrap().0
}

fn client(script: &Script) -> Client {
    Client::new(
        Arc::new(script.clone()),
        Network::of(BNB, MAINNET_CHAIN_ID, "mainnet"),
    )
}

#[test]
fn a_stored_network_is_bnb_with_bnb_and_18_decimal_tokens() {
    let config = chain_bnb::config(&stored(json!({"batch": 10}))).unwrap();
    assert_eq!(config.chain_id, 56);
    assert_eq!(config.confirmations, 15);
    assert_eq!(config.batch, 10);
    assert_eq!(config.interval_ms, 3_000, "BSC's default");
    assert_eq!(config.tracking.bump_after, 400, "BSC's default");
    let assets = config.assets();
    let native = assets.get(&Asset::native(network())).unwrap();
    assert_eq!(native.asset.network, network());
    assert_eq!(native.ticker.as_deref(), Some("BNB"));
    assert_eq!(native.decimals, 18);
    let token = assets.get(&usdt()).unwrap();
    assert_eq!(
        (token.ticker.as_deref(), token.decimals),
        (Some("USDT"), 18)
    );

    let set = chain_bnb::config(&stored(json!({"interval_ms": 1_000}))).unwrap();
    assert_eq!(set.interval_ms, 1_000, "a stored setting wins");

    // Neither family builds the other's networks.
    assert!(chain_ethereum::Factory::new(&stored(json!({}))).is_err());
    let ethereum = NetworkConfig {
        network: base::Network::new(chain_ethereum::CHAIN_NAME, "mainnet"),
        ..stored(json!({}))
    };
    assert!(Factory::new(&ethereum).is_err());
}

#[test]
fn a_block_s_bnb_and_bep20_transfers_become_legs() {
    let script = Script::new([]);
    let chain = factory().chain(&client(&script));
    let (height, own) = (5_u64, 0x55_u64);
    let tx = |index: u64, from: u64, to: &str, value: u64| {
        json!({
            "hash": hash(0x100 + index), "from": account(from), "to": to,
            "value": format!("{value:#x}"), "transactionIndex": format!("{index:#x}"),
            "blockHash": hash(own), "blockNumber": format!("{height:#x}"),
        })
    };
    let receipt = |index: u64, from: u64, to: &str| {
        json!({
            "transactionHash": hash(0x100 + index), "transactionIndex": format!("{index:#x}"),
            "blockHash": hash(own), "blockNumber": format!("{height:#x}"),
            "from": account(from), "to": to, "status": "0x1",
            "gasUsed": "0x5208", "effectiveGasPrice": "0x2faf080", "contractAddress": null,
            "logs": [],
        })
    };
    let topic = |byte: u64| format!("0x{byte:064x}");
    let bnb_wei: u64 = 1_500_000_000_000_000_000;
    let hundred = U256::from_str_radix(HUNDRED, 10).unwrap();
    // BSC blocks carry a zero base fee and no withdrawals.
    let block = json!({
        "hash": hash(own), "parentHash": hash(0x44), "number": format!("{height:#x}"),
        "sha3Uncles": hash(0), "miner": account(0), "stateRoot": hash(0),
        "transactionsRoot": hash(0), "receiptsRoot": hash(0),
        "logsBloom": format!("0x{}", "0".repeat(512)), "difficulty": "0x2",
        "gasLimit": "0x4c4b400", "gasUsed": "0x0", "timestamp": "0x64",
        "extraData": "0x", "mixHash": hash(0), "nonce": "0x0000000000000000",
        "baseFeePerGas": "0x0", "uncles": [],
        "transactions": [tx(0, 0xa, &account(0xb), bnb_wei), tx(1, 0xc, USDT, 0)],
    });
    let log = json!({
        "address": USDT, "topics": [TRANSFER, topic(0xc), topic(0xd)],
        "data": format!("{hundred:#066x}"), "logIndex": "0x0",
        "transactionHash": hash(0x101), "transactionIndex": "0x1",
        "blockHash": hash(own), "blockNumber": format!("{height:#x}"), "removed": false,
    });
    script.push([
        Reply::ok("eth_getBlockByNumber", block).expecting(json!(["0x5", true])),
        Reply::ok(
            "eth_getBlockReceipts",
            json!([receipt(0, 0xa, &account(0xb)), receipt(1, 0xc, USDT)]),
        ),
        Reply::ok("eth_getLogs", json!([log])).expecting(json!([{
            "fromBlock": "0x5", "toBlock": "0x5", "address": [USDT], "topics": [TRANSFER],
        }])),
    ]);
    let block = block_on(chain.block(height)).unwrap().unwrap();
    script.exhausted();

    let native = Asset::native(network());
    let fee = |payer| Leg::Fee {
        address: address(payer),
        asset: native.clone(),
        amount: units(&(21_000_u64 * 50_000_000).to_string()),
    };
    let [bnb, token] = block.transactions.0.as_slice() else {
        panic!("two transactions");
    };
    assert_eq!(
        bnb.legs,
        vec![
            Leg::Spent {
                address: address(0xa),
                asset: native.clone(),
                amount: units(&bnb_wei.to_string()),
                coin: None
            },
            Leg::Received {
                address: address(0xb),
                asset: native.clone(),
                amount: units(&bnb_wei.to_string()),
                coin: None
            },
            fee(0xa),
        ]
    );
    assert_eq!(bnb.id.id, hash(0x100));
    assert_eq!(bnb.id.network, network(), "ids read bnb.mainnet.…");
    assert_eq!(
        token.legs,
        vec![
            Leg::Spent {
                address: address(0xc),
                asset: usdt(),
                amount: units(HUNDRED),
                coin: None
            },
            Leg::Received {
                address: address(0xd),
                asset: usdt(),
                amount: units(HUNDRED),
                coin: None
            },
            fee(0xc),
        ]
    );
}

#[test]
fn a_bep20_send_signs_for_chain_56_at_bsc_fees() {
    let script = Script::new([]);
    let wallets = factory().wallets(
        &client(&script),
        Arc::new(base::NoHoldings),
        Arc::new(base::NoTransactionIndexes),
    );
    let mut secret = [0_u8; 32];
    secret[31] = 1;
    let wallet = block_on(wallets.from_pk(SecretBytes::new(secret.to_vec()))).unwrap();
    assert_eq!(wallet.address(), &base::Address::new(network(), KEY_ONE));
    let to = address(0xb0);
    let mut builder = wallet.transaction(&usdt()).unwrap();
    // BSC's answers: a 0.05 gwei tip over a zero base fee.
    script.push([
        Reply::ok("eth_getTransactionCount", json!("0x3")),
        Reply::ok("eth_maxPriorityFeePerGas", json!("0x2faf080")),
        Reply::ok("eth_getBlockByNumber", json!({"baseFeePerGas": "0x0"})),
        Reply::ok("eth_estimateGas", json!("0xc350")),
    ]);
    builder
        .from_transfer(Transfer::new(to.clone(), units(HUNDRED)))
        .unwrap();
    let transaction = block_on(builder.sign()).unwrap();
    script.exhausted();

    let envelope = TxEnvelope::decode_2718_exact(&transaction.bytes).unwrap();
    assert_eq!(envelope.chain_id(), Some(56));
    assert_eq!(envelope.nonce(), 3);
    assert_eq!(envelope.to(), Some(USDT.parse().unwrap()));
    assert_eq!(envelope.value(), U256::ZERO);
    assert_eq!(envelope.max_fee_per_gas(), 50_000_000, "2 * 0 + tip");
    assert_eq!(envelope.max_priority_fee_per_gas(), Some(50_000_000));
    // `transfer(to, 100e18)`.
    let mut call = hex(&format!("a9059cbb{:0>64}", &to.value[2..]));
    call.extend(
        U256::from_str_radix(HUNDRED, 10)
            .unwrap()
            .to_be_bytes::<32>(),
    );
    assert_eq!(envelope.input().as_ref(), call.as_slice());
    let from = base::Address::new(network(), KEY_ONE);
    let gas = 60_000_u64;
    assert_eq!(
        transaction.legs,
        vec![
            Leg::Spent {
                address: from.clone(),
                asset: usdt(),
                amount: units(HUNDRED),
                coin: None
            },
            Leg::Received {
                address: to,
                asset: usdt(),
                amount: units(HUNDRED),
                coin: None
            },
            Leg::Fee {
                address: from,
                asset: Asset::native(network()),
                amount: units(&(gas * 50_000_000).to_string()),
            },
        ]
    );
}

fn hex(text: &str) -> Vec<u8> {
    alloy_primitives::hex::decode(text).unwrap()
}
