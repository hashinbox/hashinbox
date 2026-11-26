//! The routes over the scripted network and a real SQLite file: create
//! (generate, import, watch), index (create, pause, resume, reconcile,
//! delete), tag,
//! list and read wallets; balance and legs of indexed blocks; a send
//! enqueued and read back as a job; assets, networks and health; the network
//! asset icons.

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
use rpc::Client as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::support::{
    as_root,
    db::seeded,
    scripted::{Script, container, limited, network},
};
use crate::{api, api::AppState};

/// The raw body of `GET uri`.
async fn text(router: &Router, uri: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("a request");
    let response = router.clone().oneshot(request).await.expect("a response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

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
        expires_height: None,
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
async fn wallets_balance_legs_and_sends() {
    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let state = Arc::new(AppState {
        container,
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db.clone()),
        actions: Arc::new(crate::usecase::workers::trigger::actions::registry()),
        logs: crate::usecase::logs::feed(),
        candles: Default::default(),
        root_exists: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = as_root(api::router(state, &config).expect("a router")).await;
    let create = |kind: Value| {
        let mut body = json!({ "chain": "bitcoin", "network": "regtest" });
        if let (Some(body), Value::Object(kind)) = (body.as_object_mut(), kind) {
            body.extend(kind);
        }
        let router = router.clone();
        async move { call(&router, Method::POST, "/v1/wallets", Some(body)).await }
    };
    let index = |id: &str, start: Value| {
        let (router, uri) = (router.clone(), format!("/v1/wallets/{id}/indexes"));
        let mut body = json!({ "asset": "native" });
        if let (Some(body), false) = (body.as_object_mut(), start.is_null()) {
            body.insert("start".to_owned(), start);
        }
        async move { call(&router, Method::POST, &uri, Some(body)).await }
    };

    let (status, generated) = create(json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{generated}");
    assert_eq!(generated["keyed"], true);
    assert_eq!(generated["indexes"], json!({}));
    let generated_id = generated["id"].as_str().unwrap_or_default().to_owned();
    let (status, index_of) = index(&generated_id, Value::Null).await;
    assert_eq!(status, StatusCode::CREATED, "{index_of}");
    assert_eq!(index_of["asset"], "native");
    let (status, watched) = create(json!({ "address": "alice" })).await;
    assert_eq!(status, StatusCode::CREATED, "{watched}");
    assert_eq!(watched["id"], "bitcoin.regtest.alice");
    assert_eq!(watched["keyed"], false);
    assert_eq!(watched["indexes"], json!({}));
    let (status, index_of) = index("bitcoin.regtest.alice", json!(0)).await;
    assert_eq!(status, StatusCode::CREATED, "{index_of}");
    assert_eq!(index_of["requested_from"], 0);
    // Asked again, the index keeps its first start.
    let (status, index_of) = index("bitcoin.regtest.alice", json!(3)).await;
    assert_eq!(
        (status, &index_of["requested_from"]),
        (StatusCode::OK, &json!(0))
    );
    let (status, list) = call(&router, Method::GET, "/v1/wallets", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["wallets"].as_array().map(Vec::len), Some(2));

    BlockStore::insert(
        &crate::usecase::sdk::Stores::new(db.clone(), Default::default()),
        &paid("alice", 150_000_000),
    )
    .await
    .expect("block 0 kept");
    let (status, alice) = call(
        &router,
        Method::GET,
        "/v1/wallets/bitcoin.regtest.alice",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{alice}");
    assert_eq!(alice["indexes"]["native"]["balance_pending"], "150000000");
    assert_eq!(alice["indexes"]["native"]["indexed_through"], 0);
    let (status, _) = call(
        &router,
        Method::PUT,
        "/v1/wallets/bitcoin.regtest.alice/tags/vip",
        None,
    )
    .await;
    assert!(status.is_success(), "{status}");
    let (status, list) = call(&router, Method::GET, "/v1/wallets", None).await;
    assert_eq!(status, StatusCode::OK);
    let alice = list["wallets"]
        .as_array()
        .and_then(|wallets| wallets.iter().find(|w| w["id"] == "bitcoin.regtest.alice"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(alice["tags"], json!(["vip"]), "{list}");
    let (_, tags) = call(&router, Method::GET, "/v1/wallets/tags", None).await;
    assert_eq!(tags, json!({ "tags": ["vip"], "next_cursor": null }));
    assert_eq!(
        alice["indexes"]["native"]["balance_pending"], "150000000",
        "{list}"
    );
    assert_eq!(generated["tags"], json!([]));
    let (status, legs) = call(
        &router,
        Method::GET,
        "/v1/wallets/bitcoin.regtest.alice/indexes/native/legs?limit=1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{legs}");
    assert_eq!(legs["legs"][0]["txid"], "t0");
    assert_eq!(legs["legs"][0]["state"], 1);
    assert_eq!(legs["legs"][0]["amount"], "150000000");
    assert_eq!(legs["next_cursor"], "0:1");
    let blocks = "/v1/networks/bitcoin/regtest/blocks";
    let (status, page) = call(
        &router,
        Method::GET,
        &format!("{blocks}/0/transactions?limit=1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        (&page["transactions"][0]["txid"], &page["next_cursor"]),
        (&json!("t0"), &json!("0:0:0:t0"))
    );
    let (status, _) = call(
        &router,
        Method::GET,
        &format!("{blocks}/1/transactions"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let send = json!({ "destination": "bob", "amount": "0.1" });
    let (status, _) = call(
        &router,
        Method::POST,
        "/v1/wallets/bitcoin.regtest.alice/transactions",
        Some(send.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let from = format!("/v1/wallets/{generated_id}/transactions");
    // One payment per send: a list of transfers is no body of it.
    let list = json!({ "transfers": [{ "destination": "bob", "amount": "0.1" }] });
    let (status, _) = call(&router, Method::POST, &from, Some(list)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A sign, or more than all bitcoin: refused before any job.
    for amount in ["+0.1", "21000000.00000001"] {
        let body = json!({ "destination": "bob", "amount": amount });
        let (status, body) = call(&router, Method::POST, &from, Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{amount}: {body}");
    }
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
    let network = &networks["networks"][0];
    assert_eq!(network["endpoints"], json!([]));
    assert_eq!(
        (&network["index"]["tip"], &network["tip"]["live"]),
        (&Value::Null, &Value::Null)
    );
    let endpoints = "/v1/networks/bitcoin/regtest/endpoints";
    let (status, listed) = call(&router, Method::GET, endpoints, None).await;
    assert_eq!((status, &listed["endpoints"]), (StatusCode::OK, &json!([])));
    let (status, _) = call(
        &router,
        Method::PUT,
        endpoints,
        Some(json!({ "endpoints": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&router, Method::DELETE, &format!("{endpoints}/0"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &router,
        Method::GET,
        "/v1/networks/bitcoin/x/endpoints",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&router, Method::GET, "/v1/networks/bitcoin/regtest", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, events) = text(&router, "/v1/networks/bitcoin/regtest/tip/search?at=1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        events.contains("event: error") && events.contains("cannot find blocks by time"),
        "{events}"
    );
    let (status, _) = call(
        &router,
        Method::GET,
        "/v1/networks/bitcoin/regtest/tip",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, health) = call(&router, Method::GET, "/health", None).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "no index yet: {health}"
    );
    db.write(|connection| {
        use diesel::connection::SimpleConnection as _;
        Ok(connection.batch_execute(
            "INSERT INTO network_index (chain, network, tip, confirmed)
             VALUES ('bitcoin', 'regtest', 5, 4);
             INSERT INTO network_endpoint (chain, network, url, position)
             VALUES ('bitcoin', 'regtest', 'http://ready.invalid/', 0)",
        )?)
    })
    .await
    .unwrap();
    let (status, health) = call(&router, Method::GET, "/health", None).await;
    assert_eq!((status, &health["ready"]), (StatusCode::OK, &json!(true)));
    assert_eq!(health["networks"][0]["indexing"]["tip"], 5, "{health}");

    let stored = json!([
        "http://a.invalid/",
        "https://u:p@b.invalid:8443/v2/K?key=1&header.x-api-key=K&rpc.max_batch=1"
    ]);
    let nodes = serde_json::from_value(stored.clone()).unwrap();
    let nodes = crate::models::RpcEndpoints::new(nodes).unwrap();
    let network = base::Network::new(base::Chain::new("bitcoin"), "regtest");
    db.write(move |connection| crate::models::endpoint::db::replace(connection, &network, &nodes))
        .await
        .unwrap();
    let urls = |nodes: &Value| -> Value {
        nodes
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["url"].clone())
            .collect()
    };
    let (status, listed) = call(&router, Method::GET, endpoints, None).await;
    assert_eq!(
        (status, urls(&listed["endpoints"])),
        (StatusCode::OK, stored.clone())
    );
    let (_, networks) = call(&router, Method::GET, "/v1/networks", None).await;
    assert_eq!(urls(&networks["networks"][0]["endpoints"]), stored);
    let (status, _) = call(&router, Method::DELETE, &format!("{endpoints}/99"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A JSON-RPC node on loopback answering every call with `answer`.
async fn answering(answer: u64) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let reply = move |axum::Json(call): axum::Json<Value>| async move {
        axum::Json(json!({ "jsonrpc": "2.0", "id": call["id"], "result": answer }))
    };
    let node = Router::new().route("/", axum::routing::post(reply));
    tokio::spawn(async move { axum::serve(listener, node).await });
    url
}

#[tokio::test(flavor = "multi_thread")]
async fn a_routed_method_alternates_its_nodes_and_routes_follow_their_nodes() {
    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let state = Arc::new(AppState {
        container: container.clone(),
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db.clone()),
        actions: Arc::new(crate::usecase::workers::trigger::actions::registry()),
        logs: crate::usecase::logs::feed(),
        candles: Default::default(),
        root_exists: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = as_root(api::router(state, &config).expect("a router")).await;
    let ask = |method: &'static str| {
        let container = container.clone();
        async move {
            let client = base::Transport::client(&*container.transport(&network()).unwrap());
            let raw = client
                .request(rpc::Request::new(method, json!([])))
                .await
                .unwrap();
            raw.deserialize::<u64>().unwrap()
        }
    };
    let endpoints = "/v1/networks/bitcoin/regtest/endpoints";
    let routes = "/v1/networks/bitcoin/regtest/routes";
    let nodes = [answering(0).await, answering(1).await, answering(2).await];
    let (status, put) = call(
        &router,
        Method::PUT,
        endpoints,
        Some(json!({ "endpoints": nodes })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let ids: Vec<i64> = put["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_i64().unwrap())
        .collect();
    assert_eq!(put["endpoints"][1]["url"], json!(nodes[1]));
    assert_eq!(put["endpoints"][1]["disabled"], Value::Null, "{put}");
    assert_eq!(
        ask("m").await,
        0,
        "unrouted: the highest tip, a tie: the first"
    );

    let route = json!({ "routes": [{ "method": "m", "endpoint_ids": [ids[1], ids[2]] }] });
    let (status, put) = call(&router, Method::PUT, routes, Some(route.clone())).await;
    assert_eq!((status, &put["routes"]), (StatusCode::OK, &route["routes"]));
    let mut answers = Vec::new();
    for _ in 0..4 {
        answers.push(ask("m").await);
    }
    assert_eq!(answers, [1, 2, 1, 2]);
    assert_eq!((ask("n").await, ask("n").await), (0, 0));

    for bad in [
        json!([{ "method": "", "endpoint_ids": [ids[0]] }]),
        json!([{ "method": "m", "endpoint_ids": [] }]),
        json!([{ "method": "m", "endpoint_ids": [ids[0], ids[0]] }]),
        json!([{ "method": "m", "endpoint_ids": [999] }]),
        json!([{ "method": "m", "endpoint_ids": [ids[0]] }, { "method": "m", "endpoint_ids": [ids[1]] }]),
    ] {
        let (status, _) = call(&router, Method::PUT, routes, Some(json!({ "routes": bad }))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let (_, listed) = call(&router, Method::GET, routes, None).await;
    assert_eq!(
        listed["routes"], route["routes"],
        "a refusal changes nothing"
    );

    let reordered = json!({ "endpoints": [nodes[2], nodes[0], nodes[1]] });
    let (status, put) = call(&router, Method::PUT, endpoints, Some(reordered)).await;
    assert_eq!(status, StatusCode::OK, "{put}");
    assert_eq!(
        put["endpoints"][0]["id"],
        json!(ids[2]),
        "a kept URL keeps its id"
    );
    let (_, listed) = call(&router, Method::GET, routes, None).await;
    assert_eq!(listed["routes"][0]["endpoint_ids"], json!([ids[2], ids[1]]));

    let (status, _) = call(
        &router,
        Method::DELETE,
        &format!("{endpoints}/{}", ids[1]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, listed) = call(&router, Method::GET, routes, None).await;
    assert_eq!(listed["routes"][0]["endpoint_ids"], json!([ids[2]]));
    assert_eq!((ask("m").await, ask("m").await), (2, 2));
    let (status, _) = call(
        &router,
        Method::DELETE,
        &format!("{endpoints}/{}", ids[2]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, networks) = call(&router, Method::GET, "/v1/networks", None).await;
    assert_eq!(
        networks["networks"][0]["routes"],
        json!([]),
        "removing a node removes its routes"
    );
    assert_eq!(ask("m").await, 0);
    let (_, networks) = call(&router, Method::GET, "/v1/networks", None).await;
    let state = &networks["networks"][0]["endpoints"][0];
    assert!(
        state["stats"]["calls"].as_u64() > Some(1),
        "kept across edits: {state}"
    );
    assert_eq!(state["stats"]["failed_calls"], 0, "{state}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_start_past_the_depth_limit_is_refused_and_an_index_pauses_resumes_and_goes() {
    let (_, db, _directory) = seeded().await;
    let container = limited(&db, Arc::new(Script::default()), Some(30));
    let state = Arc::new(AppState {
        container,
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db.clone()),
        actions: Arc::new(crate::usecase::workers::trigger::actions::registry()),
        logs: crate::usecase::logs::feed(),
        candles: Default::default(),
        root_exists: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = as_root(api::router(state, &config).expect("a router")).await;
    let (status, _) = call(
        &router,
        Method::POST,
        "/v1/wallets",
        Some(json!({ "chain": "bitcoin", "network": "regtest", "address": "alice" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let watch = |start: u64| json!({ "asset": "native", "start": start });
    let indexes = "/v1/wallets/bitcoin.regtest.alice/indexes";

    // The tip is 100: block 70 is the oldest start within 30 positions.
    let (status, error) = call(&router, Method::POST, indexes, Some(watch(69))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert!(error.to_string().contains("start at 70"), "{error}");
    for future in [101, u64::MAX] {
        let (status, error) = call(&router, Method::POST, indexes, Some(watch(future))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    }
    let (status, index) = call(&router, Method::POST, indexes, Some(watch(80))).await;
    assert_eq!(status, StatusCode::CREATED, "{index}");
    assert_eq!(index["asset"], "native");
    assert_eq!(index["state"], "indexing");
    assert_eq!(index["reason"], Value::Null);
    assert_eq!(
        (&index["requested_from"], &index["birthday"]),
        (&json!(80), &json!(80))
    );
    assert_eq!(index["indexed_through"], Value::Null);
    db.write(|connection| {
        use diesel::connection::SimpleConnection as _;
        Ok(connection.batch_execute("UPDATE network SET max_depth = 30")?)
    })
    .await
    .unwrap();
    let (_, networks) = call(&router, Method::GET, "/v1/networks", None).await;
    assert_eq!(networks["networks"][0]["max_depth"], 30);
    // No node told a tip yet, and none is stored: the list asks no node.
    assert_eq!(networks["networks"][0]["tip"]["live"], Value::Null);
    // Block p is 100 - p days old: 50.5 days ago is after block 49.
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs()
        - 101 * 43_200;
    let uri = format!("/v1/networks/bitcoin/regtest/tip/search?at={at}");
    let (status, events) = text(&router, &uri).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert!(events.contains("event: probe"), "{events}");
    let found = events
        .split("\n\n")
        .find(|event| event.starts_with("event: found"))
        .and_then(|event| event.split("data: ").nth(1))
        .and_then(|data| serde_json::from_str::<Value>(data).ok())
        .unwrap_or_default();
    assert_eq!(found["position"], 49, "{events}");
    let (status, _) = text(&router, "/v1/networks/bitcoin/regtest/tip/search?at=soon").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let uri = "/v1/wallets/bitcoin.regtest.alice";
    let native = format!("{uri}/indexes/native");
    let state = |index_state: &str| Some(json!({ "index_state": index_state }));
    let (status, paused) = call(&router, Method::PATCH, &native, state("paused")).await;
    assert_eq!(status, StatusCode::OK, "{paused}");
    assert_eq!(paused["state"], "paused");
    let (_, list) = call(&router, Method::GET, "/v1/wallets", None).await;
    assert_eq!(
        list["wallets"][0]["indexes"]["native"]["state"], "paused",
        "{list}"
    );
    let (status, resumed) = call(&router, Method::PATCH, &native, state("indexing")).await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    let (_, read) = call(&router, Method::GET, uri, None).await;
    assert_eq!(read["indexes"]["native"]["state"], "indexing", "{read}");
    let (status, _) = call(&router, Method::PATCH, &native, state("no_data")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // This chain reads no balance: nothing to reconcile with.
    let reconcile = format!("{native}/reconcile");
    let (status, error) = call(&router, Method::POST, &reconcile, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    let nobody = "/v1/wallets/bitcoin.regtest.nobody/indexes/native";
    let (status, _) = call(&router, Method::PATCH, nobody, state("paused")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&router, Method::DELETE, &native, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, read) = call(&router, Method::GET, uri, None).await;
    assert_eq!(read["indexes"], json!({}), "{read}");
    let (status, _) = call(&router, Method::DELETE, &native, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wallet_is_generated_imported_or_watched_then_indexed() {
    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let state = Arc::new(AppState {
        container,
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db.clone()),
        actions: Arc::new(crate::usecase::workers::trigger::actions::registry()),
        logs: crate::usecase::logs::feed(),
        candles: Default::default(),
        root_exists: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = as_root(api::router(state, &config).expect("a router")).await;
    let create = |kind: Value| {
        let mut body = json!({ "chain": "bitcoin", "network": "regtest" });
        if let (Some(body), Value::Object(kind)) = (body.as_object_mut(), kind) {
            body.extend(kind);
        }
        let router = router.clone();
        async move { call(&router, Method::POST, "/v1/wallets", Some(body)).await }
    };

    // The scripted chain's key text is its address text.
    let (status, bob) = create(json!({ "secret": "bob" })).await;
    assert_eq!(status, StatusCode::CREATED, "{bob}");
    assert_eq!(
        (&bob["id"], &bob["keyed"]),
        (&json!("bitcoin.regtest.bob"), &json!(true))
    );
    assert_eq!(bob["indexes"], json!({}));
    assert_eq!(bob["external_id"], Value::Null, "{bob}");

    // An external id is kept trimmed, read back, set once and never replaced:
    // another is refused, the same or none answers the wallet.
    let (status, frank) = create(json!({ "address": "frank", "external_id": " cus-1 " })).await;
    assert_eq!(
        (status, &frank["external_id"]),
        (StatusCode::CREATED, &json!("cus-1"))
    );
    let frank_uri = "/v1/wallets/bitcoin.regtest.frank";
    let (_, read) = call(&router, Method::GET, frank_uri, None).await;
    assert_eq!(read["external_id"], "cus-1", "{read}");
    let (status, error) = create(json!({ "address": "frank", "external_id": "cus-2" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    for again in [
        json!({ "address": "frank", "external_id": "cus-1" }),
        json!({ "address": "frank" }),
    ] {
        let (status, frank) = create(again).await;
        assert_eq!(
            (status, &frank["external_id"]),
            (StatusCode::OK, &json!("cus-1"))
        );
    }
    let (_, bob) = create(json!({ "address": "bob", "external_id": "cus-3" })).await;
    assert_eq!(bob["external_id"], "cus-3", "{bob}");
    for external_id in [String::new(), "  ".to_owned(), "x".repeat(129)] {
        let (status, error) =
            create(json!({ "address": "gina", "external_id": external_id })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    }
    let (status, _) = call(
        &router,
        Method::GET,
        "/v1/wallets/bitcoin.regtest.gina",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, error) = create(json!({ "secret": "carol", "address": "dave" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert!(error.to_string().contains("derives carol"), "{error}");
    for wrong in [
        json!({ "address": "erin", "assets": ["native"] }),
        json!({ "generate": {} }),
    ] {
        let (status, error) = create(wrong.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{wrong} {error}");
    }

    // A watched wallet, tagged and indexed, keeps both when its key comes.
    let (status, erin) = create(json!({ "address": "erin" })).await;
    assert_eq!(
        (status, &erin["keyed"]),
        (StatusCode::CREATED, &json!(false))
    );
    let erin_uri = "/v1/wallets/bitcoin.regtest.erin";
    let (status, _) = call(&router, Method::PUT, &format!("{erin_uri}/tags/vip"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, index) = call(
        &router,
        Method::POST,
        &format!("{erin_uri}/indexes"),
        Some(json!({ "asset": "native", "start": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{index}");
    assert_eq!(index["requested_from"], 0);
    let (status, erin) = create(json!({ "secret": "erin", "address": "erin" })).await;
    assert_eq!(status, StatusCode::OK, "{erin}");
    assert_eq!(
        (&erin["keyed"], &erin["indexes"]["native"]["asset"]),
        (&json!(true), &json!("native"))
    );
    // Watching a keyed address answers it as is.
    let (status, erin) = create(json!({ "address": "erin" })).await;
    assert_eq!((status, &erin["keyed"]), (StatusCode::OK, &json!(true)));
    let (_, list) = call(&router, Method::GET, "/v1/wallets", None).await;
    let listed = list["wallets"]
        .as_array()
        .and_then(|wallets| wallets.iter().find(|w| w["id"] == "bitcoin.regtest.erin"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(listed["tags"], json!(["vip"]), "{list}");
    let (status, _) = call(
        &router,
        Method::DELETE,
        &format!("{erin_uri}/tags/vip"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &router,
        Method::DELETE,
        &format!("{erin_uri}/tags/vip"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, tags) = call(&router, Method::GET, "/v1/wallets/tags", None).await;
    assert_eq!(tags, json!({ "tags": [], "next_cursor": null }));

    // Indexes need a kept wallet and an asset.
    let unknown = "/v1/wallets/bitcoin.regtest.nobody";
    let fresh = json!({ "asset": "native" });
    let (status, _) = call(
        &router,
        Method::POST,
        &format!("{unknown}/indexes"),
        Some(fresh),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&router, Method::PUT, &format!("{unknown}/tags/vip"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let other = json!({ "asset": "usdc" });
    let (status, _) = call(
        &router,
        Method::POST,
        &format!("{erin_uri}/indexes"),
        Some(other),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_network_asset_icon_is_served_and_an_unknown_one_is_not() {
    let router = crate::ui::Settings::from_params(crate::ui::SettingsParams {
        api_base: String::new(),
    })
    .routes()
    .expect("ui routes");
    let get = |uri: &str| {
        Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("a request")
    };
    let found = router
        .clone()
        .oneshot(get("/assets/crypto/eth_usdc.svg"))
        .await
        .expect("a response");
    assert_eq!(found.status(), StatusCode::OK);
    assert_eq!(found.headers()["content-type"], "image/svg+xml");
    let missing = router
        .oneshot(get("/assets/crypto/eth_doge.svg"))
        .await
        .expect("a response");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_index_step_leaves_its_reason_on_the_job_and_health_not_ready() {
    use diesel::connection::SimpleConnection as _;
    use tasks::{Admission, Store as _, Task, Worker as _, testing::claim};

    use crate::usecase::workers::index::{Index, Target};

    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let worker = Arc::new(Index::new(container.clone(), db.clone(), queue.clone()));
    let target = Target {
        chain: "bitcoin".into(),
        network: "regtest".into(),
    };
    let queued = Index::enqueue(&queue, &target).await.expect("queued");
    let Admission::Queued(id) = queued else {
        panic!("the first index job");
    };
    // The block store fails, so every indexer step does.
    db.write(|connection| Ok(connection.batch_execute("DROP TABLE block")?))
        .await
        .expect("dropped");
    let now = tasks::now();
    let job = claim(&queue, Index::KIND, 1, now).await.remove(0);
    let mut task = Task::new(worker, job, Arc::new(queue.clone()));
    task.run().await;
    task.update().await.expect("settled");
    let job = queue.get(id).await.expect("a read").expect("the job");
    let reason = job.reason.clone().unwrap_or_default();
    assert!(reason.contains("no such table: block"), "{job:?}");
    assert_eq!(job.status, tasks::Status::Pending);
    // Retried by the queue, which backs off: the failure counts as an
    // attempt, and the next one waits for it (1 s per failure in a row).
    assert_eq!(job.attempts, 1, "{job:?}");
    assert!(job.run_at >= now + 1_000, "{job:?}");

    let state = Arc::new(AppState {
        container,
        db: db.clone(),
        queue,
        actions: Arc::new(crate::usecase::workers::trigger::actions::registry()),
        logs: crate::usecase::logs::feed(),
        candles: Default::default(),
        root_exists: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = as_root(api::router(state, &config).expect("a router")).await;
    let (status, health) = call(&router, Method::GET, "/health", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{health}");
    assert_eq!(health["ready"], json!(false));
    // Failed before its first tip: no row written yet, only why.
    let indexing = &health["networks"][0]["indexing"];
    assert_eq!(indexing["tip"], Value::Null, "{health}");
    assert_eq!(indexing["job_reason"], json!(reason), "{health}");
    let (status, networks) = call(&router, Method::GET, "/v1/networks", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        networks["networks"][0]["index"]["job_reason"],
        json!(reason)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trigger_run_shows_its_jobs_and_how_they_ended() {
    use diesel::connection::SimpleConnection as _;

    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    // One run that queued two sends: one succeeded, one failed.
    db.write(|connection| {
        Ok(connection.batch_execute(
            r#"INSERT INTO q (id, topic, kind, status, run_at, attempts, reason, input,
               created_at, updated_at) VALUES
             (7,'n.a','broadcast',2,0,1,NULL,'{}',0,0),
             (8,'n.b','broadcast',3,0,1,'insufficient-funds','{}',0,0);
             INSERT INTO "trigger" (id, chain, network, group_id, selector, "check", action,
               enabled, created_at, updated_at) VALUES
             ('t','bitcoin','regtest','g','{"asset":"native"}','{"all":[]}',
              '{"name":"send","input":{}}',1,0,0);
             INSERT INTO trigger_run (id, "trigger", started_at, jobs, stats)
               VALUES ('t.5','t',5,'["7","8"]',
                '{"selected":3,"kept":2,"checked":true,"jobs":2}');"#,
        )?)
    })
    .await
    .expect("seeded");
    let state = Arc::new(AppState {
        container,
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db),
        actions: Arc::new(crate::usecase::workers::trigger::actions::registry()),
        logs: crate::usecase::logs::feed(),
        candles: Default::default(),
        root_exists: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = as_root(api::router(state, &config).expect("a router")).await;
    let (status, page) = call(&router, Method::GET, "/v1/triggers/t/runs", None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let run = &page["runs"][0];
    assert_eq!(run["started_at"], json!(5));
    assert_eq!(run["outcome"], json!("mixed"));
    assert_eq!(
        run["stats"],
        json!({"selected": 3, "kept": 2, "checked": true, "jobs": 2})
    );
    assert_eq!(
        run["jobs"],
        json!([
            {"id": "7", "state": "succeeded", "reason": null},
            {"id": "8", "state": "failed", "reason": "insufficient-funds"},
        ])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_count_compares_with_a_whole_number_and_a_put_keeps_enabled() {
    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let state = Arc::new(AppState {
        container,
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db),
        actions: Arc::new(crate::usecase::workers::trigger::actions::registry()),
        logs: crate::usecase::logs::feed(),
        candles: Default::default(),
        root_exists: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = as_root(api::router(state, &config).expect("a router")).await;
    // A sweep of regtest's native indexes (8 decimals), gated by `check`.
    let create = |id: &str, check: Value| {
        json!({"triggers": [{
            "id": id,
            "chain": "bitcoin",
            "network": "regtest",
            "selector": {"asset": "native"},
            "check": check,
            "action": {"name": "send", "input": {
                "source": {"this": {}},
                "asset": "native",
                "destination": {"select": {"ids": ["bitcoin.regtest.x.native"]}},
                "amount": {"balance": {}},
            }},
        }]})
    };
    let compare = |of: Value, aggregate: &str, value: &str| {
        json!({"compare": {
            "of": of,
            "aggregate": aggregate,
            "operator": "above",
            "value": {"value": value},
        }})
    };
    let select = || json!({"select": {"asset": "native"}});
    let half = create("half", compare(select(), "count", "1.5"));
    let (status, body) = call(&router, Method::POST, "/v1/triggers", Some(half)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("whole number"), "{body}");
    let fine = create("fine", compare(select(), "count", "2"));
    let (status, body) = call(&router, Method::POST, "/v1/triggers", Some(fine)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["triggers"][0]["enabled"], json!(true), "absent: on");

    // A PUT without `enabled` keeps a paused trigger paused.
    let mut definition = create("x", compare(select(), "count", "2"))["triggers"][0].clone();
    definition.as_object_mut().expect("an object").remove("id");
    let mut paused = definition.clone();
    paused["enabled"] = json!(false);
    let (status, body) = call(&router, Method::PUT, "/v1/triggers/fine", Some(paused)).await;
    assert_eq!((status, &body["enabled"]), (StatusCode::OK, &json!(false)));
    let (status, body) = call(&router, Method::PUT, "/v1/triggers/fine", Some(definition)).await;
    assert_eq!((status, &body["enabled"]), (StatusCode::OK, &json!(false)));
    let (_, body) = call(&router, Method::GET, "/v1/triggers/fine", None).await;
    assert_eq!(body["enabled"], json!(false), "{body}");
}

/// On a chain that reads balances: an index opens at the chain's final
/// block, its balances null until a step settles it there; a manual
/// reconcile (once the indexer read the final block) deletes its legs above
/// that block and unsets its progress (no balance, no trigger), and the
/// next steps settle it again and read on; a paused index is not
/// reconciled.
#[tokio::test(flavor = "multi_thread")]
async fn a_reconciled_index_reads_null_skips_triggers_and_settles_again() {
    use base::testing::{
        Held, Served,
        fixtures::{block, payment},
        idle,
    };

    let (_, db, _directory) = seeded().await;
    let held = Arc::new(Held::new(network()));
    // Blocks 0..=5, final through 3; alice is paid 0.5 at 5.
    let network = network();
    let mut chain: Vec<_> = (0..=5).map(|p| block(&network, p, 0, vec![])).collect();
    let paid = payment(&network, "a", "alice", 500_000_000, None);
    chain[5] = block(&network, 5, 0, vec![paid]);
    let script = Script {
        balances: Some(held.clone()),
        chain: Some(Served::new(chain, 2)),
        ..Script::default()
    };
    let container = container(&db, Arc::new(script));
    let indexer = container
        .service(&network)
        .expect("a service")
        .indexer
        .clone();
    let state = Arc::new(AppState {
        container,
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db.clone()),
        actions: Arc::new(crate::usecase::workers::trigger::actions::registry()),
        logs: crate::usecase::logs::feed(),
        candles: Default::default(),
        root_exists: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = as_root(api::router(state, &config).expect("a router")).await;
    let alice = json!({ "chain": "bitcoin", "network": "regtest", "secret": "alice" });
    let (status, _) = call(&router, Method::POST, "/v1/wallets", Some(alice)).await;
    assert_eq!(status, StatusCode::CREATED);
    let wallet = "/v1/wallets/bitcoin.regtest.alice";
    let indexes = format!("{wallet}/indexes");
    let (status, index) = call(
        &router,
        Method::POST,
        &indexes,
        Some(json!({"asset": "native", "start": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{index}");
    assert_eq!(index["state"], "indexing", "{index}");
    assert_eq!(
        (&index["requested_from"], &index["birthday"]),
        (&json!(1), &json!(3)),
        "{index}"
    );
    assert_eq!(index["balance_pending"], Value::Null, "{index}");
    let reconcile = format!("{indexes}/native/reconcile");
    let (status, error) = call(&router, Method::POST, &reconcile, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{error}");

    // The chain reads 5 at 3: it opens at 5, and reads on.
    held.set("alice", Some((3, 5_000_000_000)));
    idle(&indexer).await;
    let read = || async {
        let (_, wallet) = call(&router, Method::GET, wallet, None).await;
        wallet["indexes"]["native"].clone()
    };
    let index = read().await;
    assert_eq!(index["indexed_through"], 5, "{index}");
    assert_eq!(index["balance_pending"], "5500000000", "{index}");
    // What a trigger round reads: the fully indexed indexes.
    let fully = || {
        db.read(|connection| {
            let network = super::support::scripted::network();
            let alice = base::Address::new(network.clone(), "alice");
            let wanted = [(alice, base::Asset::native(network.clone()))];
            crate::models::wallet_index::db::find(connection, &network, &wanted, 0)
        })
    };
    assert_eq!(fully().await.expect("a read").len(), 1);

    // At 4 the chain says 5.2, 0.2 more than its legs (a contract paid it):
    // reconciled, its leg above 3 deleted.
    held.set("alice", Some((4, 5_200_000_000)));
    for _ in 0..2 {
        let (status, index) = call(&router, Method::POST, &reconcile, None).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{index}");
        assert_eq!(index["state"], "indexing", "{index}");
        assert_eq!(index["indexed_through"], Value::Null, "{index}");
        assert_eq!(index["balance_pending"], Value::Null, "{index}");
    }
    assert!(
        fully().await.expect("a read").is_empty(),
        "triggers skip it"
    );
    idle(&indexer).await;
    let index = read().await;
    assert_eq!(index["indexed_through"], 5, "{index}");
    assert_eq!(index["balance_pending"], "5700000000", "{index}");
    // Latest first, two a page: the cursor is the (position, seq) pair.
    let legs = format!("{indexes}/native/legs?limit=2");
    let (_, first) = call(&router, Method::GET, &legs, None).await;
    assert_eq!(first["next_cursor"], "4:2", "{first}");
    let next = format!("{legs}&cursor=4:2");
    let (_, second) = call(&router, Method::GET, &next, None).await;
    assert_eq!(second["next_cursor"], Value::Null, "{second}");
    let shown: Vec<(Value, Value, Value)> = [&first, &second]
        .iter()
        .flat_map(|page| page["legs"].as_array().cloned().unwrap_or_default())
        .map(|leg| {
            (
                leg["position"].clone(),
                leg["kind"].clone(),
                leg["amount"].clone(),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            (json!(5), json!(0), json!("500000000")),
            (json!(4), json!(6), json!("200000000")),
            (json!(3), json!(6), json!("5000000000")),
        ]
    );

    let native = format!("{indexes}/native");
    let paused = Some(json!({ "index_state": "paused" }));
    let (status, _) = call(&router, Method::PATCH, &native, paused).await;
    assert_eq!(status, StatusCode::OK);
    let (status, error) = call(&router, Method::POST, &reconcile, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    let resume = Some(json!({ "index_state": "indexing" }));
    let (_, index) = call(&router, Method::PATCH, &native, resume).await;
    assert_eq!(index["state"], "indexing", "{index}");
}

/// After a jump without balances (design 083 §10, [`jumped`]), the legs
/// route shows `archived_at` on each coin the jump archived, null on every
/// other leg.
///
/// [`jumped`]: super::storage::jumped
#[tokio::test(flavor = "multi_thread")]
async fn the_legs_route_shows_when_a_jump_archived_a_coin() {
    let (_, db, _directory) = seeded().await;
    let state = Arc::new(AppState {
        container: container(&db, Arc::new(Script::default())),
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db.clone()),
        actions: Arc::new(crate::usecase::workers::trigger::actions::registry()),
        logs: crate::usecase::logs::feed(),
        candles: Default::default(),
        root_exists: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = as_root(api::router(state, &config).expect("a router")).await;
    super::storage::jumped(&crate::usecase::sdk::Stores::new(
        db.clone(),
        Default::default(),
    ))
    .await;
    let legs = "/v1/wallets/bitcoin.regtest.alice/indexes/native/legs";
    let (status, page) = call(&router, Method::GET, legs, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let shown: Vec<(Value, Value, Value)> = page["legs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|leg| {
            (
                leg["position"].clone(),
                leg["kind"].clone(),
                leg["archived_at"].clone(),
            )
        })
        .collect();
    let leg =
        |position: u64, kind: u64, archived_at: Value| (json!(position), json!(kind), archived_at);
    assert_eq!(
        shown,
        [
            leg(950, 0, Value::Null),
            leg(950, 4, Value::Null),
            leg(950, 1, Value::Null),
            leg(850, 6, Value::Null),
            leg(840, 0, json!(900)),
            leg(800, 0, json!(900)),
        ]
    );
}

/// The routes over `container` and `db`, sends queued in `queue`.
async fn app(
    container: &Arc<base::Container<crate::usecase::Transport>>,
    db: &crate::db::Db,
    queue: &crate::usecase::tasks::Queue,
) -> Router {
    let state = Arc::new(AppState {
        container: container.clone(),
        db: db.clone(),
        queue: queue.clone(),
        actions: Arc::new(crate::usecase::workers::trigger::actions::registry()),
        logs: crate::usecase::logs::feed(),
        candles: Default::default(),
        root_exists: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    as_root(api::router(state, &config).expect("a router")).await
}

/// `worker`'s job of `input` enqueued, claimed, run once and settled: the
/// job as stored after.
async fn run_once<W: tasks::Worker + 'static>(
    queue: &crate::usecase::tasks::Queue,
    worker: W,
    input: &W::Input,
) -> tasks::Job {
    use tasks::{Admission, Store as _, Task, testing::claim};

    // Queued now, or already (an enabled node queues its network's index job).
    let (Admission::Queued(id) | Admission::Duplicate(id)) =
        W::enqueue(queue, input).await.expect("queued");
    let job = claim(queue, W::KIND, 1, tasks::now()).await.remove(0);
    let mut task = Task::new(Arc::new(worker), job, Arc::new(queue.clone()));
    task.run().await;
    task.update().await.expect("settled");
    queue.get(id).await.expect("a read").expect("the job")
}

/// Stores `urls` as the scripted network's nodes, loaded into its
/// transport; their ids, in order.
async fn stored(
    container: &Arc<base::Container<crate::usecase::Transport>>,
    db: &crate::db::Db,
    urls: Vec<String>,
) -> Vec<i64> {
    let nodes = urls.into_iter().map(rpc::Endpoint::new).collect();
    let nodes = crate::models::RpcEndpoints::new(nodes).unwrap();
    db.write(move |connection| {
        crate::models::endpoint::db::replace(connection, &network(), &nodes)
    })
    .await
    .unwrap();
    let networks = crate::usecase::Networks::new(db.clone()).await.unwrap();
    networks.load(&network(), container).unwrap();
    let stored = db.read(|connection| crate::models::network::db::find(connection, &network()));
    stored.await.unwrap().expect("the network").endpoints.ids()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_is_disabled_and_enabled_and_a_network_with_none_idles() {
    use tasks::{Admission, Worker as _};

    use crate::usecase::workers::index::{Index, Target};

    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    let ask = |method: &'static str| {
        let container = container.clone();
        async move {
            let client = base::Transport::client(&*container.transport(&network()).unwrap());
            let raw = client.request(rpc::Request::new(method, json!([])));
            raw.await.unwrap().deserialize::<u64>().unwrap()
        }
    };
    let endpoints = "/v1/networks/bitcoin/regtest/endpoints";
    let nodes = [answering(0).await, answering(1).await];
    let put = json!({ "endpoints": nodes });
    let (status, put) = call(&router, Method::PUT, endpoints, Some(put)).await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let ids: Vec<i64> = (0..2)
        .map(|i| put["endpoints"][i]["id"].as_i64().unwrap())
        .collect();
    assert_eq!(put["endpoints"][0]["disabled"], Value::Null);
    assert_eq!(ask("m").await, 0);

    let state = |id: i64| format!("{endpoints}/{id}/state");
    let disable = json!({ "state": "disabled", "reason": "flaky" });
    let (status, put) = call(&router, Method::PUT, &state(ids[0]), Some(disable)).await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let first = &put["endpoints"][0];
    assert_eq!(first["disabled"]["kind"], "manual", "the view: its record");
    assert_eq!(first["disabled"]["message"], "flaky", "the view: why");
    let failures = |id: i64| format!("{endpoints}/{id}/failures");
    let (_, listed) = call(&router, Method::GET, &failures(ids[0]), None).await;
    assert_eq!(listed["failures"][0]["kind"], "manual", "{listed}");
    assert_eq!(listed["failures"][0]["message"], "flaky");
    assert_eq!(ask("m").await, 1, "a disabled node gets no call");
    let route = json!({ "routes": [{ "method": "m", "endpoint_ids": [ids[0]] }] });
    let routes = "/v1/networks/bitcoin/regtest/routes";
    let (status, _) = call(&router, Method::PUT, routes, Some(route)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ask("m").await, 1, "its route skips it");

    let enable = json!({ "state": "enabled", "reason": "ignored" });
    let (status, put) = call(&router, Method::PUT, &state(ids[0]), Some(enable)).await;
    assert_eq!(status, StatusCode::OK, "{put}");
    let first = &put["endpoints"][0];
    assert_eq!(first["disabled"], Value::Null, "the view: none, enabled");
    let (_, listed) = call(&router, Method::GET, &failures(ids[0]), None).await;
    let kinds: Vec<&Value> = listed["failures"].as_array().unwrap().iter().collect();
    assert_eq!(kinds.len(), 2, "the enable recorded: {listed}");
    assert_eq!(listed["failures"][0]["message"], "ignored");
    assert_eq!(ask("m").await, 0, "routed to it again");
    let bad = json!({ "state": "off" });
    let (status, _) = call(&router, Method::PUT, &state(ids[0]), Some(bad)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let off = json!({ "state": "disabled" });
    let (status, _) = call(&router, Method::PUT, &state(999), Some(off.clone())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    for id in &ids {
        let (status, _) = call(&router, Method::PUT, &state(*id), Some(off.clone())).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (_, networks) = call(&router, Method::GET, "/v1/networks", None).await;
    assert_eq!(enabled(&networks["networks"][0]), 0, "{networks}");
    let (_, health) = call(&router, Method::GET, "/health", None).await;
    assert_eq!(health["networks"][0]["enabled_endpoints"], 0, "{health}");

    let target = Target {
        chain: "bitcoin".into(),
        network: "regtest".into(),
    };
    let worker = Index::new(container.clone(), db.clone(), queue.clone());
    let job = run_once(&queue, worker, &target).await;
    assert_eq!(job.status, tasks::Status::Idle, "stopped: {job:?}");
    assert_eq!(job.reason, None, "{job:?}");
    let (status, health) = call(&router, Method::GET, "/health", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{health}");

    // Enabling a node queues the network's index job again.
    let enable = json!({ "state": "enabled" });
    let (status, _) = call(&router, Method::PUT, &state(ids[0]), Some(enable)).await;
    assert_eq!(status, StatusCode::OK);
    let Admission::Duplicate(again) = Index::enqueue(&queue, &target).await.unwrap() else {
        panic!("the enable queued no index job");
    };
    assert_ne!(again, job.id, "a new job");
}

/// A node on loopback answering every request with `reply` of its body:
/// an HTTP status and a JSON body.
async fn replying(reply: fn(&Value) -> (u16, Value)) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let reply = move |axum::Json(body): axum::Json<Value>| async move {
        let (status, answer) = reply(&body);
        (StatusCode::from_u16(status).unwrap(), axum::Json(answer))
    };
    let node = Router::new().route("/", axum::routing::post(reply));
    tokio::spawn(async move { axum::serve(listener, node).await });
    url
}

/// Each call of a JSON-RPC batch answered with an error object 429.
fn limited_batch(body: &Value) -> Value {
    let answers = body.as_array().unwrap().iter().map(|call| {
        let error = json!({"code": 429, "message": "Too many requests"});
        json!({"jsonrpc": "2.0", "id": call["id"], "error": error})
    });
    Value::Array(answers.collect())
}

/// The scripted network's stored nodes, as the API lists them.
async fn listed(router: &Router) -> Vec<Value> {
    let endpoints = "/v1/networks/bitcoin/regtest/endpoints";
    let (_, listed) = call(router, Method::GET, endpoints, None).await;
    listed["endpoints"].as_array().unwrap().clone()
}

/// Node `id`'s records, newest first, as the API lists them.
async fn records(router: &Router, id: i64) -> Vec<Value> {
    let at = format!("/v1/networks/bitcoin/regtest/endpoints/{id}/failures");
    let (_, listed) = call(router, Method::GET, &at, None).await;
    listed["failures"].as_array().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_401_disables_the_node_in_the_store_and_its_calls_move_on() {
    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    let refusing = replying(|_| (401, json!({"error": "unauthorized"}))).await;
    let ids = stored(&container, &db, vec![refusing, answering(7).await]).await;
    let transport = container.transport(&network()).unwrap();
    let answer = transport.request(rpc::Request::new("m", json!([]))).await;
    assert_eq!(answer.unwrap().deserialize::<u64>().unwrap(), 7, "moved on");

    let nodes = listed(&router).await;
    let disabled = &nodes[0]["disabled"];
    assert_eq!(disabled["kind"], "http_401", "stored at once: {nodes:?}");
    assert_eq!(disabled["message"], "the node answered HTTP 401");
    let history = records(&router, ids[0]).await;
    assert_eq!(history.len(), 1, "{history:?}");
    assert_eq!(history[0]["kind"], "http_401");
    let running = transport.clients();
    assert_eq!(running.len(), 1, "refused at once, before any load");
    let answer = transport.request(rpc::Request::new("m", json!([]))).await;
    assert_eq!(answer.unwrap().deserialize::<u64>().unwrap(), 7);
    assert_eq!(records(&router, ids[0]).await.len(), 1, "disabled once");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_5xx_or_a_method_not_on_the_plan_disables_the_node_and_calls_move_on() {
    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    let unavailable = replying(|_| (503, json!({}))).await;
    let unpaid = replying(|body| {
        let error = json!({
            "code": -16401,
            "message": "Method 'getBalance' is available for paid plans only."
        });
        (
            200,
            json!({"jsonrpc": "2.0", "id": body["id"], "error": error}),
        )
    })
    .await;
    let ids = stored(
        &container,
        &db,
        vec![unavailable, unpaid, answering(7).await],
    )
    .await;
    let transport = container.transport(&network()).unwrap();
    let answer = transport
        .request(rpc::Request::new("getBalance", json!([])))
        .await;
    assert_eq!(answer.unwrap().deserialize::<u64>().unwrap(), 7, "moved on");

    let nodes = listed(&router).await;
    assert_eq!(nodes[0]["disabled"]["kind"], "http_503", "{nodes:?}");
    assert_eq!(
        nodes[0]["disabled"]["message"],
        "the node answered HTTP 503"
    );
    let message = &nodes[1]["disabled"]["message"];
    let refused = "the node refused a method as not on its plan (error code -16401)";
    assert_eq!(message, refused, "{nodes:?}");
    assert_eq!(nodes[2]["disabled"], Value::Null, "{nodes:?}");
    assert_eq!(records(&router, ids[0]).await.len(), 1);
    assert_eq!(transport.clients().len(), 1, "both off at once");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_of_another_chain_is_disabled_by_the_probe() {
    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    let ids = stored(&container, &db, vec![answering(1).await]).await;
    let networks = db.read(crate::models::network::db::find_all).await.unwrap();
    let stored = networks.iter().find(|n| n.key() == network()).unwrap();
    let mut config = base::NetworkConfig::from(stored);
    config.chain_id = Some("elsewhere".to_owned());
    let transport = crate::usecase::Transport::new::<base::testing::Unasked>(&config, db.clone());
    transport.load(&stored.endpoints, &[]).unwrap();
    let failed = transport.request(rpc::Request::new("m", json!([]))).await;
    assert!(failed.is_err(), "its only node, off");
    assert!(transport.has_no_node());

    let nodes = listed(&router).await;
    assert_eq!(nodes[0]["disabled"]["kind"], "wrong_chain", "{nodes:?}");
    let history = records(&router, ids[0]).await;
    assert_eq!(history.len(), 1, "{history:?}");
    assert_eq!(history[0]["kind"], "wrong_chain");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_429_passes_through_and_is_no_record() {
    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    let limiting = replying(|body| {
        if body.is_array() {
            return (200, limited_batch(body));
        }
        (429, json!({}))
    })
    .await;
    let ids = stored(&container, &db, vec![limiting]).await;
    let transport = container.transport(&network()).unwrap();
    let failed = transport.request(rpc::Request::new("m", json!([]))).await;
    let kind = failed.unwrap_err().kind();
    assert_eq!(kind, Some(rpc::ErrorKind::HttpStatus(429)), "untouched");
    let calls = vec![
        rpc::Call::new("a", json!([])),
        rpc::Call::new("a", json!([])),
    ];
    let answers = transport.batch(rpc::Batch::new(calls).unwrap()).await;
    assert!(answers.unwrap().iter().all(Result::is_err), "answered: 200");

    assert_eq!(listed(&router).await[0]["disabled"], Value::Null);
    assert!(records(&router, ids[0]).await.is_empty(), "no record");
    assert_eq!(transport.clients().len(), 1, "still in use");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nodes_counter_writes_its_calls_and_the_view_shows_them() {
    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    let node = replying(|body| {
        if body["method"] == "busy" {
            return (429, json!({}));
        }
        (
            200,
            json!({"jsonrpc": "2.0", "id": body["id"], "result": 1}),
        )
    })
    .await;
    stored(&container, &db, vec![node]).await;
    let transport = container.transport(&network()).unwrap();
    for method in ["a", "b", "busy"] {
        let _answer = transport
            .request(rpc::Request::new(method, json!([])))
            .await;
    }
    // Each count lands on its own task, after its call answered.
    let node = super::support::until("the three calls counted", || async {
        let node = listed(&router).await[0].clone();
        if node["stats"]["calls"] == 3 {
            Ok(node)
        } else {
            Err(node.to_string())
        }
    })
    .await;
    assert_eq!(node["stats"]["failed_calls"], 1, "{node}");
    assert_eq!(
        node["disabled"],
        Value::Null,
        "a 429 disables nothing: {node}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_node_disabled_by_its_guard_idles_the_index_job() {
    use crate::usecase::workers::index::{Index, Target};

    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    let refusing = replying(|_| (403, json!({"error": "forbidden"}))).await;
    stored(&container, &db, vec![refusing]).await;
    let transport = container.transport(&network()).unwrap();
    let failed = transport.request(rpc::Request::new("m", json!([]))).await;
    assert!(failed.is_err());
    assert!(transport.has_no_node(), "refused at once");
    let target = Target {
        chain: "bitcoin".into(),
        network: "regtest".into(),
    };
    let worker = Index::new(container.clone(), db.clone(), queue.clone());
    let job = run_once(&queue, worker, &target).await;
    assert_eq!(job.status, tasks::Status::Idle, "stopped: {job:?}");
    assert_eq!(job.reason, None, "{job:?}");
    let (status, health) = call(&router, Method::GET, "/health", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{health}");

    let networks = crate::usecase::Networks::new(db.clone()).await.unwrap();
    networks.load(&network(), &container).unwrap();
    assert!(transport.has_no_node(), "the next load leaves it out");
    let (_, networks) = call(&router, Method::GET, "/v1/networks", None).await;
    assert_eq!(enabled(&networks["networks"][0]), 0, "{networks}");
}

/// How many of `network`'s nodes, as the API lists it, are enabled.
fn enabled(network: &Value) -> usize {
    let nodes = network["endpoints"].as_array().unwrap().iter();
    nodes.filter(|node| node["disabled"].is_null()).count()
}

/// A subscriber keeping every event from debug, with its spans' fields, in
/// `db`'s `log` table, published to `feed`.
pub(super) fn subscriber(
    db: &crate::db::Db,
    feed: crate::usecase::logs::Feed,
) -> tracing::Dispatch {
    use tracing_subscriber::{Layer as _, layer::SubscriberExt as _};

    // With one dispatcher alive, `tracing` judges a callsite first hit on
    // another test's thread by that thread's (none) and caches `never`; a
    // second one kept alive makes it ask every dispatcher.
    static SECOND: std::sync::LazyLock<tracing::Dispatch> = std::sync::LazyLock::new(|| {
        tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default())
    });
    std::sync::LazyLock::force(&SECOND);
    let debug = tracing::level_filters::LevelFilter::DEBUG;
    let layer = crate::usecase::logs::DbLayer::new(db.clone(), feed).with_filter(debug);
    tracing::Dispatch::new(tracing_subscriber::registry().with(layer))
}

/// The entries at `at` once `count` are kept, newest first.
async fn kept(router: &Router, at: &str, count: usize) -> Vec<Value> {
    super::support::until("the entries stored", || async {
        let (_, listed) = call(router, Method::GET, at, None).await;
        let logs = listed["logs"].as_array().cloned().unwrap_or_default();
        (logs.len() >= count)
            .then_some(logs)
            .ok_or(listed.to_string())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn rpc_http_carries_its_fields_and_the_network_and_node_spans() {
    use tracing::instrument::WithSubscriber as _;

    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    let node = replying(|body| {
        if body.is_array() {
            return (200, limited_batch(body));
        }
        if body["method"] == "busy" {
            return (429, json!({}));
        }
        (
            200,
            json!({"jsonrpc": "2.0", "id": body["id"], "result": 7}),
        )
    })
    .await;
    let ids = stored(&container, &db, vec![node]).await;
    let transport = container.transport(&network()).unwrap();
    async {
        let fine = transport.request(rpc::Request::new("fine", json!(["secret"])));
        fine.await.unwrap();
        let busy = transport.request(rpc::Request::new("busy", json!([])));
        busy.await.unwrap_err();
        let calls = vec![
            rpc::Call::new("a", json!([])),
            rpc::Call::new("a", json!([])),
        ];
        let batch = transport.batch(rpc::Batch::new(calls).unwrap());
        batch.await.unwrap();
    }
    .with_subscriber(subscriber(&db, crate::usecase::logs::feed()))
    .await;

    let at = "/v1/logs?network=bitcoin/regtest&target=rpc.http.answered,rpc.http.failed";
    let mut http = kept(&router, at, 3).await;
    http.reverse();
    assert_eq!(http.len(), 3, "{http:?}");
    let node = ids[0].to_string();
    for entry in &http {
        assert_eq!(
            entry["fields"]["network"], "bitcoin/regtest",
            "the node span"
        );
        assert_eq!(entry["fields"]["node"], json!(node));
        for timing in ["ms", "wait_ms"] {
            let shown = entry["fields"][timing].as_str().unwrap_or_default();
            assert!(shown.parse::<u64>().is_ok(), "{timing}: {entry}");
        }
        assert!(!entry.to_string().contains("secret"), "no params: {entry}");
    }
    let keys = ["method", "batch", "status", "rpc_errors", "error"];
    let shown = |at: usize| keys.map(|key| http[at]["fields"][key].as_str().unwrap().to_owned());
    assert_eq!(
        (&http[0]["level"], &http[0]["target"], &http[0]["message"]),
        (&json!("debug"), &json!("rpc.http.answered"), &json!(null)),
        "no message: the template's"
    );
    assert_eq!(shown(0), ["fine", "-", "200", "-", "-"].map(str::to_owned));
    assert_eq!(
        (&http[1]["level"], &http[1]["target"]),
        (&json!("warn"), &json!("rpc.http.failed"))
    );
    assert_eq!(
        shown(1),
        ["busy", "-", "429", "-", "http_429"].map(str::to_owned)
    );
    assert_eq!(http[2]["level"], "warn");
    assert_eq!(
        shown(2),
        ["a", "2", "200", "429,429", "-"].map(str::to_owned)
    );
}

/// reth and Erigon answer `eth_getLogs` past their head with `-32602`:
/// the node is not there yet, not failing.
#[tokio::test(flavor = "multi_thread")]
async fn a_range_beyond_the_head_is_debug_and_not_a_failure() {
    use diesel::prelude::*;
    use tracing::instrument::WithSubscriber as _;

    use crate::models::schema::network_endpoint_stats as stats;

    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    let node = replying(|body| {
        let message = "block range extends beyond current head block: requested 6, head 5";
        let error = json!({"code": -32602, "message": message});
        (
            200,
            json!({"jsonrpc": "2.0", "id": body["id"], "error": error}),
        )
    })
    .await;
    let ids = stored(&container, &db, vec![node]).await;
    let transport = container.transport(&network()).unwrap();
    async {
        let ahead = transport.request(rpc::Request::new("eth_getLogs", json!([])));
        assert!(ahead.await.unwrap_err().is_not_ready());
    }
    .with_subscriber(subscriber(&db, crate::usecase::logs::feed()))
    .await;

    let at = "/v1/logs?target=rpc.http.not_yet";
    let http = kept(&router, at, 1).await;
    assert_eq!(http[0]["level"], "debug", "{http:?}");
    assert_eq!(http[0]["fields"]["rpc_errors"], "-32602");
    let id = ids[0];
    let counted = super::support::until("the call counted", || async {
        let read = db.read(move |connection| {
            let counts = stats::table.filter(stats::endpoint_id.eq(id));
            let counts = counts.select((stats::calls, stats::failed_calls));
            Ok(counts.first::<(i64, i64)>(connection)?)
        });
        let counts = read.await.map_err(|error| error.to_string())?;
        (counts.0 == 1)
            .then_some(counts)
            .ok_or(format!("{counts:?}"))
    })
    .await;
    assert_eq!(counted, (1, 0), "answered, not failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_index_run_logs_one_step_in_its_job_and_network() {
    use tracing::instrument::WithSubscriber as _;

    use crate::usecase::workers::index::{Index, Target};

    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    stored(&container, &db, vec!["http://a.invalid/".to_owned()]).await;
    let target = Target {
        chain: "bitcoin".into(),
        network: "regtest".into(),
    };
    let worker = Index::new(container.clone(), db.clone(), queue.clone());
    let job = run_once(&queue, worker, &target)
        .with_subscriber(subscriber(&db, crate::usecase::logs::feed()))
        .await;

    let at = format!("/v1/logs?job_id={}", job.id);
    let entries = super::support::until("the job's entries stored", || async {
        let (_, listed) = call(&router, Method::GET, &at, None).await;
        let logs = listed["logs"].as_array().cloned().unwrap_or_default();
        let has = |target: &str| logs.iter().any(|e| e["target"] == target);
        (has("index.step") && has("job.run"))
            .then_some(logs.clone())
            .ok_or(listed.to_string())
    })
    .await;
    let id = i64::try_from(job.id.0).unwrap();
    let step = entries.iter().find(|e| e["target"] == "index.step");
    let step = step.unwrap_or_else(|| panic!("an index.step: {entries:?}"));
    assert_eq!(step["job_id"], id);
    assert_eq!(
        (&step["chain"], &step["network"]),
        (&json!("bitcoin"), &json!("regtest"))
    );
    assert_eq!(step["fields"]["kind"], "index");
    assert!(step["fields"]["outcome"].is_string() && step["fields"]["ms"].is_string());
    let run = entries.iter().find(|e| e["target"] == "job.run");
    let run = run.unwrap_or_else(|| panic!("a job.run: {entries:?}"));
    assert_eq!(run["fields"]["ack"], "again");
}

#[tokio::test(flavor = "multi_thread")]
async fn kept_logs_go_to_the_table_list_newest_first_and_prune() {
    use diesel::{connection::SimpleConnection as _, prelude::*};
    use tracing::instrument::WithSubscriber as _;

    use crate::{models::schema::log, usecase::workers::prune::Prune};

    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    stored(&container, &db, vec!["http://a.invalid/".to_owned()]).await;
    let dispatch = subscriber(&db, crate::usecase::logs::feed());
    async {
        let span = crate::usecase::network_span(&network());
        span.in_scope(|| {
            tracing::info!(target: "index.step", outcome = "first", "Index step finished");
            tracing::warn!(target: "index.reorg", from = 3, "Kept blocks reorged");
        });
        tracing::info!(target: "api", "No network's");
    }
    .with_subscriber(dispatch)
    .await;
    let all = super::support::until("the entries stored", || async {
        let (_, listed) = call(&router, Method::GET, "/v1/logs", None).await;
        let found = listed["logs"].as_array().is_some_and(|l| l.len() == 3);
        found.then(|| listed.clone()).ok_or(listed.to_string())
    })
    .await;
    assert_eq!(all["logs"][0]["target"], "api", "newest first");
    assert_eq!(all["logs"][0]["network"], Value::Null);
    let at = "/v1/logs?network=bitcoin/regtest";
    let (_, page) = call(&router, Method::GET, &format!("{at}&limit=1"), None).await;
    let newest = &page["logs"][0];
    assert_eq!(newest["target"], "index.reorg", "{page}");
    assert_eq!(newest["level"], "warn");
    assert_eq!(
        (&newest["chain"], &newest["network"]),
        (&json!("bitcoin"), &json!("regtest"))
    );
    assert_eq!(newest["fields"]["from"], "3");
    assert_eq!(newest["fields"]["network"], "bitcoin/regtest");
    let cursor = page["next_cursor"].as_str().expect("a cursor").to_owned();
    let older = format!("{at}&limit=1&cursor={cursor}");
    let (_, page) = call(&router, Method::GET, &older, None).await;
    assert_eq!(page["logs"][0]["target"], "index.step", "older");
    let count = |listed: &Value| listed["logs"].as_array().map(Vec::len);
    let (_, warned) = call(&router, Method::GET, &format!("{at}&level=warn"), None).await;
    assert_eq!(count(&warned), Some(1));
    let (_, only) = call(&router, Method::GET, "/v1/logs?level=info&only=true", None).await;
    assert_eq!(count(&only), Some(2), "info alone, not the warn: {only}");
    let (_, targeted) = call(&router, Method::GET, "/v1/logs?target=api", None).await;
    assert_eq!(count(&targeted), Some(1));
    let both = "/v1/logs?target=api,index.reorg";
    let (_, targeted) = call(&router, Method::GET, both, None).await;
    assert_eq!(count(&targeted), Some(2));
    let (status, _) = call(&router, Method::GET, "/v1/logs?level=loud", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&router, Method::GET, "/v1/logs?network=regtest", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // By time: none after the future, all before it; from a moment, oldest first.
    let now = i64::try_from(tasks::now()).unwrap();
    let later = now + 60_000;
    let (_, after) = call(
        &router,
        Method::GET,
        &format!("/v1/logs?after={later}"),
        None,
    )
    .await;
    assert_eq!(count(&after), Some(0));
    let (_, before) = call(&router, Method::GET, &format!("{at}&before={later}"), None).await;
    assert_eq!(count(&before), Some(2));
    let (_, before) = call(&router, Method::GET, "/v1/logs?before=1", None).await;
    assert_eq!(count(&before), Some(0));
    let (_, forward) = call(&router, Method::GET, &format!("{at}&after=0&limit=1"), None).await;
    assert_eq!(forward["logs"][0]["target"], "index.step", "oldest first");
    let cursor = forward["next_cursor"]
        .as_str()
        .expect("a cursor")
        .to_owned();
    let next = format!("{at}&after=0&limit=1&cursor={cursor}");
    let (_, forward) = call(&router, Method::GET, &next, None).await;
    assert_eq!(forward["logs"][0]["target"], "index.reorg", "then newer");

    // Debug a day and more old goes; info goes only past 30 days.
    let day = now - 25 * 60 * 60 * 1000;
    let month = now - 31 * 24 * 60 * 60 * 1000;
    db.write(move |connection| {
        connection.batch_execute(&format!(
            "INSERT INTO log (at, level, target, message, fields) VALUES
                ({day}, 'debug', 'old.debug', 'm', '{{}}'),
                ({day}, 'trace', 'old.trace', 'm', '{{}}'),
                ({day}, 'info', 'old.info', 'm', '{{}}'),
                ({month}, 'error', 'ancient', 'm', '{{}}');"
        ))?;
        Ok(())
    })
    .await
    .unwrap();
    let job = run_once(&queue, Prune::new(db.clone(), container.clone()), &()).await;
    assert_eq!(job.reason, None, "{job:?}");
    let kept: Vec<String> = db
        .read(move |connection| {
            let targets = log::table.select(log::target).order(log::target.asc());
            Ok(targets.load(connection)?)
        })
        .await
        .unwrap();
    assert_eq!(kept, ["api", "index.reorg", "index.step", "old.info"]);
}

/// A job keeps its newest 10 000 entries, the entries of no job each
/// network's newest 10 000 and no network's newest 10 000; a month-old one
/// goes however few its job has.
#[tokio::test(flavor = "multi_thread")]
async fn the_prune_caps_each_job_and_each_network_outside_a_job() {
    use diesel::{connection::SimpleConnection as _, prelude::*};

    use crate::{models::schema::log, usecase::workers::prune::Prune};

    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    db.write(|connection| {
        Ok(connection.batch_execute(
            "INSERT INTO q (id, topic, kind, status, run_at, attempts, reason, input,
               created_at, updated_at) VALUES
             (901,'n.a','broadcast',0,0,0,NULL,'{}',0,0),
             (902,'n.b','broadcast',0,0,0,NULL,'{}',0,0);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10005)
             INSERT INTO log (at, level, target, message, fields, chain, network, job_id)
             SELECT strftime('%s', 'now') * 1000, 'info', 'big', i, '{}', 'bitcoin', 'regtest', 901
               FROM n
             UNION ALL SELECT strftime('%s', 'now') * 1000, 'info', 'small', i, '{}', NULL, NULL, 902
               FROM n WHERE i <= 3
             UNION ALL SELECT strftime('%s', 'now') * 1000, 'info', 'regtest', i, '{}', 'bitcoin',
               'regtest', NULL FROM n WHERE i <= 10003
             UNION ALL SELECT strftime('%s', 'now') * 1000, 'info', 'none', i, '{}', NULL, NULL, NULL
               FROM n WHERE i <= 10002;
             INSERT INTO log (at, level, target, message, fields, job_id)
               VALUES (0, 'info', 'small', 'ancient', '{}', 902);",
        )?)
    })
    .await
    .expect("seeded");
    let job = run_once(&queue, Prune::new(db.clone(), container.clone()), &()).await;
    assert_eq!(job.reason, None, "{job:?}");
    let kept: Vec<(String, Option<String>)> = db
        .read(|connection| {
            Ok(log::table
                .select((log::target, log::message))
                .load(connection)?)
        })
        .await
        .unwrap();
    for (target, count, oldest) in [
        ("big", 10_000, "6"),
        ("small", 3, "1"),
        ("regtest", 10_000, "4"),
        ("none", 10_000, "3"),
    ] {
        let messages: Vec<u32> = kept
            .iter()
            .filter(|(kept, _)| kept == target)
            .map(|(_, message)| {
                let message = message.as_deref().unwrap_or_default();
                message.parse().expect("a number: the ancient one is gone")
            })
            .collect();
        assert_eq!(messages.len(), count, "{target}");
        let least = messages.iter().min().map(ToString::to_string);
        assert_eq!(least.as_deref(), Some(oldest), "{target}: the oldest go");
    }
}

/// A round tells its story: one `trigger.fired` per payment queued (under
/// that payment's job), a `trigger.skipped` with why, each under its
/// trigger, and one `trigger.round.acted` with the counts.
#[tokio::test(flavor = "multi_thread")]
async fn a_trigger_round_logs_each_trigger_and_its_counts() {
    use diesel::connection::SimpleConnection as _;
    use tracing::instrument::WithSubscriber as _;

    use crate::usecase::workers::{index::Target, trigger::Round};

    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    // A keyed payer and a watched payee, both indexed through block 0, the
    // confirmed one.
    db.write(|connection| {
        Ok(connection.batch_execute(
            "INSERT INTO wallet (id, chain, network, address, curve, secret) VALUES
              ('bitcoin.regtest.payer','bitcoin','regtest','payer','secp256k1',x'01'),
              ('bitcoin.regtest.payee','bitcoin','regtest','payee',NULL,NULL);
             INSERT INTO wallet_index (id, wallet_id, asset, requested_from, birthday,
               indexed_at, state) VALUES
              ('bitcoin.regtest.payer.native','bitcoin.regtest.payer','native',0,0,0,0),
              ('bitcoin.regtest.payee.native','bitcoin.regtest.payee','native',0,0,0,0);
             INSERT INTO network_index (chain, network, tip, confirmed)
               VALUES ('bitcoin','regtest',0,0)
               ON CONFLICT (chain, network) DO UPDATE SET tip = 0, confirmed = 0;",
        )?)
    })
    .await
    .expect("seeded");
    // Pays while the payer is one index or more; waits for five.
    let trigger = |id: &str, count: &str| {
        json!({
            "id": id,
            "chain": "bitcoin",
            "network": "regtest",
            "selector": {"ids": ["bitcoin.regtest.payer.native"]},
            "check": {"compare": {
                "of": {"this": {}},
                "aggregate": "count",
                "operator": "at_least",
                "value": {"value": count},
            }},
            "action": {"name": "send", "input": {
                "source": {"this": {}},
                "asset": "native",
                "destination": {"select": {"ids": ["bitcoin.regtest.payee.native"]}},
                "amount": {"value": "0.00000001"},
            }},
        })
    };
    let body = json!({"triggers": [trigger("pays", "1"), trigger("waits", "5")]});
    let (status, created) = call(&router, Method::POST, "/v1/triggers", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let target = Target {
        chain: "bitcoin".into(),
        network: "regtest".into(),
    };
    let actions = Arc::new(crate::usecase::workers::trigger::actions::registry());
    let worker = Round::new(container.clone(), db.clone(), queue.clone(), actions);
    let round = run_once(&queue, worker, &target)
        .with_subscriber(subscriber(&db, crate::usecase::logs::feed()))
        .await;

    let at = "/v1/logs?target=trigger.round.acted,trigger.fired,trigger.skipped";
    let logs = kept(&router, at, 3).await;
    let of = |target: &str| {
        let found = logs.iter().find(|entry| entry["target"] == target);
        found
            .unwrap_or_else(|| panic!("a {target}: {logs:?}"))
            .clone()
    };
    let counted = of("trigger.round.acted");
    assert_eq!(counted["level"], "info", "one fired");
    assert_eq!(counted["job_id"], i64::try_from(round.id.0).unwrap());
    assert_eq!(counted["network"], "regtest");
    for (count, value) in [
        ("evaluated", "2"),
        ("fired", "1"),
        ("skipped", "1"),
        ("refused", "0"),
        ("wallets", "2"),
    ] {
        assert_eq!(counted["fields"][count], value, "{count}: {counted}");
    }
    assert!(counted["fields"]["ms"].is_string(), "{counted}");
    let (_, sends) = call(&router, Method::GET, "/v1/jobs?kind=broadcast", None).await;
    let send = sends["jobs"][0]["id"].as_str().expect("the queued send");
    let fired = of("trigger.fired");
    assert_eq!(fired["level"], "info");
    assert_eq!(fired["trigger_id"], "pays");
    assert_eq!(fired["job_id"].to_string(), send, "the payment's job");
    assert_eq!(fired["wallet_id"], "bitcoin.regtest.payer");
    assert_eq!(
        (&fired["fields"]["asset"], &fired["fields"]["amount"]),
        (&json!("native"), &json!("1"))
    );
    let skipped = of("trigger.skipped");
    assert_eq!(skipped["level"], "debug");
    assert_eq!(skipped["trigger_id"], "waits");
    assert_eq!(skipped["fields"]["reason"], "check did not hold");

    let (_, waits) = call(&router, Method::GET, "/v1/logs?trigger_id=waits", None).await;
    let waits = waits["logs"].as_array().expect("entries");
    assert!(!waits.is_empty());
    assert!(
        waits.iter().all(|entry| entry["trigger_id"] == "waits"),
        "{waits:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broadcast_job_keeps_its_node_calls_and_steps_under_its_id() {
    use base::Addressed as _;
    use tracing::instrument::WithSubscriber as _;

    use crate::usecase::workers::broadcast::{Broadcast, SendIntent};

    let (stores, db, _directory) = seeded().await;
    let script = Arc::new(Script {
        relay: true,
        ..Script::default()
    });
    let sender = script.keys.keyed("sender");
    base::WalletStore::insert(&stores, &sender)
        .await
        .expect("the wallet kept");
    super::storage::watch(&stores, sender.address(), 0).await;
    let container = container(&db, script);
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    let node = replying(|body| {
        (
            200,
            json!({"jsonrpc": "2.0", "id": body["id"], "result": "id"}),
        )
    });
    stored(&container, &db, vec![node.await]).await;
    let dispatch = subscriber(&db, crate::usecase::logs::feed());
    let intent = SendIntent {
        chain: network().chain.to_string(),
        network: network().name,
        address: "sender".to_owned(),
        asset: "native".to_owned(),
        to: "recipient".into(),
        amount: "1".into(),
        fee: base::FeeLevel::Normal,
    };
    let worker = Broadcast::new(container.clone(), db.clone());
    let job = run_once(&queue, worker, &intent)
        .with_subscriber(dispatch)
        .await;

    let at = format!("/v1/logs?job_id={}", job.id);
    let listed = super::support::until("the job's entries stored", || async {
        let (_, listed) = call(&router, Method::GET, &at, None).await;
        let targets: Vec<String> = listed["logs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["target"].as_str().map(str::to_owned))
            .collect();
        let found = targets.iter().any(|t| t == "rpc.http.answered")
            && targets.iter().any(|t| t == "send.broadcast.accepted");
        found.then(|| listed.clone()).ok_or(listed.to_string())
    })
    .await;
    let id = i64::try_from(job.id.0).unwrap();
    for entry in listed["logs"].as_array().unwrap() {
        assert_eq!(entry["job_id"], id, "{entry}");
        // `job.run` is the executor's, outside the run's network span.
        if entry["target"] != "job.run" {
            assert_eq!(entry["network"], "regtest", "{entry}");
        }
    }
    let http = listed["logs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["target"] == "rpc.http.answered")
        .unwrap();
    assert_eq!(http["fields"]["method"], "sendrawtransaction");
    assert_eq!(
        http["fields"]["purpose"], "send.broadcast",
        "why it was sent"
    );
    let sent = listed["logs"].as_array().unwrap().iter();
    let sent = sent
        .clone()
        .find(|entry| entry["target"] == "send.broadcast.accepted");
    let sent = sent.expect("a send.broadcast.accepted");
    let wallet = sent["wallet_id"].as_str().expect("its wallet").to_owned();
    assert!(wallet.starts_with("bitcoin.regtest."), "{sent}");
    let transaction = sent["transaction_id"].as_str().expect("its transaction");
    assert!(transaction.starts_with("bitcoin.regtest."), "{sent}");
    for (filter, id) in [
        ("wallet_id", wallet.as_str()),
        ("transaction_id", transaction),
    ] {
        let (_, listed) = call(
            &router,
            Method::GET,
            &format!("/v1/logs?{filter}={id}"),
            None,
        )
        .await;
        let entries = listed["logs"].as_array().unwrap();
        assert!(!entries.is_empty(), "{filter}: {listed}");
        assert!(entries.iter().all(|entry| entry[filter] == id), "{listed}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_log_stream_sends_each_new_entry_its_filters_read() {
    let (_, db, _directory) = seeded().await;
    let feed = crate::usecase::logs::feed();
    let state = Arc::new(AppState {
        container: container(&db, Arc::new(Script::default())),
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db.clone()),
        actions: Arc::new(crate::usecase::workers::trigger::actions::registry()),
        logs: feed.clone(),
        candles: Default::default(),
        root_exists: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = as_root(api::router(state, &config).expect("a router")).await;
    let (status, _) = text(&router, "/v1/logs/stream?network=regtest").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let uri = "/v1/logs/stream?level=warn&target=index.reorg&network=bitcoin/regtest";
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = router.clone().oneshot(request).await.expect("a response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let uri = "/v1/logs/stream?level=info&only=true&wallet_id=bitcoin.regtest.w1";
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let wallet = router.clone().oneshot(request).await.expect("a response");
    assert_eq!(wallet.status(), StatusCode::OK);
    let uri = "/v1/logs/stream?trigger_id=t1";
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let trigger = router.oneshot(request).await.expect("a response");
    assert_eq!(trigger.status(), StatusCode::OK);
    tracing::dispatcher::with_default(&subscriber(&db, feed), || {
        let span = crate::usecase::network_span(&network());
        span.in_scope(|| {
            tracing::info!(target: "index.reorg", "Too mild");
            tracing::warn!(target: "index.step", "Another event");
            tracing::warn!(target: "index.leg", wallet = "w1", "Too severe");
            tracing::info!(target: "index.leg", wallet = "w2", "Another wallet");
        });
        tracing::warn!(target: "index.reorg", "No network's");
        span.in_scope(|| tracing::warn!(target: "index.reorg", from = 3, "Kept blocks reorged"));
        span.in_scope(|| tracing::info!(target: "index.leg", wallet = "w1", "The wallet's"));
        tracing::info!(target: "trigger.skipped", trigger_id = "t2", "Another trigger");
        let trigger = tracing::info_span!("trigger", trigger_id = "t1");
        trigger.in_scope(|| tracing::debug!(target: "trigger.skipped", "The trigger's"));
    });
    let first = |response: axum::response::Response| async move {
        let mut body = response.into_body();
        let wait = std::time::Duration::from_secs(5);
        let frame = tokio::time::timeout(wait, body.frame()).await;
        let frame = frame
            .expect("an event")
            .expect("a frame")
            .expect("its bytes");
        let event = String::from_utf8_lossy(frame.data_ref().expect("data")).into_owned();
        let data = event.strip_prefix("data: ");
        let data = data.unwrap_or_else(|| panic!("a data line: {event}"));
        serde_json::from_str::<Value>(data.trim_end()).expect("an entry")
    };
    let its = first(trigger).await;
    assert_eq!(its["message"], "The trigger's", "{its}");
    assert_eq!(its["trigger_id"], "t1");
    let theirs = first(wallet).await;
    assert_eq!(theirs["message"], "The wallet's", "{theirs}");
    assert_eq!(theirs["wallet_id"], "bitcoin.regtest.w1");
    let entry = first(response).await;
    assert_eq!(
        entry["message"], "Kept blocks reorged",
        "the first it reads"
    );
    assert_eq!((&entry["id"], &entry["level"]), (&json!(0), &json!("warn")));
    assert_eq!(
        (&entry["chain"], &entry["network"]),
        (&json!("bitcoin"), &json!("regtest"))
    );
    assert_eq!(entry["fields"]["from"], "3");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_index_step_that_never_answers_times_out_and_retries() {
    use crate::usecase::workers::index::{Index, Target};

    let (_, db, _directory) = seeded().await;
    let script = Script {
        hung: true,
        ..Script::default()
    };
    let container = container(&db, Arc::new(script));
    let queue = crate::usecase::tasks::queue(db.clone());
    let target = Target {
        chain: "bitcoin".into(),
        network: "regtest".into(),
    };
    let worker = Index::new(container, db.clone(), queue.clone())
        .with_step(std::time::Duration::from_millis(50));
    let job = run_once(&queue, worker, &target).await;
    let reason = job.reason.clone().unwrap_or_default();
    assert_eq!(reason, "an index step timed out after 0.05 s", "{job:?}");
    assert_eq!(job.status, tasks::Status::Pending, "{job:?}");
    assert_eq!(job.attempts, 1, "retried: {job:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_log_messages_are_the_catalog() {
    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let router = app(&container, &db, &queue).await;
    let (status, listed) = call(&router, Method::GET, "/v1/logs/messages", None).await;
    assert_eq!(status, StatusCode::OK);
    let catalog = serde_json::to_value(&*crate::events::MESSAGES).unwrap();
    assert_eq!(listed["messages"], catalog);
}

/// The markets list each market with its last quote and the latest prices
/// from memory; a feed is turned off by name (logged in its market); a
/// market's hourly candles read; each market's requests are logged in it,
/// read by `market=`.
#[tokio::test(flavor = "multi_thread")]
async fn markets_list_set_read_candles_and_log() {
    use tracing::instrument::WithSubscriber as _;

    use crate::{
        models::market_candle_1h::MarketCandles1hExchangeAssetMapCached,
        usecase::{market::tests::Fixed, workers::market::Candles},
    };

    let (_, db, _directory) = seeded().await;
    let container = container(&db, Arc::new(Script::default()));
    let queue = crate::usecase::tasks::queue(db.clone());
    let (feed, candles) = (
        crate::usecase::logs::feed(),
        MarketCandles1hExchangeAssetMapCached::default(),
    );
    let state = Arc::new(AppState {
        container: container.clone(),
        db: db.clone(),
        queue: queue.clone(),
        actions: Arc::new(crate::usecase::workers::trigger::actions::registry()),
        logs: feed.clone(),
        candles: candles.clone(),
        root_exists: Default::default(),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = as_root(api::router(state, &config).expect("a router")).await;
    let markets: Vec<Box<dyn crate::usecase::market::Market>> = vec![
        Box::new(Fixed::new("binance", Some("100"))),
        Box::new(Fixed::new("kraken", None)),
    ];
    Candles::new(db.clone(), markets, candles)
        .tick(tasks::now())
        .with_subscriber(subscriber(&db, feed.clone()))
        .await
        .unwrap();

    let (status, listed) = call(&router, Method::GET, "/v1/markets", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let names: Vec<&str> = listed["markets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|market| market["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["binance", "bitstamp", "coinbase", "coingecko", "kraken"]
    );
    assert!(listed["markets"][0]["last_at"].is_i64(), "{listed}");
    assert_eq!(listed["markets"][0]["feed"], true);
    assert!(listed["markets"][4]["last_at"].is_null(), "kraken failed");
    assert_eq!(listed["prices"]["BTC"], "100");

    let off = Some(json!({"feed": false}));
    let (status, set) = call(&router, Method::PATCH, "/v1/markets/binance", off.clone())
        .with_subscriber(subscriber(&db, feed.clone()))
        .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(set["feed"], false);
    let (status, _) = call(&router, Method::PATCH, "/v1/markets/nope", off).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let set = kept(&router, "/v1/logs?market=binance&target=market.feed.set", 1).await;
    assert_eq!(set[0]["fields"]["feed"], "false");

    let at = "/v1/markets/binance/candles?asset=BTC";
    let (status, read) = call(&router, Method::GET, at, None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    let candles = read["candles"].as_array().unwrap();
    assert_eq!(candles.len(), 1, "{read}");
    assert_eq!(candles[0]["market"], "binance");
    assert_eq!(candles[0]["open"], "100");
    assert_eq!(candles[0]["close"], "100");
    let start = candles[0]["start"].as_i64().unwrap();
    let later = format!("{at}&after={}", start + 3600);
    let (_, read) = call(&router, Method::GET, &later, None).await;
    assert_eq!(read["candles"], json!([]), "none from a later hour");
    let (_, read) = call(
        &router,
        Method::GET,
        "/v1/markets/kraken/candles?asset=BTC",
        None,
    )
    .await;
    assert_eq!(read["candles"], json!([]), "kraken failed: none");
    let (status, _) = call(
        &router,
        Method::GET,
        "/v1/markets/nope/candles?asset=BTC",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let failed = kept(
        &router,
        "/v1/logs?market=kraken&target=market.feed.failed",
        1,
    )
    .await;
    assert_eq!(failed[0]["market"], "kraken");
    assert_eq!(failed[0]["fields"]["status"], "-", "no HTTP answer");
    let answered = kept(
        &router,
        "/v1/logs?market=binance&target=market.feed.answered",
        1,
    )
    .await;
    assert_eq!(answered[0]["level"], "debug");
    assert_eq!(answered[0]["fields"]["assets"], "5", "every seeded ticker");
    let (_, all) = call(&router, Method::GET, "/v1/logs?market=kraken", None).await;
    let all = all["logs"].as_array().unwrap();
    assert!(
        all.iter().all(|entry| entry["market"] == "kraken"),
        "{all:?}"
    );
}
