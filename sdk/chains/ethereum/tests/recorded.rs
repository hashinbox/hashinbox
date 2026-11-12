//! The mock Sepolia against its recordings (offline), and the record step
//! that refreshes them (`RECORD=1`, by hand, from a live node).

use std::path::Path;

use ::rpc::{Client as _, Request};
use base::testing::TestChain as _;
use chain_ethereum::testing::{Ethereum, TOKEN, recording, recordings};
use futures_executor::block_on;
use serde_json::{Value, json};

/// A holder of ether and the token on the mock.
const HOLDER: &str = "0x00000000000000000000000000000000000000b1";

/// Asks node 0 of `ethereum`.
fn ask(ethereum: &Ethereum, method: &str, params: Value) -> rpc::Answer {
    match block_on(ethereum.mock(0).request(Request::new(method, params))) {
        Err(rpc::Error::Remote(failure)) => Err(failure),
        reply => Ok(reply.expect("the mock answers")),
    }
}

/// For every recording, the mock answers the same request on an
/// equivalent state (a block with an ether and a token transfer, a node
/// that pruned it, an empty key) in the recorded shape: the same fields
/// and kinds for a result, the exact code and message for an error.
#[test]
fn the_mock_answers_as_the_recorded_node() {
    let ethereum = Ethereum::start(1);
    ethereum.fund(HOLDER, "1000000000000000000", "native");
    ethereum.fund(HOLDER, "5000000", TOKEN);
    ethereum.mine(1);
    let height = format!("{:#x}", ethereum.chain().tip());
    let block = ask(&ethereum, "eth_getBlockByNumber", json!([height, false])).unwrap();
    let block: Value = block.deserialize().unwrap();
    let (native, token) = (&block["transactions"][0], &block["transactions"][1]);
    let future = format!("{:#x}", ethereum.chain().tip() + 100_000);
    let mut differences = Vec::new();
    for name in recordings() {
        let recorded = recording(name);
        let params = match name {
            "eth_getBlockByNumber.full" => json!([height, true]),
            "eth_getBlockByNumber.hashes" => json!([height, false]),
            "eth_getBlockByNumber.future" => json!([future, false]),
            "eth_getBlockReceipts" | "eth_getBlockReceipts.pruned" => json!([block["hash"]]),
            "eth_getTransactionReceipt" | "eth_getTransactionByHash" => json!([token]),
            "eth_getTransactionReceipt.pruned" => json!([native]),
            "eth_getTransactionCount" => json!([HOLDER, "pending"]),
            "eth_getLogs" => {
                let mut params = recorded.params.clone();
                params[0]["fromBlock"] = json!(height);
                params[0]["toBlock"] = json!(height);
                params
            }
            "eth_estimateGas" => {
                let mut params = recorded.params.clone();
                params[0]["from"] = json!(HOLDER);
                params
            }
            _ => recorded.params.clone(),
        };
        let pruned = name.ends_with(".pruned");
        let floor = if pruned {
            ethereum.chain().tip() + 1
        } else {
            0
        };
        ethereum.node(0).prune(floor);
        if let Err(difference) = recorded.check(&ask(&ethereum, &recorded.method, params)) {
            differences.push(format!("{name}: {difference}"));
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}

/// Refreshes `src/testing/recordings/` from `SEPOLIA_URL` (default: the
/// public node). `RECORD=1 cargo test -p chain-ethereum record -- --ignored`.
#[test]
#[ignore = "asks a live node; run by hand with RECORD=1"]
fn record() {
    if std::env::var("RECORD").as_deref() != Ok("1") {
        return;
    }
    let url = std::env::var("SEPOLIA_URL")
        .unwrap_or_else(|_| "https://ethereum-sepolia-rpc.publicnode.com".to_owned());
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/testing/recordings");
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    runtime
        .block_on(chain_ethereum::testing::record(&url, &directory))
        .expect("every reply recorded");
}
