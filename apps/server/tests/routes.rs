//! The routes over the scripted network and a real SQLite file: create,
//! list and read wallets; balance and history of indexed blocks; a send
//! enqueued and read back as a job; assets, networks and health.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use base::{
    Block, BlockHash, BlockHeader, BlockStore, Blocks, Decimal, Leg, Transaction, TransactionId,
    TransactionState,
};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::{Script, database, limited, network, payments};
use crate::{api, state::AppState};

async fn call(
    router: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .expect("a request");
    let response = router.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Block 0, paying `amount` atomic units to `to`.
fn paid(to: &str, amount: u64) -> Blocks {
    let transaction = Transaction {
        id: TransactionId::new(network(), "t0"),
        state: TransactionState::Succeeded,
        order: Some(0),
        legs: vec![Leg::Received {
            address: base::Address::new(network(), to),
            asset: base::Asset::native(network()),
            amount: Decimal::from(amount),
            coin: None,
        }],
        bytes: vec![0],
        expires: None,
    };
    let mut blocks = Blocks::new(network());
    assert!(blocks.push(Block {
        network: network(),
        header: BlockHeader {
            position: 0,
            height: 0,
            hash: BlockHash(vec![0; 32]),
            parent: None,
            timestamp: Some(1),
        },
        transactions: vec![transaction].into(),
        rewards: Vec::new(),
    }));
    blocks
}

#[tokio::test(flavor = "multi_thread")]
async fn wallets_balance_history_and_sends() {
    let (storage, _directory) = database().await;
    let payments = payments(&storage, Arc::new(Script::default())).await;
    let state = Arc::new(AppState {
        payments,
        storage: storage.clone(),
        explorers: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = api::router(state, &config).expect("a router");
    let wallet = |address: Option<&str>| json!({ "chain": "bitcoin", "network": "regtest", "address": address, "start": 0 });

    let (status, generated) = call(&router, Method::POST, "/v1/wallets", Some(wallet(None))).await;
    assert_eq!(status, StatusCode::CREATED, "{generated}");
    assert_eq!(generated["keyed"], true);
    assert_eq!(generated["assets"], json!(["native"]));
    let (status, watched) = call(
        &router,
        Method::POST,
        "/v1/wallets",
        Some(wallet(Some("alice"))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{watched}");
    assert_eq!(watched["id"], "bitcoin:regtest:alice");
    assert_eq!(watched["keyed"], false);
    let (status, list) = call(&router, Method::GET, "/v1/wallets", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["wallets"].as_array().map(Vec::len), Some(2));

    BlockStore::insert(&storage, &paid("alice", 150_000_000))
        .await
        .expect("block 0 kept");
    let (status, balance) = call(
        &router,
        Method::GET,
        "/v1/wallets/bitcoin:regtest:alice/balance",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{balance}");
    assert_eq!(balance["amount"], "1.5");
    assert_eq!(balance["indexed_through"], 0);
    let (status, _) = call(
        &router,
        Method::PUT,
        "/v1/wallets/bitcoin:regtest:alice/tags/vip",
        None,
    )
    .await;
    assert!(status.is_success(), "{status}");
    let (status, list) = call(&router, Method::GET, "/v1/wallets", None).await;
    assert_eq!(status, StatusCode::OK);
    let alice = list["wallets"]
        .as_array()
        .and_then(|wallets| wallets.iter().find(|w| w["id"] == "bitcoin:regtest:alice"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(alice["tags"], json!(["vip"]), "{list}");
    assert_eq!(alice["balances"], json!({ "native": "1.5" }), "{list}");
    assert_eq!(generated["tags"], Value::Null);
    let (status, history) = call(
        &router,
        Method::GET,
        "/v1/wallets/bitcoin:regtest:alice/transactions?limit=1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history["transactions"][0]["transaction_id"], "t0");
    assert_eq!(history["transactions"][0]["movements"][0]["amount"], "1.5");
    assert_eq!(history["next_cursor"], "0:0:t0");
    let (status, _) = call(
        &router,
        Method::GET,
        "/v1/wallets/bitcoin:regtest:alice/balance?asset=usdc",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let send = json!({ "transfers": [{ "destination": "bob", "amount": "0.1" }] });
    let (status, _) = call(
        &router,
        Method::POST,
        "/v1/wallets/bitcoin:regtest:alice/transactions",
        Some(send.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let from = format!(
        "/v1/wallets/{}/transactions",
        generated["id"].as_str().unwrap_or_default()
    );
    let (status, accepted) = call(&router, Method::POST, &from, Some(send)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let job = format!("/v1/jobs/{}", accepted["job"].as_str().unwrap_or_default());
    let (status, job) = call(&router, Method::GET, &job, None).await;
    assert_eq!((status, &job["state"]), (StatusCode::OK, &json!("pending")));
    assert_eq!(job["kind"], json!("broadcast"));
    let (status, listed) = call(
        &router,
        Method::GET,
        "/v1/jobs?kind=broadcast&state=pending",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["jobs"][0]["id"], job["id"]);
    let (status, _) = call(&router, Method::GET, "/v1/jobs?limit=0", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, assets) = call(&router, Method::GET, "/v1/assets", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(assets["assets"][0]["metadata"]["decimals"], 8);
    let (status, networks) = call(&router, Method::GET, "/v1/networks", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(networks["networks"][0]["nodes"], json!([]));
    let tip = |query: &'static str| {
        let uri = format!("/v1/networks/bitcoin/regtest/tip{query}");
        let router = router.clone();
        async move { call(&router, Method::GET, &uri, None).await.0 }
    };
    assert_eq!(tip("").await, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        tip("?at=2026-10-01T00:00:00Z").await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(tip("?at=yesterday").await, StatusCode::BAD_REQUEST);
    let (status, _) = call(&router, Method::GET, "/v1/networks/bitcoin/x/tip", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, health) = call(&router, Method::GET, "/health", None).await;
    assert_eq!((status, &health["ready"]), (StatusCode::OK, &json!(true)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_start_past_the_history_limit_is_refused_and_a_wallet_pauses_and_resumes() {
    let (storage, _directory) = database().await;
    let days = std::time::Duration::from_secs(30 * 86_400);
    let payments = limited(&storage, Arc::new(Script::default()), Some(days)).await;
    let state = Arc::new(AppState {
        payments,
        storage: storage.clone(),
        explorers: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = api::router(state, &config).expect("a router");
    let watch = |start: u64| json!({ "chain": "bitcoin", "network": "regtest", "address": "alice", "start": start });

    // Block 70 is about 30 days old.
    let (status, error) = call(&router, Method::POST, "/v1/wallets", Some(watch(60))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert!(error.to_string().contains("30 days"), "{error}");
    let (status, alice) = call(&router, Method::POST, "/v1/wallets", Some(watch(80))).await;
    assert_eq!(status, StatusCode::CREATED, "{alice}");
    let index = &alice["indexes"][0];
    assert_eq!(index["asset"], "native");
    assert_eq!(index["index_state"], "indexing");
    assert_eq!(index["index_reason"], Value::Null);
    assert_eq!(
        (&index["index_requested_from"], &index["indexed_from"]),
        (&json!(80), &json!(80))
    );
    assert_eq!(index["indexed_at"], Value::Null);
    let (_, networks) = call(&router, Method::GET, "/v1/networks", None).await;
    assert_eq!(networks["networks"][0]["max_history_days"], 30);

    let uri = "/v1/wallets/bitcoin:regtest:alice";
    let (status, paused) = call(&router, Method::POST, &format!("{uri}/pause"), None).await;
    assert_eq!(status, StatusCode::OK, "{paused}");
    assert_eq!(paused["indexes"][0]["index_state"], "paused");
    let (_, list) = call(&router, Method::GET, "/v1/wallets", None).await;
    assert_eq!(
        list["wallets"][0]["indexes"][0]["index_state"], "paused",
        "{list}"
    );
    let (status, resumed) = call(&router, Method::POST, &format!("{uri}/resume"), None).await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    let (_, read) = call(&router, Method::GET, uri, None).await;
    assert_eq!(read["indexes"][0]["index_state"], "indexing", "{read}");
    let (status, _) = call(
        &router,
        Method::POST,
        "/v1/wallets/bitcoin:regtest:nobody/pause",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
