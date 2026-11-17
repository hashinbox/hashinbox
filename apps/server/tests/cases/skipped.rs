//! A round is skipped while a send holds its network (076): a send stuck in
//! retry before it is signed holds it, so every round meanwhile is finished
//! skipped, naming the send; the send then goes.

use base::testing::{Caps, TestChain};

use super::{App, accepted, funded, indexed, jobs, stall, until};

pub(super) const NEEDS: Caps = Caps::NONE;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let payer = funded(app, chain).await;
    let payee = indexed(app, chain).await;
    stall(chain);
    let stuck = app.api.send(&payer, "native", &payee.address, "0.25").await;
    let held = format!("skipped: {stuck}");
    until("a round skipped behind the send", || async {
        let rounds = jobs(app, "kind=triggers&state=skipped").await;
        if rounds.iter().any(|round| round["reason"] == held.as_str()) {
            return Ok(());
        }
        Err(format!("{rounds:?}"))
    })
    .await;
    accepted(app, &stuck).await;
}
