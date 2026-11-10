//! Nodes disagree on what is finalized (one a block behind its head, the
//! other six) and the primary changes mid-way: the confirmed position and
//! the confirmed balance only ever grow. The server's rule: confirmed is
//! the indexed tip less the network's `confirmations`; a node's
//! `finalized` only tells pruned receipts from receipts not ready.

use std::cell::Cell;

use base::testing::{Caps, TestChain};

use super::{App, number, reaches, unit, until};

pub(super) const NEEDS: Caps = Caps::CLUSTER;

pub(super) fn prepare<T: TestChain>(chain: &T) {
    chain.node(0).finality(Some(1));
    chain.node(1).finality(Some(6));
}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = app.api.wallet(&chain.network()).await;
    app.api.index(&wallet, "native", None).await;
    chain.fund(&wallet.address, &unit(chain), "native");
    // The confirmed position and balance last seen.
    let seen = Cell::new((0, 0));
    for block in 0..8 {
        if block == 4 {
            chain.node(0).faults().down(true);
        }
        chain.mine(1);
        let tip = chain.tip();
        until(&format!("the index at {tip}"), || async {
            let network = app.api.network().await;
            let index = app.api.index_of(&wallet, "native").await;
            let confirmed = network["index"]["confirmed"].as_u64().unwrap_or(0);
            let balance = index["balance_confirmed"].as_str().map_or(0, number);
            let now = (confirmed, balance);
            let before = seen.replace(now);
            assert!(
                now.0 >= before.0 && now.1 >= before.1,
                "went back: {network} {index}"
            );
            if network["index"]["tip"] == tip {
                return Ok(());
            }
            Err(network.to_string())
        })
        .await;
    }
    reaches(app, &wallet, "native", "balance_confirmed", &unit(chain)).await;
}
