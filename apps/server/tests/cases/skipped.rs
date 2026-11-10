//! A round waiting behind a send stuck in retry gives way to a send queued
//! after it (076): it would decide without seeing that send. It is finished
//! skipped, naming the send; both sends then go.

use base::testing::{Caps, TestChain};
use rpc::{ErrorKind, testing::Fault};

use super::{App, funded, indexed, jobs, succeeded, until};

pub(super) const NEEDS: Caps = Caps::NONE;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let payer = funded(app, chain).await;
    let payee = indexed(app, chain).await;
    for _ in 0..2 {
        chain
            .node(0)
            .faults()
            .fail_next(Some(T::SUBMIT), Fault::Transport(ErrorKind::Unavailable));
    }
    let stuck = app.api.send(&payer, "native", &payee.address, "0.25").await;
    let round = until("a round waiting behind the send", || async {
        let job = app.api.job(&stuck).await;
        let rounds = jobs(app, "kind=triggers&state=pending").await;
        let waiting = job["state"] == "pending" && job["reason"].is_string();
        match rounds.first() {
            Some(round) if waiting => Ok(round["id"].as_str().unwrap_or_default().to_owned()),
            _ => Err(format!("{job} rounds {rounds:?}")),
        }
    })
    .await;
    let newer = app.api.send(&payer, "native", &payee.address, "0.25").await;
    let skipped = until("the round to give way", || async {
        let job = app.api.job(&round).await;
        if job["state"] == "skipped" {
            return Ok(job);
        }
        Err(job.to_string())
    })
    .await;
    assert_eq!(skipped["reason"], format!("skipped: {newer}"));
    succeeded(app, &stuck).await;
    succeeded(app, &newer).await;
}
