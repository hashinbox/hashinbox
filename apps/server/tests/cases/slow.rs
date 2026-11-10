//! The primary turns slow, past the transport's timeout, mid-run: each
//! call waits and then times out. The cluster turns to the other node and
//! indexing reaches the tip.

use std::time::Duration;

use base::testing::{Caps, TestChain};
use rpc::{ErrorKind, testing::Fault};

use super::{App, at_tip, funded, number, reaches, settle, unit};

/// How long the slow node takes before its call times out.
const SLOW: Duration = Duration::from_millis(25);

pub(super) const NEEDS: Caps = Caps::CLUSTER;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = funded(app, chain).await;
    let slow = chain.node(0).faults().clone();
    slow.delay(SLOW);
    slow.limit(Some(0), Fault::Transport(ErrorKind::Timeout));
    chain.fund(&wallet.address, &unit(chain), "native");
    settle(chain);
    let twice = (2 * number(&unit(chain))).to_string();
    reaches(app, &wallet, "native", "balance_confirmed", &twice).await;
    at_tip(app, chain).await;
}
