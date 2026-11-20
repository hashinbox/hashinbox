//! A broadcast whose answer is lost: the job counts it sent and sends the
//! same bytes again at its next check, the node refuses them as already
//! known, and the send still counts once.

use base::testing::{Caps, TestChain};
use rpc::testing::Fault;

use super::{App, accepted, funded, quarter, settle, succeeded, text, unit, until};

pub(super) const NEEDS: Caps = Caps::NONE;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let wallet = funded(app, chain).await;
    let other = app.api.wallet(&chain.network()).await;
    let node = chain.node(0);
    node.faults().fail_next(Some(T::SUBMIT), Fault::Lost);
    let job = app
        .api
        .send(&wallet, "native", &other.address, "0.25")
        .await;
    accepted(app, &job).await;
    until("the same bytes again at the next check", || async {
        let calls = node.faults().calls(T::SUBMIT);
        if calls >= 2 {
            return Ok(());
        }
        Err(calls.to_string())
    })
    .await;
    settle(app, chain).await;
    succeeded(app, &job).await;
    let legs = until("the send to be mined", || async {
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
    assert_eq!(legs.len(), 1, "{legs:?}");
    assert_eq!(legs[0]["amount"], quarter(chain));
    let index = app.api.index_of(&wallet, "native").await;
    assert_ne!(text(&index, "balance_pending"), unit(chain));
}
