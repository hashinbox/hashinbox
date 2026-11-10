//! One node ahead of the other on the chain: the cluster reads the one
//! ahead. When it goes down, the cluster turns to the one behind (within
//! its allowed lag); what the server called confirmed stays confirmed.

use base::testing::{Caps, TestChain};

use super::{App, at_tip, funded, text, unit, until};

/// Blocks the second node trails by: within the cluster's allowed lag.
const LAG: u64 = 2;

pub(super) const NEEDS: Caps = Caps::CLUSTER;

pub(super) fn prepare<T: TestChain>(chain: &T) {
    chain.node(1).lag(LAG);
}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = funded(app, chain).await;
    let before = at_tip(app, chain).await["index"].clone();
    chain.node(0).faults().down(true);
    chain.mine(1);
    // The step that met the dead node failed; the next one, on the node
    // behind, cleared the reason.
    let network = until("a step on the node behind", || async {
        let network = app.api.network().await;
        let failed = network["index"]["failures"].as_u64() > before["failures"].as_u64();
        let index = app.api.index_of(&wallet, "native").await;
        assert_eq!(
            text(&index, "balance_confirmed"),
            unit(chain),
            "the balance went back: {index}"
        );
        if failed && network["index_job_reason"].is_null() {
            return Ok(network);
        }
        Err(network.to_string())
    })
    .await;
    assert!(
        network["index"]["confirmed"].as_u64() >= before["confirmed"].as_u64(),
        "{network}"
    );
    assert!(
        network["index"]["tip"].as_u64() >= before["tip"].as_u64(),
        "{network}"
    );
    let index = app.api.index_of(&wallet, "native").await;
    assert_eq!(text(&index, "balance_confirmed"), unit(chain), "{index}");
}
