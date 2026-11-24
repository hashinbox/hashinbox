//! The server suite: each case drives the API, lets the real workers
//! (index, broadcast, triggers) run over a mock chain's nodes, and
//! reads the outcome back through the API. One scenario set for every
//! chain; `scenarios!` names which chain and topology runs each.

mod ahead;
mod backlog;
mod boot;
mod broken;
mod deposits;
mod duplicate;
mod failover;
mod finality;
mod finalized;
mod forked;
mod lagging;
mod limited;
mod lost;
mod outage;
mod partial;
mod paused;
mod pruned;
mod pruned_one;
mod queued;
mod refused;
mod reorg;
mod send;
mod shorter;
mod skipped;
mod slow;
mod split;
mod stale;

use base::testing::TestChain;
use rpc::testing::Fault;
use serde_json::{Value, json};

use super::support::{App, Wallet, scenarios, until};

scenarios! {
    deposits: ethereum => chain_ethereum::testing::Ethereum [single];
    send: ethereum => chain_ethereum::testing::Ethereum [single, cluster];
    duplicate: ethereum => chain_ethereum::testing::Ethereum [single, cluster];
    reorg: ethereum => chain_ethereum::testing::Ethereum [single, cluster];
    shorter: ethereum => chain_ethereum::testing::Ethereum [single];
    pruned: ethereum => chain_ethereum::testing::Ethereum [single, cluster];
    lagging: ethereum => chain_ethereum::testing::Ethereum [cluster];
    refused: ethereum => chain_ethereum::testing::Ethereum [cluster];
    lost: ethereum => chain_ethereum::testing::Ethereum [cluster];
    ahead: ethereum => chain_ethereum::testing::Ethereum [cluster];
    forked: ethereum => chain_ethereum::testing::Ethereum [cluster];
    stale: ethereum => chain_ethereum::testing::Ethereum [
        #[ignore = "finding: a primary stuck on a dead fork is kept (until the 120 s refresh); its block and leg stay live"] cluster
    ];
    failover: ethereum => chain_ethereum::testing::Ethereum [cluster];
    outage: ethereum => chain_ethereum::testing::Ethereum [single, cluster];
    limited: ethereum => chain_ethereum::testing::Ethereum [cluster];
    slow: ethereum => chain_ethereum::testing::Ethereum [cluster];
    pruned_one: ethereum => chain_ethereum::testing::Ethereum [
        #[ignore = "finding: a pruned primary's null receipts move the start up although another node serves them"] cluster
    ];
    broken: ethereum => chain_ethereum::testing::Ethereum [
        #[ignore = "finding: a node answering garbage is never failed over; the index fails every interval (a storm)"] cluster
    ];
    finality: ethereum => chain_ethereum::testing::Ethereum [cluster];
    finalized: ethereum => chain_ethereum::testing::Ethereum [
        #[ignore = "finding: confirmed is tip - confirmations; no node's finalized bounds the confirmed balance"] cluster
    ];
    backlog: ethereum => chain_ethereum::testing::Ethereum [single];
    queued: ethereum => chain_ethereum::testing::Ethereum [single];
    partial: ethereum => chain_ethereum::testing::Ethereum [single];
    paused: ethereum => chain_ethereum::testing::Ethereum [single];
    skipped: ethereum => chain_ethereum::testing::Ethereum [single];
    split: ethereum => chain_ethereum::testing::Ethereum [single];
}

/// One whole unit of the native currency, in atomic units.
fn unit<T: TestChain>(chain: &T) -> String {
    let decimals = chain.network().assets[0].decimals;
    format!("1{}", "0".repeat(usize::try_from(decimals).unwrap_or(0)))
}

/// A quarter unit of the native currency, in atomic units.
fn quarter<T: TestChain>(chain: &T) -> String {
    let decimals = chain.network().assets[0].decimals;
    format!(
        "25{}",
        "0".repeat(usize::try_from(decimals).unwrap_or(2) - 2)
    )
}

/// Atomic units as a number.
fn number(text: &str) -> u128 {
    text.parse().expect("atomic units")
}

/// Waits until job `id` succeeded: a send's block is final.
async fn succeeded(app: &App, id: &str) -> Value {
    until("the send job to succeed", || async {
        let job = app.api.job(id).await;
        if job["state"] == "succeeded" {
            return Ok(job);
        }
        Err(job.to_string())
    })
    .await
}

