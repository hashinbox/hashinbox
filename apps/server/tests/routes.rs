//! The routes over the scripted network and a real SQLite file: create
//! (generate, import, watch), index (create, pause, resume, delete), tag,
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
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::{Script, database, limited, network, payments};
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
async fn wallets_balance_legs_and_sends() {
    let (db, _directory) = database().await;
    let payments = payments(&db, Arc::new(Script::default())).await;
    let state = Arc::new(AppState {
        payments,
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db.clone()),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = api::router(state, &config).expect("a router");
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
        &crate::usecase::sdk::Stores::new(db.clone()),
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
    assert_eq!(legs["next_cursor"], "1");
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

    let send = json!({ "transfers": [{ "destination": "bob", "amount": "0.1" }] });
    let (status, _) = call(
        &router,
        Method::POST,
        "/v1/wallets/bitcoin.regtest.alice/transactions",
        Some(send.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let from = format!("/v1/wallets/{generated_id}/transactions");
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
        (&network["index"]["tip"], &network["tip_live"]),
        (&Value::Null, &Value::Null)
    );
    assert!(network["tip_error"].is_string(), "{network}");
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
    assert_eq!((status, &health["ready"]), (StatusCode::OK, &json!(true)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_start_past_the_history_limit_is_refused_and_an_index_pauses_resumes_and_goes() {
    let (db, _directory) = database().await;
    let days = std::time::Duration::from_secs(30 * 86_400);
    let payments = limited(&db, Arc::new(Script::default()), Some(days)).await;
    let state = Arc::new(AppState {
        payments,
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db.clone()),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = api::router(state, &config).expect("a router");
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

    // Block 70 is about 30 days old.
    let (status, error) = call(&router, Method::POST, indexes, Some(watch(60))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert!(error.to_string().contains("30 days"), "{error}");
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
        Ok(connection.batch_execute("UPDATE network SET max_history_days = 30")?)
    })
    .await
    .unwrap();
    let (_, networks) = call(&router, Method::GET, "/v1/networks", None).await;
    assert_eq!(networks["networks"][0]["max_history_days"], 30);
    assert_eq!(networks["networks"][0]["tip_live"], 100);
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
    let (db, _directory) = database().await;
    let payments = payments(&db, Arc::new(Script::default())).await;
    let state = Arc::new(AppState {
        payments,
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db.clone()),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = api::router(state, &config).expect("a router");
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

    // An external id is kept trimmed, read back, set once and never replaced.
    let (status, frank) = create(json!({ "address": "frank", "external_id": " cus-1 " })).await;
    assert_eq!(
        (status, &frank["external_id"]),
        (StatusCode::CREATED, &json!("cus-1"))
    );
    let frank_uri = "/v1/wallets/bitcoin.regtest.frank";
    let (_, read) = call(&router, Method::GET, frank_uri, None).await;
    assert_eq!(read["external_id"], "cus-1", "{read}");
    let (status, frank) = create(json!({ "address": "frank", "external_id": "cus-2" })).await;
    assert_eq!(
        (status, &frank["external_id"]),
        (StatusCode::OK, &json!("cus-1"))
    );
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
        Some(json!({ "asset": "native", "start": 5 })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{index}");
    assert_eq!(index["requested_from"], 5);
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
        brand: None,
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
    use tasks::{Admission, Claim, OnLock, Store as _, Task, Worker as _};

    use crate::usecase::workers::index::{Index, Target};

    let (db, _directory) = database().await;
    let payments = payments(&db, Arc::new(Script::default())).await;
    let queue = crate::usecase::tasks::queue(db.clone());
    let worker = Arc::new(Index::new(payments.clone(), db.clone(), queue.clone()));
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
    let claim = Claim {
        kind: Index::KIND,
        limit: 1,
        on_lock: OnLock::Wait,
        now,
        until: now + 60_000,
    };
    let job = queue.claim(claim).await.expect("claimed").remove(0);
    let mut task = Task::new(worker, job, Arc::new(queue.clone()));
    task.run().await;
    task.update().await.expect("settled");
    let job = queue.get(id).await.expect("a read").expect("the job");
    let reason = job.reason.clone().unwrap_or_default();
    assert!(reason.contains("no such table: block"), "{job:?}");
    assert_eq!(job.status, tasks::Status::Pending);

    let state = Arc::new(AppState {
        payments,
        db: db.clone(),
        queue,
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = api::router(state, &config).expect("a router");
    let (status, health) = call(&router, Method::GET, "/health", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{health}");
    assert_eq!(health["ready"], json!(false));
    assert_eq!(health["networks"][0]["indexing"]["failures"], json!(1));
    let (status, networks) = call(&router, Method::GET, "/v1/networks", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(networks["networks"][0]["index_job_reason"], json!(reason));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trigger_run_shows_its_jobs_and_how_they_ended() {
    use diesel::connection::SimpleConnection as _;

    let (db, _directory) = database().await;
    let payments = payments(&db, Arc::new(Script::default())).await;
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
        payments,
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = api::router(state, &config).expect("a router");
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
async fn a_count_compares_with_a_whole_number_or_the_trigger_is_refused() {
    let (db, _directory) = database().await;
    let payments = payments(&db, Arc::new(Script::default())).await;
    let state = Arc::new(AppState {
        payments,
        db: db.clone(),
        queue: crate::usecase::tasks::queue(db),
    });
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("an address"),
        http_server::TransportSecurity::PlaintextLoopback,
        None,
        http_server::RequestLimits::default(),
    );
    let router = api::router(state, &config).expect("a router");
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
}
