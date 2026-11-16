//! A reorg to a shorter chain: the node falls below the last kept block, so
//! the indexer cannot tell it from a node behind, and the tip and confirmed
//! position stay where they were. Once the new branch passes the last kept
//! block, the old blocks are archived, the reorg is counted, and the
//! deposit lands on the new branch.

use base::testing::{Caps, TestChain};

use super::{App, at_tip, open, reaches, unit, until};

pub(super) const NEEDS: Caps = Caps::REORG;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = app.api.wallet(&chain.network()).await;
    open(app, &wallet, "native").await;
    chain.fund(&wallet.address, &unit(chain), "native");
    chain.mine(1);
    reaches(app, &wallet, "native", "balance_pending", &unit(chain)).await;
    let first = app.api.legs(&wallet, "native").await;
    let before = at_tip(app, chain).await["index"].clone();

    // Two blocks taken back, one mined: the chain is a block shorter.
    let served = chain.node(0).faults().requests();
    chain.reorg(2, 1);
    until("steps on the shorter chain", || async {
        let network = app.api.network().await;
        let index = &network["index"];
        assert!(index["tip"].as_u64() >= before["tip"].as_u64(), "{network}");
        assert!(
            index["confirmed"].as_u64() >= before["confirmed"].as_u64(),
            "{network}"
        );
        if chain.node(0).faults().requests() > served + 6 {
            return Ok(());
        }
        Err(network.to_string())
    })
    .await;

    // The new branch passes the last kept block: the reorg is seen.
    chain.mine(2);
    until("the reorg to be counted", || async {
        let network = app.api.network().await;
        if network["index"]["reorgs"].as_u64() > before["reorgs"].as_u64() {
            return Ok(());
        }
        Err(network.to_string())
    })
    .await;
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
