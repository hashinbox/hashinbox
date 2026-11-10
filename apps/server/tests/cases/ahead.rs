//! One node ahead of the other on the chain: the cluster reads the one
//! ahead. When it goes down, the cluster turns to the one behind (within
//! its allowed lag); the tip and what the server called confirmed never go
//! back, and the confirmed balance stays.

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
    let served = chain.node(1).faults().requests();
    chain.node(0).faults().down(true);
    chain.mine(1);
    // The cluster turns to the node behind within the same step; a few of
    // its answers mean the indexer read through it.
    let network = until("steps on the node behind", || async {
        let index = app.api.index_of(&wallet, "native").await;
        assert_eq!(
            text(&index, "balance_confirmed"),
            unit(chain),
            "the balance went back: {index}"
        );
        let network = app.api.network().await;
        if chain.node(1).faults().requests() > served + 6 {
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
