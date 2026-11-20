//! One node pruned old receipts, the other keeps them all: an index asked
//! to start below the floor keeps its start, and the deposit below the
//! floor is read from the full node.

use base::testing::{Caps, TestChain};

use super::{App, reaches, settle, unit};

pub(super) const NEEDS: Caps = Caps::PRUNE;

pub(super) fn prepare<T: TestChain>(chain: &T) {
    chain.mine(4);
}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = app.api.wallet(&chain.network()).await;
    chain.fund(&wallet.address, &unit(chain), "native");
    chain.mine(1);
    let deposit = chain.tip();
    chain.mine(8);
    chain.node(0).prune(chain.tip() - 4);
    let asked = deposit - 2;
    app.api.index(&wallet, "native", Some(asked)).await;
    settle(app, chain).await;
    let index = reaches(app, &wallet, "native", "balance_confirmed", &unit(chain)).await;
    assert_eq!(
        (index["requested_from"].as_u64(), index["birthday"].as_u64()),
        (Some(asked), Some(asked)),
        "{index}"
    );
    let legs = app.api.legs(&wallet, "native").await;
    assert_eq!(legs.len(), 1, "{legs:?}");
    assert_eq!(legs[0]["position"], deposit, "{legs:?}");
}
