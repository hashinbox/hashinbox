//! The composed server on its default storage: one index file, no external
//! service. Every assertion goes through the public routes, which know no
//! chain: assets, generate, watch, list, health, and restoration of
//! all of them after a restart.

use super::*;

/// The Bitcoin section of a configuration pointed at `node`.
fn bitcoin_index(node: &BitcoinNode) -> Value {
    json!({
        "network": "regtest",
        "rpc": rpc_config(&node.rpc_url, true),
        "confirmation_depth": 1,
        "cadence": {"poll_millis": 10, "batch_size": 10}
    })
}

async fn start_composed(
    directory: &TempDir,
    node: &BitcoinNode,
) -> Result<RunningApi, Box<dyn std::error::Error>> {
    let storage = json!({"path": directory.path().join("payment.db")});
    start_server(
        storage,
        json!({"bitcoin": bitcoin_index(node)}),
        json!([]),
        &[],
        None,
    )
    .await
}

async fn get_json(root: &str, path: &str) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(reqwest::Client::new()
        .get(format!("{root}{path}"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn post_json(
    root: &str,
    path: &str,
    body: Value,
) -> Result<(reqwest::StatusCode, Value), Box<dyn std::error::Error>> {
    let response = reqwest::Client::new()
        .post(format!("{root}{path}"))
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    Ok((status, response.json().await?))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_server_serves_every_abstraction_and_restores_after_a_restart()
-> Result<(), Box<dyn std::error::Error>> {
    let node = BitcoinNode::start().await;
    let directory = TempDir::new()?;
    let api = start_composed(&directory, &node).await?;

    // The configured assets are data, not a closed enum.
    let assets = get_json(&api.root, "/v1/assets").await?;
    assert_eq!(
        assets["assets"],
        json!([{
            "asset": "btc",
            "network": {"chain": "bitcoin", "name": "regtest"},
            "metadata": {
                "chain": "bitcoin",
                "id": "native",
                "name": "Bitcoin",
                "ticker": "BTC",
                "decimals": 8
            }
        }])
    );

    // Two generated wallets retain signing custody across restart.
    let generated = create_wallet(&api.root, "btc").await?;
    let imported = create_wallet(&api.root, "btc").await?;
    let watched_text = bitcoin_destination(5);
    let (status, watched) = post_json(
        &api.root,
        "/v1/addresses",
        json!({"asset": "btc", "address": watched_text}),
    )
    .await?;
    assert_eq!(status, reqwest::StatusCode::CREATED, "{watched}");
    assert_eq!(watched["watch_only"], true);
    assert_eq!(watched["address"], watched_text);
    assert_eq!(imported["watch_only"], false);

    let listed = get_json(&api.root, "/v1/wallets").await?;
    let ids = listed["wallets"]
        .as_array()
        .expect("wallet list")
        .iter()
        .map(|wallet| wallet["id"].as_str().expect("wallet ID").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 3);

    // The watched address is indexed like a wallet, without a key.
    let funding = node.fund(vec![
        FundingOutput::new(
            generated["address"].as_str().expect("generated address"),
            120_000,
        ),
        FundingOutput::new(&watched_text, 50_000),
    ]);
    node.mine();
    wait_balance(&api.root, wallet_id(&generated), "0.0012").await;
    let history = wait_history(&api.root, wallet_id(&watched), &funding).await;
    assert!(serde_json::to_string(&history)?.contains(&watched_text));
    let balance = reqwest::Client::new()
        .get(format!(
            "{}/v1/wallets/{}/balance",
            api.root,
            wallet_id(&watched)
        ))
        .bearer_auth(TOKEN)
        .send()
        .await?;
    assert_eq!(
        balance.status(),
        reqwest::StatusCode::OK,
        "a balance is a fold over indexed movements, so it needs no key"
    );
    assert_eq!(
        balance.json::<serde_json::Value>().await?["amount"],
        "0.0005",
        "the watched address's own funding, folded from the index"
    );

    let status = get_json(&api.root, "/healthz").await?;
    assert_eq!(
        status["ready"], true,
        "forward indexing reached the tip, so the process serves: {status}"
    );
    assert_eq!(
        status["networks"][0]["network"],
        json!({"chain": "bitcoin", "name": "regtest"})
    );
    assert!(status["networks"][0]["checkpoint"]["height"].as_u64() > Some(0));

    // Restart restores persisted signing keys and independent watch-only rows.
    api.stop().await;
    let restarted = start_composed(&directory, &node).await?;
    let restored = get_json(&restarted.root, "/v1/wallets").await?;
    let observed = |wallets: &Value| {
        wallets["wallets"]
            .as_array()
            .expect("wallet list")
            .iter()
            .map(|wallet| {
                json!({
                    "id": wallet["id"],
                    "asset": wallet["asset"],
                    "network": wallet["network"],
                    "address": wallet["address"],
                })
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        observed(&restored),
        observed(&listed),
        "every watched address must be restored"
    );
    for wallet in restored["wallets"].as_array().expect("wallet list") {
        assert_eq!(wallet["watch_only"], wallet["id"] == watched["id"]);
    }
    wait_history(&restarted.root, wallet_id(&generated), &funding).await;
    wait_history(&restarted.root, wallet_id(&watched), &funding).await;

    // A generated wallet can spend after restarting without an imported key.
    let refused = reqwest::Client::new()
        .post(format!(
            "{}/v1/wallets/{}/transactions",
            restarted.root,
            wallet_id(&generated)
        ))
        .bearer_auth(TOKEN)
        .json(&json!({"destination": bitcoin_destination(6), "amount": "0.0005"}))
        .send()
        .await?;
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::ACCEPTED,
        "a restored wallet retains its signing key"
    );

    restarted.stop().await;
    node.stop().await;
    Ok(())
}

/// The five rejections a framework produces before any handler runs, probed
/// over a real socket against the composed server: they must all answer the
/// documented JSON error body, never Axum's `text/plain` default.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_framework_rejection_answers_json_over_the_wire()
-> Result<(), Box<dyn std::error::Error>> {
    let node = BitcoinNode::start().await;
    let directory = TempDir::new()?;
    let api = start_composed(&directory, &node).await?;
    let wallet = create_wallet(&api.root, "btc").await?;
    let history = format!("/v1/wallets/{}/transactions", wallet_id(&wallet));
    let client = reqwest::Client::new();

    let probes: Vec<(&str, reqwest::RequestBuilder)> = vec![
        (
            "unknown query field",
            client
                .get(format!("{}{history}?commitment=finalized", api.root))
                .bearer_auth(TOKEN),
        ),
        (
            "malformed limit",
            client
                .get(format!("{}{history}?limit=all", api.root))
                .bearer_auth(TOKEN),
        ),
        (
            "malformed asset tag",
            client
                .post(format!("{}/v1/wallets", api.root))
                .json(&json!({"asset": "NOT-A-TAG"}))
                .bearer_auth(TOKEN),
        ),
        (
            "missing bearer token",
            client.get(format!("{}/v1/wallets", api.root)),
        ),
        (
            "wrong bearer token",
            client
                .get(format!("{}/v1/wallets", api.root))
                .bearer_auth("not-the-configured-token"),
        ),
        (
            "wrong content type",
            client
                .post(format!("{}/v1/wallets", api.root))
                .bearer_auth(TOKEN)
                .header(reqwest::header::CONTENT_TYPE, "text/plain")
                .body(r#"{"asset":"btc"}"#),
        ),
    ];
    for (probe, request) in probes {
        let response = request.send().await?;
        let status = response.status();
        assert!(status.is_client_error(), "{probe} answered {status}");
        assert_eq!(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.split(';').next().unwrap_or_default().trim()),
            Some("application/json"),
            "{probe} must answer JSON"
        );
        let body: Value = response.json().await?;
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "{probe} must carry a message: {body}"
        );
    }

    api.stop().await;
    node.stop().await;
    Ok(())
}

/// Readiness is what a load balancer reads, so nothing a client asks for
/// through the API may move it.
///
/// Admitting an address takes its network's watch/commit handshake
/// (`indexing::CommitGate`), which refuses whichever supervised pass was
/// mid-plan with a retryable `Conflict`. That is internal and self-healing —
/// no block, node, store or checkpoint changed, and the next pass reconciles —
/// so `/healthz` has to stay 200 across every admission. It did not: the
/// supervisor published `Retrying` for the lost race, roughly one `POST
/// /v1/addresses` in three landed inside a pass, and the process answered 503
/// until the next one. Sampled once, that is a readiness flake in
/// `the_server_serves_every_abstraction_and_restores_after_a_restart`; run for
/// real it is a process that depools itself whenever a client registers
/// addresses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admitting_addresses_never_moves_readiness() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let node = BitcoinNode::start().await;
    let directory = TempDir::new()?;
    let api = start_composed(&directory, &node).await?;

    // One reader watching the probe an orchestrator watches, for the whole of
    // the admissions below.
    let stop = Arc::new(AtomicBool::new(false));
    let probe = tokio::spawn({
        let url = format!("{}/healthz", api.root);
        let stop = Arc::clone(&stop);
        async move {
            let client = reqwest::Client::new();
            let mut dips = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                if let Ok(response) = client.get(&url).send().await
                    && !response.status().is_success()
                {
                    dips.push(response.status().as_u16());
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            dips
        }
    });

    for byte in 20..60_u8 {
        let (status, body) = post_json(
            &api.root,
            "/v1/addresses",
            json!({
                "asset": "btc",
                "address": bitcoin_destination(byte)
            }),
        )
        .await?;
        assert_eq!(status, reqwest::StatusCode::CREATED, "{body}");
    }

    stop.store(true, Ordering::Relaxed);
    let dips = probe.await?;
    assert!(
        dips.is_empty(),
        "admitting an address must never answer 503 on /healthz: {dips:?}"
    );
    let status = get_json(&api.root, "/healthz").await?;
    assert_eq!(status["ready"], true, "{status}");
    assert_eq!(status["networks"][0]["state"], "ready", "{status}");
    assert!(
        status["networks"][0]["failure"].is_null(),
        "a lost internal race is not a reason forward indexing is held back: {status}"
    );

    api.stop().await;
    node.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runtime_nodes_and_reindex_preserve_wallets_and_saved_configuration()
-> Result<(), Box<dyn std::error::Error>> {
    let node = BitcoinNode::start().await;
    let directory = TempDir::new()?;
    let api = start_composed(&directory, &node).await?;
    let wallet = create_wallet(&api.root, "btc").await?;
    let address = wallet["address"].as_str().unwrap();
    let funding = node.fund(vec![FundingOutput::new(address, 120_000)]);
    node.mine();
    wait_balance(&api.root, wallet_id(&wallet), "0.0012").await;

    let saved = get_json(&api.root, "/v1/chains").await?;
    assert_eq!(
        saved[0]["networks"][0]["nodes"][0]["origin"],
        url::Url::parse(&node.rpc_url)?
            .origin()
            .ascii_serialization()
    );
    assert!(saved[0]["networks"][0]["nodes"][0]["endpoint"].is_null());
    assert!(!saved.to_string().contains("Basic test"));
    let network = &saved[0]["networks"][0];
    let response = reqwest::Client::new()
        .put(format!("{}/v1/chains/bitcoin", api.root))
        .bearer_auth(TOKEN)
        .json(&json!({"revision": saved[0]["revision"], "networks": [{
            "name": "regtest", "parameters": network["parameters"],
            "nodes": [
                {"id":"unavailable", "endpoint":"http://127.0.0.1:1"},
                {"id": network["nodes"][0]["id"]}
            ], "indexing": "running"
        }]}))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let revision = response.json::<Value>().await?["revision"].clone();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let row = get_json(&api.root, "/v1/chains").await.unwrap();
            if row[0]["networks"][0]["active_revision"] == revision {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert_eq!(
        get_json(&api.root, "/v1/wallets").await?["wallets"][0]["id"],
        wallet["id"]
    );

    let response = reqwest::Client::new()
        .put(format!("{}/v1/indexing", api.root))
        .bearer_auth(TOKEN)
        .json(&json!({"network":{"chain":"bitcoin", "name":"regtest"}, "action":"reindex"}))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let row = get_json(&api.root, "/v1/chains").await.unwrap();
            if row[0]["networks"][0]["rebuild"].is_null() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    wait_balance(&api.root, wallet_id(&wallet), "0.0012").await;
    wait_history(&api.root, wallet_id(&wallet), &funding).await;
    api.stop().await;
    let restarted = start_composed(&directory, &node).await?;
    let saved = get_json(&restarted.root, "/v1/chains").await?;
    assert_eq!(saved[0]["networks"][0]["nodes"][0]["id"], "unavailable");
    assert_eq!(
        get_json(&restarted.root, "/v1/wallets").await?["wallets"][0]["address"],
        address
    );
    wait_balance(&restarted.root, wallet_id(&wallet), "0.0012").await;
    restarted.stop().await;
    node.stop().await;
    Ok(())
}
