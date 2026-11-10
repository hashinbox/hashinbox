//! Nodes that pruned old receipts (Sepolia answers `null` for them): an
//! index asked to start below the floor starts at the first block served,
//! keeps the start it was asked for, and indexes on.

use base::testing::{Caps, TestChain};

use super::{App, reaches, settle, unit};

pub(super) const NEEDS: Caps = Caps::PRUNE;

pub(super) fn prepare<T: TestChain>(chain: &T) {
    chain.mine(12);
}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let floor = chain.tip() - 4;
    chain.prune(floor);
    let wallet = app.api.wallet(&chain.network()).await;
    let asked = floor - 6;
    app.api.index(&wallet, "native", Some(asked)).await;
    chain.fund(&wallet.address, &unit(chain), "native");
    settle(chain);
    let index = reaches(app, &wallet, "native", "balance_confirmed", &unit(chain)).await;
    assert_eq!(index["requested_from"], asked);
    assert_eq!(index["birthday"], floor);
    assert_eq!(index["state"], "indexing");
}
