//! The primary sits on a side branch: it holds the block with the deposit
//! when the chain takes that block back, while the other node follows the
//! chain ahead of it. Once it rejoins the chain, the side branch's block is
//! archived, every live block is one branch, and the deposit counts once,
//! on the chain's own block.

use base::testing::{Caps, TestChain};

use super::{App, at_tip, linked, open, reaches, settle, unit, until};

pub(super) const NEEDS: Caps = Caps::CLUSTER.with(Caps::REORG);

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = app.api.wallet(&chain.network()).await;
    open(app, &wallet, "native").await;
    chain.fund(&wallet.address, &unit(chain), "native");
    chain.mine(1);
    reaches(app, &wallet, "native", "balance_pending", &unit(chain)).await;
    let side = app.api.legs(&wallet, "native").await[0]["block"].clone();
    chain.node(0).fork(true);
    let primary = chain.nodes()[0].clone();
    T::client(primary)
        .tip()
        .await
        .expect("the primary pins its branch");
    chain.reorg(1, 2);
    chain.mine(1);

    chain.node(0).fork(false);
    settle(chain);
    let leg = until("the deposit on the chain's block", || async {
        let legs = app.api.legs(&wallet, "native").await;
        match legs.as_slice() {
            [leg] if leg["block"] != side => Ok(leg.clone()),
            _ => Err(format!("{legs:?}")),
        }
    })
    .await;
    assert_eq!(leg["amount"], unit(chain));
    reaches(app, &wallet, "native", "balance_confirmed", &unit(chain)).await;
    at_tip(app, chain).await;
    let blocks = linked(app, chain).await;
    assert!(blocks.iter().all(|block| block["id"] != side), "{blocks:?}");
}
