//! Every node down mid-run: the server keeps serving and the network says
//! why (the index job's reason, the live tip's error, health not ready);
//! back up, it indexes to the tip with no gap, the deposit made meanwhile
//! counted once.

use axum::http::{Method, StatusCode};
use base::testing::{Caps, TestChain};

use super::{App, at_tip, funded, linked, number, reaches, unit, until};

pub(super) const NEEDS: Caps = Caps::NONE;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = funded(app, chain).await;
    let nodes = chain.nodes().len();
    for node in 0..nodes {
        chain.node(node).faults().down(true);
    }
    chain.fund(&wallet.address, &unit(chain), "native");
    chain.mine(1);
    until("the network to fail with why", || async {
        let network = app.api.network().await;
        if network["index"]["job_reason"].is_string() {
            return Ok(());
        }
        Err(network.to_string())
    })
    .await;
    let (status, health) = app.api.call(Method::GET, "/health", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{health}");

    for node in 0..nodes {
        chain.node(node).faults().down(false);
    }
    // The deposit's block read before it is confirmed (`settle`).
    at_tip(app, chain).await;
    chain.mine(chain.network().confirmations);
    let twice = (2 * number(&unit(chain))).to_string();
    reaches(app, &wallet, "native", "balance_confirmed", &twice).await;
    at_tip(app, chain).await;
    assert_eq!(app.api.legs(&wallet, "native").await.len(), 2);
    let blocks = linked(app, chain).await;
    assert_eq!(blocks[0]["position"], chain.tip());
}
