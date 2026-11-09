//! The primary answers every call with a result that decodes as nothing
//! asked: a node failure like any other, so the cluster turns to the other
//! node and indexing reaches the tip.

use base::testing::{Caps, TestChain};
use rpc::testing::Fault;

use super::{App, at_tip, funded, number, reaches, settle, unit};

pub(super) const NEEDS: Caps = Caps::CLUSTER;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = funded(app, chain).await;
    chain.node(0).faults().limit(Some(0), Fault::Broken);
    chain.fund(&wallet.address, &unit(chain), "native");
    settle(chain);
    let twice = (2 * number(&unit(chain))).to_string();
    reaches(app, &wallet, "native", "balance", &twice).await;
    at_tip(app, chain).await;
}
