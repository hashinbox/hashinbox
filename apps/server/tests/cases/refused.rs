//! Two nodes, one refusing a send the other accepts: the first trails the
//! block that funded the sender, so it sees no funds and refuses as a node
//! does. The send reaches each node once, the job counts it accepted, and
//! it is spent once.

use base::testing::{Caps, TestChain};

use super::{App, funded, number, quarter, settle, succeeded, text, unit, until};

/// Blocks the refusing node trails by: below the funding block, and beyond
/// the cluster's allowed lag, so it never reads for the server.
const LAG: u64 = 4;

pub(super) const NEEDS: Caps = Caps::CLUSTER;

pub(super) fn prepare<T: TestChain>(chain: &T) {
    chain.node(0).lag(LAG);
}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = funded(app, chain).await;
    let other = app.api.wallet(&chain.network()).await;
    let job = app
        .api
        .send(&wallet, "native", &other.address, "0.25")
        .await;
    let done = succeeded(app, &job).await;
    assert_eq!(
        (done["attempts"].as_u64(), done["accepted"].as_u64()),
        (Some(1), Some(1)),
        "{done}"
    );
    for node in 0..chain.nodes().len() {
        assert_eq!(chain.node(node).faults().calls(T::SUBMIT), 1, "node {node}");
    }
    settle(chain);
    let mined = until("the send to be mined", || async {
        let legs = app.api.legs(&wallet, "native").await;
        let spent: Vec<_> = legs
            .iter()
            .filter(|leg| leg["kind"] == 1)
            .cloned()
            .collect();
        if spent.is_empty() {
            return Err(format!("{legs:?}"));
        }
        Ok(spent)
    })
    .await;
    assert_eq!(mined.len(), 1, "{mined:?}");
    assert_eq!(mined[0]["amount"], quarter(chain));
    let index = app.api.index_of(&wallet, "native").await;
    let left = number(&unit(chain)) - number(&quarter(chain));
    assert!(number(&text(&index, "balance_pending")) < left, "{index}");
}
