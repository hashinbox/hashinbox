//! Nodes that pruned old receipts (Sepolia answers `null` for them): an
//! index asked to start below the floor keeps the start it was asked for,
//! starts at the chain's final block, above the floor, and indexes on.

use base::testing::{Caps, TestChain};

use super::{App, reaches, settle, unit};

pub(super) const NEEDS: Caps = Caps::PRUNE;

pub(super) fn prepare<T: TestChain>(chain: &T) {
    chain.mine(12);
}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    // Below the final block, `confirmations` under the tip.
    let floor = chain.tip() - chain.network().confirmations - 1;
    chain.prune(floor);
    let wallet = app.api.wallet(&chain.network()).await;
    let asked = floor - 6;
    app.api.index(&wallet, "native", Some(asked)).await;
    chain.fund(&wallet.address, &unit(chain), "native");
    settle(app, chain).await;
    let index = reaches(app, &wallet, "native", "balance_confirmed", &unit(chain)).await;
    assert_eq!(index["requested_from"], asked);
    assert!(index["birthday"].as_u64() >= Some(floor), "{index}");
    assert_eq!(index["state"], "indexing");
}
