//! The primary stays on a side branch the chain took back, answering
//! without fault, while the other node follows the chain ahead of it: the
//! server leaves the dead branch for the chain's tip, and the deposit
//! counts on the chain's own block.

use base::testing::{Caps, TestChain};

use super::{App, at_tip, reaches, unit, until};

pub(super) const NEEDS: Caps = Caps::CLUSTER.with(Caps::REORG);

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = app.api.wallet(&chain.network()).await;
    app.api.index(&wallet, "native", None).await;
    chain.fund(&wallet.address, &unit(chain), "native");
    chain.mine(1);
    reaches(app, &wallet, "native", "balance_pending", &unit(chain)).await;
    let side = app.api.legs(&wallet, "native").await[0]["block"].clone();
    chain.node(0).fork(true);
    let primary = chain.nodes()[0].clone();
    T::height(&primary)
        .await
        .expect("the primary pins its branch");
    chain.reorg(1);
    chain.mine(3);

    until("the deposit on the chain's block", || async {
        let legs = app.api.legs(&wallet, "native").await;
        match legs.as_slice() {
            [leg] if leg["block"] != side => Ok(()),
            _ => Err(format!("{legs:?}")),
        }
    })
    .await;
    at_tip(app, chain).await;
    reaches(app, &wallet, "native", "balance_confirmed", &unit(chain)).await;
}
