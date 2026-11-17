//! The broadcast job's first run over the scripted network: one payment,
//! one transaction. It signs once, records it as pending, not yet sent, and
//! broadcasts it; accepted, or failed transiently, it is marked sent and
//! checked later, never built again; a refusal forgets it (`refused`); a
//! sender short of funds sends nothing. Two payments are two jobs, run in
//! order, each failing on its own; the second runs while the first waits
//! for its check.

use std::sync::{Arc, Mutex};

use base::{TransactionErrorKind, WalletStore as _};
use tasks::{Ack, Admission, Claim, Context, Id, OnLock, Status, Store, Worker};

use super::{
    Fake, Script, container, database, network,
    storage::{block, confirm, forgotten, run},
};
use crate::{
    db::Db,
    models::{transaction, transaction::TransactionId},
    usecase::workers::broadcast::{self, Broadcast, SendIntent},
    usecase::{sdk::Stores, tasks::Queue},
};

/// Broadcast jobs from `from` over the scripted network, one per amount,
/// the first of them claimed.
struct Fixture {
    db: Db,
    queue: Queue,
    script: Arc<Script>,
    worker: Broadcast,
    jobs: Vec<Id>,
    directory: tempfile::TempDir,
}

impl Fixture {
    async fn new(amounts: &[&str], answers: Vec<Result<(), TransactionErrorKind>>) -> Self {
        Self::of("sender", amounts, answers, 0).await
    }

    /// Only `sender` is keyed; each transaction pays `fee`.
    async fn of(
        from: &str,
        amounts: &[&str],
        answers: Vec<Result<(), TransactionErrorKind>>,
        fee: u64,
    ) -> Self {
        let (db, directory) = database().await;
        let queue = crate::usecase::tasks::queue(db.clone());
        let script = Arc::new(Script {
            answers: Mutex::new(answers.into()),
            fee,
            ..Script::default()
        });
        let sender = Fake {
            address: base::Address::new(network(), "sender"),
            script: script.clone(),
        };
        Stores::new(db.clone())
            .insert(&sender)
            .await
            .expect("the wallet kept");
        let mut jobs = Vec::new();
        for amount in amounts {
            let intent = SendIntent {
                chain: network().chain.to_string(),
                network: network().name,
                address: from.to_owned(),
                asset: "native".to_owned(),
                to: "recipient".into(),
                amount: (*amount).into(),
            };
            let queued = Broadcast::enqueue(&queue, &intent).await.expect("a job");
            let Admission::Queued(job) = queued else {
                panic!("a send is always queued");
            };
            jobs.push(job);
        }
        let fixture = Self {
            worker: Broadcast::new(container(&db, script.clone()), db.clone(), broadcast::CHECK),
            db,
            queue,
            script,
            jobs,
            directory,
        };
        assert_eq!(fixture.claim().await, fixture.jobs[..1], "the first runs");
        fixture
    }

    /// Claims the jobs that may start now.
    async fn claim(&self) -> Vec<Id> {
        let now = tasks::now();
        let claim = Claim {
            kind: Broadcast::KIND,
            limit: 4,
            on_lock: OnLock::Wait,
            now,
            until: now + 60_000,
        };
        let claimed = self.queue.claim(claim).await.expect("claimed");
        claimed.into_iter().map(|job| job.id).collect()
    }

    /// One run of the first job as stored now.
    async fn run(&self) -> Ack {
        self.run_job(self.jobs[0]).await
    }

    /// One run of job `id` as stored now, its outcome not stored.
    async fn run_job(&self, id: Id) -> Ack {
        let mut job = self.job(id).await;
        let store: Arc<dyn Store> = Arc::new(self.queue.clone());
        let cx = Context::new(&mut job, store).expect("a context");
        self.worker.run(cx).await
    }

    /// Stores how job `id`'s run ended.
    async fn end(&self, id: Id, ack: Ack) {
        let job = self.job(id).await;
        let attempt = job.attempts;
        let ended = job.settle(ack, &Broadcast::POLICY, tasks::now());
        self.queue.update(ended, attempt).await.expect("stored");
    }

    async fn job(&self, id: Id) -> tasks::Job {
        self.queue.get(id).await.expect("a read").expect("the job")
    }

    /// Job `id`'s transaction (txid) and whether the nodes accepted it.
    async fn progress(&self, id: Id) -> (Option<String>, bool) {
        broadcast::progress(self.job(id).await.state.as_ref())
    }

    /// Txid `txid` as recorded pending: `None` unless pending, else when a
    /// node accepted it.
    async fn recorded(&self, txid: &str) -> Option<Option<i64>> {
        let id = TransactionId::new(&network(), txid).expect("an id");
        self.db
            .read(move |connection| {
                let kept = transaction::db::get(connection, &id)?;
                Ok(kept
                    .filter(|kept| kept.block.is_none())
                    .map(|kept| kept.sent_at))
            })
            .await
            .expect("a read")
    }

    fn built(&self) -> u8 {
        *self.script.built.lock().expect("the counter")
    }

    fn sent(&self) -> Vec<Vec<u8>> {
        self.script.sent.lock().expect("the log").clone()
    }
}

