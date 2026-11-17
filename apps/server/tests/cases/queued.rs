//! Back-to-back blocks while a send holds the network: the index job
//! queues one round and waits for it; never a pile of them.

use base::testing::{Caps, TestChain};

use super::{App, accepted, at_tip, funded, indexed, stall, unfinished_rounds, until};

pub(super) const NEEDS: Caps = Caps::NONE;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let payer = funded(app, chain).await;
    let payee = indexed(app, chain).await;
    stall(chain);
    let stuck = app.api.send(&payer, "native", &payee.address, "0.25").await;
    until("one round waiting behind the send", || async {
        let job = app.api.job(&stuck).await;
        let rounds = unfinished_rounds(app, chain).await;
        if job["state"] == "pending" && job["reason"].is_string() && rounds == 1 {
            return Ok(());
        }
        Err(format!("{job} rounds {rounds}"))
    })
    .await;
    for _ in 0..5 {
        chain.mine(1);
        at_tip(app, chain).await;
        let rounds = unfinished_rounds(app, chain).await;
        assert!(rounds <= 1, "{rounds} rounds queued");
    }
    accepted(app, &stuck).await;
}
