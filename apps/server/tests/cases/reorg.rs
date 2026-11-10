//! A reorg above the confirmed block: the deposit's legs and balances are
//! rewound, and re-applied once the deposit lands on the new branch.

use base::testing::{Caps, TestChain};

use super::{App, reaches, unit, until};

pub(super) const NEEDS: Caps = Caps::REORG;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = app.api.wallet(&chain.network()).await;
    app.api.index(&wallet, "native", None).await;
    chain.fund(&wallet.address, &unit(chain), "native");
    chain.mine(1);
    reaches(app, &wallet, "native", "balance_pending", &unit(chain)).await;
    let first = app.api.legs(&wallet, "native").await;
    assert_eq!(first.len(), 1, "{first:?}");

    chain.reorg(2, 3);
    reaches(app, &wallet, "native", "balance_pending", "0").await;
    assert!(app.api.legs(&wallet, "native").await.is_empty());
    // The block is archived within the step; the row counts it as the
    // step ends.
    until("the reorg to be counted", || async {
        let network = app.api.network().await;
        if network["index"]["reorgs"].as_u64() >= Some(1) {
            return Ok(());
        }
        Err(network.to_string())
    })
    .await;

    chain.mine(1);
    let again = until("the deposit on the new branch", || async {
        let legs = app.api.legs(&wallet, "native").await;
        match legs.as_slice() {
            [leg] if leg["block"] != first[0]["block"] => Ok(leg.clone()),
            _ => Err(format!("{legs:?}")),
        }
    })
    .await;
    assert_eq!(again["amount"], unit(chain));
    reaches(app, &wallet, "native", "balance_pending", &unit(chain)).await;
}
