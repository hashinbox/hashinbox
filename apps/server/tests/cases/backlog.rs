//! A send stuck in retry holds the network while blocks keep coming (F1):
//! the round behind it waits alone, not one per interval, so a fixed
//! payout decides once when the send clears; one payment, never two.

use base::testing::{Caps, TestChain};
use rpc::{ErrorKind, testing::Fault};

use super::{
    App, at_tip, decided_after, funded, indexed, payout, quarter, runs, settle, succeeded,
    unfinished_rounds, until,
};

pub(super) const NEEDS: Caps = Caps::NONE;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let treasury = funded(app, chain).await;
    let payer = funded(app, chain).await;
    let payee = indexed(app, chain).await;
    // The payer's send is refused twice by the node, then goes.
    for _ in 0..2 {
        chain
            .node(0)
            .faults()
            .fail_next(Some(T::SUBMIT), Fault::Transport(ErrorKind::Unavailable));
    }
    let stuck = app.api.send(&payer, "native", &payee.address, "0.25").await;
    until("the send to retry", || async {
        let job = app.api.job(&stuck).await;
        if job["state"] == "pending" && job["reason"].is_string() {
            return Ok(());
        }
        Err(job.to_string())
    })
    .await;
    payout(app, chain, &["payout"], &treasury, &payee).await;
    for _ in 0..3 {
        chain.mine(1);
        at_tip(app, chain).await;
        assert!(unfinished_rounds(app, chain).await <= 1);
    }

    succeeded(app, &stuck).await;
    let sends = decided_after(app, chain, &treasury).await;
    assert_eq!(sends.len(), 1, "{sends:?}");
    assert_eq!(runs(app, "payout").await.len(), 1);
    settle(chain);
    let legs = until("the payout and its fee to be mined", || async {
        let legs = app.api.legs(&treasury, "native").await;
        if legs.iter().any(|leg| leg["kind"] == 4) {
            return Ok(legs);
        }
        Err(format!("{legs:?}"))
    })
    .await;
    let paid: Vec<_> = legs.iter().filter(|leg| leg["kind"] == 1).collect();
    assert_eq!(paid.len(), 1, "{legs:?}");
    assert_eq!(paid[0]["amount"], quarter(chain));
}
