//! A broadcast whose answers are all lost: every node takes the bytes and
//! none answers, so the outcome is unknown. The job sends the same bytes
//! again, the nodes already hold them and the chain confirms it; one
//! transaction, spent once.

use base::testing::{Caps, TestChain};
use rpc::testing::Fault;

use super::{App, funded, quarter, settle, succeeded, until};

pub(super) const NEEDS: Caps = Caps::CLUSTER;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = funded(app, chain).await;
    let other = app.api.wallet(&chain.network()).await;
    let nodes = chain.nodes().len();
    for node in 0..nodes {
        chain
            .node(node)
            .faults()
            .fail_next(Some(T::SUBMIT), Fault::Lost);
    }
    let job = app
        .api
        .send(&wallet, "native", &other.address, "0.25")
        .await;
    let done = succeeded(app, &job).await;
    assert_eq!(done["attempts"], 2, "{done}");
    assert_eq!(
        (done["signed"].as_u64(), done["accepted"].as_u64()),
        (Some(1), Some(1)),
        "{done}"
    );
    for node in 0..nodes {
        assert_eq!(chain.node(node).faults().calls(T::SUBMIT), 2, "node {node}");
    }
    settle(chain);
    let legs = until("the send and its fee to be mined", || async {
        let legs = app.api.legs(&wallet, "native").await;
        if legs.iter().any(|leg| leg["kind"] == 4) {
            return Ok(legs);
        }
        Err(format!("{legs:?}"))
    })
    .await;
    let kinds: Vec<_> = legs.iter().map(|leg| leg["kind"].as_i64()).collect();
    assert_eq!(
        kinds.iter().filter(|kind| **kind == Some(1)).count(),
        1,
        "{legs:?}"
    );
    assert_eq!(
        kinds.iter().filter(|kind| **kind == Some(4)).count(),
        1,
        "{legs:?}"
    );
    let spent = legs
        .iter()
        .find(|leg| leg["kind"] == 1)
        .cloned()
        .unwrap_or_default();
    assert_eq!(spent["amount"], quarter(chain));
}