#[tokio::test]
async fn it_records_then_broadcasts_its_one_transaction() {
    let fixture = Fixture::new(&["1"], Vec::new()).await;
    assert!(matches!(fixture.run().await, Ack::Again(_, None)));
    assert_eq!(fixture.built(), 1);
    assert_eq!(fixture.sent(), [vec![1]]);
    assert!(matches!(fixture.recorded("1").await, Some(Some(_))), "sent");
    assert_eq!(
        fixture.progress(fixture.jobs[0]).await,
        (Some("1".into()), true)
    );
}

#[tokio::test]
async fn a_transient_failure_counts_as_sent_and_its_check_sends_the_same_bytes() {
    let fixture = Fixture::new(&["1"], vec![Err(TransactionErrorKind::Unavailable)]).await;
    assert!(
        matches!(fixture.run().await, Ack::Again(_, Some(_))),
        "kept, with why; never given up"
    );
    assert!(
        matches!(fixture.recorded("1").await, Some(Some(_))),
        "a node may hold it"
    );
    assert_eq!(
        fixture.progress(fixture.jobs[0]).await,
        (Some("1".into()), true)
    );
    // Its index not read through the confirmed block yet: not judged.
    assert!(matches!(fixture.run().await, Ack::Again(_, None)));
    assert_eq!(fixture.sent(), [vec![1]]);
    let network = network();
    let first = run(&network, vec![block(&network, 0, 0, Vec::new())]);
    let stores = Stores::new(fixture.db.clone());
    base::BlockStore::insert(&stores, &first)
        .await
        .expect("kept");
    confirm(&fixture.db, 0).await;
    assert!(matches!(fixture.run().await, Ack::Again(_, None)));
    assert_eq!(fixture.built(), 1);
    assert_eq!(fixture.sent(), [vec![1], vec![1]]);
}

#[tokio::test]
async fn a_refusal_forgets_it() {
    let fixture = Fixture::new(&["1"], vec![Err(TransactionErrorKind::Rejected)]).await;
    assert!(matches!(fixture.run().await, Ack::Fail(reason) if reason.starts_with("refused")));
    assert_eq!(fixture.recorded("1").await, None);
    let refused = [("1".to_owned(), "refused".to_owned())];
    assert_eq!(forgotten(&fixture.directory), refused);
}

#[tokio::test]
async fn two_payments_are_two_jobs_run_in_order_one_transaction_each() {
    let fixture = Fixture::new(&["1", "2"], Vec::new()).await;
    let [first, second] = fixture.jobs[..] else {
        panic!("two jobs");
    };
    assert!(
        fixture.claim().await.is_empty(),
        "the second waits its turn"
    );
    let ack = fixture.run_job(first).await;
    assert!(matches!(ack, Ack::Again(_, None)));
    fixture.end(first, ack).await;
    assert_eq!(
        fixture.claim().await,
        [second],
        "the second runs while the first waits for its check"
    );
    assert!(matches!(fixture.run_job(second).await, Ack::Again(_, None)));
    assert_eq!(fixture.built(), 2);
    assert_eq!(fixture.sent(), [vec![1], vec![2]]);
    assert_eq!(fixture.progress(first).await, (Some("1".into()), true));
    assert_eq!(fixture.progress(second).await, (Some("2".into()), true));
}

#[tokio::test]
async fn the_second_payments_refusal_leaves_the_first_sent() {
    let fixture = Fixture::new(
        &["1", "2"],
        vec![Ok(()), Err(TransactionErrorKind::Rejected)],
    )
    .await;
    let [first, second] = fixture.jobs[..] else {
        panic!("two jobs");
    };
    let ack = fixture.run_job(first).await;
    fixture.end(first, ack).await;
    assert_eq!(fixture.claim().await, [second]);
    let ack = fixture.run_job(second).await;
    assert!(matches!(&ack, Ack::Fail(reason) if reason.starts_with("refused")));
    fixture.end(second, ack).await;
    let waiting = fixture.job(first).await;
    assert_eq!(
        (waiting.status, waiting.attempts),
        (Status::Pending, 0),
        "waiting for its check"
    );
    assert_eq!(fixture.job(second).await.status, Status::Failed);
    assert!(matches!(fixture.recorded("1").await, Some(Some(_))), "sent");
    assert_eq!(fixture.recorded("2").await, None);
    let refused = [("2".to_owned(), "refused".to_owned())];
    assert_eq!(forgotten(&fixture.directory), refused);
}

#[tokio::test]
async fn a_sender_short_of_funds_sends_nothing() {
    let fixture = Fixture::of("sender", &["1"], Vec::new(), 5).await;
    assert_eq!(fixture.run().await, Ack::Fail("insufficient-funds".into()));
    assert_eq!(fixture.recorded("1").await, None);
    assert!(fixture.sent().is_empty());
}

#[tokio::test]
async fn a_sender_without_a_key_fails() {
    let fixture = Fixture::of("stranger", &["1"], Vec::new(), 0).await;
    assert!(matches!(fixture.run().await, Ack::Fail(reason) if reason.starts_with("invalid")));
    assert!(fixture.sent().is_empty());
}
