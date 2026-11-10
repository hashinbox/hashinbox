//! The primary rate-limits every call from mid-run on: the cluster turns to
//! the other node at its first refusal and leaves the limited one alone;
//! indexing reaches the tip without failing steps.

use base::testing::{Caps, TestChain};
use rpc::{ErrorKind, testing::Fault};

use super::{App, at_tip, funded, number, reaches, settle, unit};

pub(super) const NEEDS: Caps = Caps::CLUSTER;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = funded(app, chain).await;
    let limited = chain.node(0).faults().clone();
    limited.limit(Some(0), Fault::Transport(ErrorKind::HttpStatus(429)));
    let before = at_tip(app, chain).await["index"]["failures"].clone();
    chain.fund(&wallet.address, &unit(chain), "native");
    settle(chain);
    let twice = (2 * number(&unit(chain))).to_string();
    reaches(app, &wallet, "native", "balance_confirmed", &twice).await;
    let refused = limited.requests();
    for _ in 0..5 {
        chain.mine(1);
        at_tip(app, chain).await;
    }
    let network = at_tip(app, chain).await;
    assert_eq!(network["index"]["failures"], before, "{network}");
    assert_eq!(
        limited.requests(),
        refused,
        "the limited node is left alone"
    );
}
