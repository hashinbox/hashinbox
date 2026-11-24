//! A network whose node is down at boot, or whose settings do not decode: the
//! server boots, the other network indexes, the failing one says why in its
//! index job and live tip, and indexes once its node answers. A new index
//! starts at the chain's final block, so it waits for the node too.

use axum::http::{Method, StatusCode};
use base::{NetworkConfig, testing::TestChain};
use chain_ethereum::testing::Ethereum;

use super::{App, until};

/// Two mock Sepolias: `sepolia`, and `other` (its network renamed).
fn two() -> (Ethereum, Ethereum, NetworkConfig) {
    let (good, bad) = (Ethereum::start(1), Ethereum::start(1));
    let mut other = bad.network();
    "other".clone_into(&mut other.network.name);
    (good, bad, other)
}

/// Boots `good`'s network, a wallet indexed from block 1, and `other` over
/// `bad`'s node.
async fn boot(good: &Ethereum, bad: &Ethereum, other: NetworkConfig) -> App {
    let networks = vec![(good.network(), good.nodes()), (other, bad.nodes())];
    let app = App::boot::<Ethereum>(networks).await;
    let wallet = app.api.wallet(&good.network()).await;
    app.api.index(&wallet, "native", Some(1)).await;
    app
}

/// Waits until network `name` is indexed to `tip`.
async fn indexed(app: &App, name: &str, tip: u64) {
    until(&format!("{name} indexed to {tip}"), || async {
        let network = app.api.network_named(name).await;
        if network["index"]["tip"] == tip {
            return Ok(());
        }
        Err(network.to_string())
    })
    .await;
}

/// Waits until network `name`'s index job and live tip fail, the job's
/// reason containing `why`.
async fn failing(app: &App, name: &str, why: &str) {
    until(&format!("{name} failing with {why}"), || async {
        let network = app.api.network_named(name).await;
        let reason = network["index"]["job_reason"].as_str().unwrap_or_default();
        if reason.contains(why) && network["tip"]["error"].is_string() {
            return Ok(());
        }
        Err(network.to_string())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_down_at_boot_fails_its_network_alone_until_it_answers() {
    let (good, bad, other) = two();
    bad.node(0).faults().down(true);
    let app = boot(&good, &bad, other.clone()).await;
    let wallet = app.api.wallet(&other).await;
    let uri = format!("/v1/wallets/{}/indexes", wallet.id);
    let body = serde_json::json!({"asset": "native", "start": 1});
    let (status, error) = app.api.call(Method::POST, &uri, Some(body)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{error}");
    let (status, health) = app.api.call(Method::GET, "/health", None).await;
    assert_eq!(health["networks"].as_array().map(Vec::len), Some(2));
    assert!(status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE);
    indexed(&app, "sepolia", good.tip()).await;
    failing(&app, "other", "(fault)").await;

    bad.node(0).faults().down(false);
    app.api.index(&wallet, "native", Some(1)).await;
    indexed(&app, "other", bad.tip()).await;
    // The index row moves within the step; the job clears its reason when
    // the step ends, so health turns ready a moment later.
    until("health to be ready", || async {
        let (status, health) = app.api.call(Method::GET, "/health", None).await;
        if status == StatusCode::OK {
            return Ok(());
        }
        Err(health.to_string())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_network_whose_settings_do_not_decode_fails_alone() {
    let (good, bad, mut other) = two();
    other.settings = serde_json::json!({"interval_ms": "soon"});
    let networks = vec![(good.network(), good.nodes()), (other, bad.nodes())];
    let app = App::boot::<Ethereum>(networks).await;
    let wallet = app.api.wallet(&good.network()).await;
    app.api.index(&wallet, "native", Some(1)).await;
    indexed(&app, "sepolia", good.tip()).await;
    failing(&app, "other", "Ethereum settings").await;
}