/// Waits until send job `id` sent its transaction: a node accepted it, or
/// may hold it.
async fn accepted(app: &App, id: &str) -> Value {
    until("the send to go out", || async {
        let job = app.api.job(id).await;
        if job["accepted"] == true {
            return Ok(job);
        }
        Err(job.to_string())
    })
    .await
}

/// Makes the next send retry twice before it is signed, holding its
/// network meanwhile: node 0 answers the sender's next two nonce reads
/// (Ethereum's, as the cases that stall run on Ethereum) with an error
/// object, which the transport does not retry but the job does.
fn stall<T: TestChain>(chain: &T) {
    for _ in 0..2 {
        chain.node(0).faults().fail_next(
            Some("eth_getTransactionCount"),
            Fault::Remote(-32000, "busy".to_owned()),
        );
    }
}

/// Mines the next block, waits until it is read, then mines enough after
/// it to make it confirmed: mined at once, the next block would be
/// confirmed unread, below the line, and settled past.
async fn settle<T: TestChain>(app: &App, chain: &T) {
    chain.mine(1);
    at_tip(app, chain).await;
    chain.mine(chain.network().confirmations);
}

/// A field of an index as text (`null` as empty).
fn text(index: &Value, field: &str) -> String {
    index[field].as_str().unwrap_or_default().to_owned()
}

/// Waits until `wallet`'s index in `asset` has `field` at `value`.
async fn reaches(app: &App, wallet: &Wallet, asset: &str, field: &str, value: &str) -> Value {
    until(&format!("{asset} {field} {value}"), || async {
        let index = app.api.index_of(wallet, asset).await;
        if text(&index, field) == value {
            return Ok(index);
        }
        let network = app.api.network().await;
        Err(format!("{index} {}", network["index"]))
    })
    .await
}

/// Waits until the network is indexed to the chain's tip: the network.
async fn at_tip<T: TestChain>(app: &App, chain: &T) -> Value {
    let tip = chain.tip();
    until(&format!("the index at the chain's tip {tip}"), || async {
        let network = app.api.network().await;
        if network["index"]["tip"] == tip && network["index"]["indexed"] == tip {
            return Ok(network);
        }
        Err(network.to_string())
    })
    .await
}

/// The live kept blocks, checked to be one branch: each the child of the
/// one below it, unless that one is below its parent (a gap a settle
/// leaves, design 083).
async fn linked<T: TestChain>(app: &App, chain: &T) -> Vec<Value> {
    let blocks = app.api.blocks(&chain.network()).await;
    for pair in blocks.windows(2) {
        let (child, below) = (&pair[0], &pair[1]);
        if child["parent_position"].as_u64() > below["position"].as_u64() {
            continue;
        }
        assert_eq!(child["parent_position"], below["position"], "{blocks:?}");
        assert_eq!(child["parent"], below["id"], "{blocks:?}");
    }
    blocks
}

/// Indexes `wallet` in `asset` from the tip and waits until it is open: on
/// a chain that reads balances it starts at the chain's final block, its
/// balance null until the chain's balance read there settles it; a deposit
/// after that is a leg of its own.
async fn open(app: &App, wallet: &Wallet, asset: &str) {
    app.api.index(wallet, asset, None).await;
    until(&format!("{asset} open"), || async {
        let index = app.api.index_of(wallet, asset).await;
        if index["balance_pending"].is_string() {
            return Ok(());
        }
        Err(index.to_string())
    })
    .await;
}

/// A new wallet indexed in the native currency from the tip, holding one
/// unit, final.
async fn funded<T: TestChain>(app: &App, chain: &T) -> Wallet {
    let wallet = app.api.wallet(&chain.network()).await;
    app.api.index(&wallet, "native", None).await;
    chain.fund(&wallet.address, &unit(chain), "native");
    settle(app, chain).await;
    reaches(app, &wallet, "native", "balance_confirmed", &unit(chain)).await;
    wallet
}

/// A new wallet indexed in the native currency through the tip, empty.
async fn indexed<T: TestChain>(app: &App, chain: &T) -> Wallet {
    let wallet = app.api.wallet(&chain.network()).await;
    open(app, &wallet, "native").await;
    let tip = chain.tip();
    until("the payee indexed through the tip", || async {
        let index = app.api.index_of(&wallet, "native").await;
        if index["indexed_through"] == tip {
            return Ok(());
        }
        Err(index.to_string())
    })
    .await;
    wallet
}

