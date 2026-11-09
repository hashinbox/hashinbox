//! The primary goes down after blocks are indexed: the cluster turns to
//! the other node, indexing goes on to the tip with no gap, and each
//! deposit counts once.

use base::testing::{Caps, TestChain};

use super::{App, at_tip, funded, linked, number, reaches, settle, unit};

pub(super) const NEEDS: Caps = Caps::CLUSTER;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = funded(app, chain).await;
    chain.node(0).faults().down(true);
    chain.mine(3);
    chain.fund(&wallet.address, &unit(chain), "native");
    settle(chain);
    let twice = (2 * number(&unit(chain))).to_string();
    reaches(app, &wallet, "native", "balance", &twice).await;
    at_tip(app, chain).await;
    let legs = app.api.legs(&wallet, "native").await;
    assert_eq!(legs.len(), 2, "{legs:?}");
    assert_ne!(legs[0]["block"], legs[1]["block"], "{legs:?}");
    let blocks = linked(app, chain).await;
    assert_eq!(blocks[0]["position"], chain.tip());
}
