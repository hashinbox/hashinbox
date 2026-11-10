//! A round whose write fails at its second trigger (F2): nothing of the
//! round is kept, not even the first trigger's run and send; once the
//! write goes, each trigger decides once.

use base::testing::{Caps, TestChain};

use super::{App, decided_after, funded, indexed, jobs, payout, runs, sends_from, until};

pub(super) const NEEDS: Caps = Caps::NONE;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let treasury = funded(app, chain).await;
    let payee = indexed(app, chain).await;
    app.sql(
        "CREATE TRIGGER refuse_b BEFORE INSERT ON trigger_run WHEN NEW.\"trigger\" = 'b' \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .await;
    // One batch: a round between two creations would decide `a` alone, and
    // its send would leave too little for either to decide again.
    payout(app, chain, &["a", "b"], &treasury, &payee).await;
    until("the round to fail at b", || async {
        let failed = jobs(app, "kind=triggers&state=failed").await;
        if failed.iter().any(|round| {
            round["reason"]
                .as_str()
                .is_some_and(|why| why.contains("injected"))
        }) {
            return Ok(());
        }
        Err(format!("{failed:?}"))
    })
    .await;
    assert!(runs(app, "a").await.is_empty(), "a's run kept");
    assert!(sends_from(app, &treasury).await.is_empty(), "a's send kept");

    app.sql("DROP TRIGGER refuse_b;").await;
    let sends = decided_after(app, chain, &treasury).await;
    assert_eq!(sends.len(), 2, "{sends:?}");
    assert_eq!(runs(app, "a").await.len(), 1);
    assert_eq!(runs(app, "b").await.len(), 1);
}
