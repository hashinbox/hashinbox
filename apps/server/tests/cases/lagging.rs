//! A cluster whose first node lags: the cluster reads the node at the tip,
//! so the live tip, a new index's start and the indexing are the chain's.

use base::testing::{Caps, TestChain};

use super::{App, reaches, settle, unit, until};

/// Blocks the first node trails by: beyond the cluster's allowed lag.
const LAG: u64 = 8;

pub(super) const NEEDS: Caps = Caps::CLUSTER;

pub(super) fn prepare<T: TestChain>(chain: &T) {
    chain.mine(LAG + 2);
    chain.node(0).lag(LAG);
}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let network = app.api.network().await;
    assert_eq!(network["tip_live"], chain.tip(), "{network}");
    let wallet = app.api.wallet(&chain.network()).await;
    let index = app.api.index(&wallet, "native", None).await;
    assert_eq!(index["requested_from"], chain.tip());
    chain.fund(&wallet.address, &unit(chain), "native");
    settle(app, chain).await;
    reaches(app, &wallet, "native", "balance_confirmed", &unit(chain)).await;
    let tip = chain.tip();
    until("the index at the chain's tip", || async {
        let network = app.api.network().await;
        if network["index"]["tip"] == tip {
            return Ok(());
        }
        Err(network.to_string())
    })
    .await;
}