/// Creates triggers `ids` in one batch, so a round sees all or none of
/// them: each, while `source` can spend 0.9 or more, pays a quarter to
/// `payee`. A fixed payout: each decision is a real payment.
async fn payout<T: TestChain>(app: &App, chain: &T, ids: &[&str], source: &Wallet, payee: &Wallet) {
    let network = chain.network().network;
    let triggers: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "chain": network.chain.as_str(),
                "network": network.name,
                "selector": {"ids": [format!("{}.native", source.id)]},
                "check": {"compare": {
                    "of": {"this": {}},
                    "aggregate": "sum",
                    "operator": "at_least",
                    "value": {"value": "0.9"},
                }},
                "action": {"name": "send", "input": {
                    "source": {"this": {}},
                    "asset": "native",
                    "destination": {"select": {"ids": [format!("{}.native", payee.id)]}},
                    "amount": {"value": "0.25"},
                }},
            })
        })
        .collect();
    let body = json!({ "triggers": triggers });
    let (status, created) = app
        .api
        .call(axum::http::Method::POST, "/v1/triggers", Some(body))
        .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{created}");
}

/// The jobs `GET /v1/jobs?{query}` lists, last written first (at most 500).
async fn jobs(app: &App, query: &str) -> Vec<Value> {
    let page = app.api.get(&format!("/v1/jobs?limit=500&{query}")).await;
    page["jobs"].as_array().cloned().unwrap_or_default()
}

/// A job's id as a number: ids order as numbers, not as text.
fn id(job: &Value) -> u64 {
    job["id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .unwrap_or_default()
}

/// The broadcast jobs sending from `wallet`.
async fn sends_from(app: &App, wallet: &Wallet) -> Vec<Value> {
    let all = jobs(app, "kind=broadcast").await;
    all.into_iter()
        .filter(|job| job["input"]["address"] == wallet.address.as_str())
        .collect()
}

/// The runs of trigger `id`, latest first.
async fn runs(app: &App, id: &str) -> Vec<Value> {
    let page = app.api.get(&format!("/v1/triggers/{id}/runs")).await;
    page["runs"].as_array().cloned().unwrap_or_default()
}

/// Waits until every send from `wallet` went out, mines them final, waits
/// until every send of the network succeeded (none is checked any more, so
/// none holds a round back), and then until a round started after the last
/// of them: it decided with them spent. Their jobs, oldest first.
async fn decided_after<T: TestChain>(app: &App, chain: &T, wallet: &Wallet) -> Vec<Value> {
    until("the sends to go out", || async {
        let sends = sends_from(app, wallet).await;
        if !sends.is_empty() && sends.iter().all(|job| job["accepted"] == true) {
            return Ok(());
        }
        Err(format!("{sends:?}"))
    })
    .await;
    settle(app, chain).await;
    until("every send to succeed", || async {
        let all = jobs(app, "kind=broadcast").await;
        if all.iter().all(|job| job["state"] == "succeeded") {
            return Ok(());
        }
        Err(format!("{all:?}"))
    })
    .await;
    let mut sends = sends_from(app, wallet).await;
    sends.sort_by_key(id);
    let last = sends.last().map(id);
    chain.mine(1);
    until("a round after the sends", || async {
        // A round that acted, or one that had nothing to do.
        let mut rounds = jobs(app, "kind=triggers&state=succeeded").await;
        rounds.extend(jobs(app, "kind=triggers&state=idle").await);
        if rounds.iter().any(|round| Some(id(round)) > last) {
            return Ok(());
        }
        Err(format!("{rounds:?}"))
    })
    .await;
    sends
}

/// The rounds of the network not yet finished (pending or running).
async fn unfinished_rounds<T: TestChain>(app: &App, chain: &T) -> usize {
    let network = chain.network().network;
    let at = format!(
        "chain={}&network={}&kind=triggers",
        network.chain.as_str(),
        network.name
    );
    // One read: a round starting between two reads would count twice.
    let rounds = jobs(app, &at).await;
    rounds
        .iter()
        .filter(|round| round["state"] == "pending" || round["state"] == "running")
        .count()
}
