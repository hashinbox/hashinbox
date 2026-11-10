//! The broadcast job over the scripted network: it signs once, records its
//! transactions as pending and broadcasts them; a rerun never builds again;
//! a refusal forgets what is not sent; a sender short of funds sends
//! nothing.

use std::sync::{Arc, Mutex};

use base::{TransactionErrorKind, WalletStore as _};
use tasks::{Ack, Admission, Claim, Context, Id, OnLock, Store, Worker};

use super::{Fake, Script, database, network, payments};
use crate::{
    db::Db,
    models::{transaction, transaction::Transaction},
    usecase::workers::broadcast::{Broadcast, Payment, SendIntent},
    usecase::{sdk::Stores, tasks::Queue},
};

/// A broadcast job of `amounts` from `from` over the scripted network.
struct Fixture {
    db: Db,
    queue: Queue,
    script: Arc<Script>,
    worker: Broadcast,
    job: Id,
    _directory: tempfile::TempDir,
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
        let intent = SendIntent {
            chain: network().chain.to_string(),
            network: network().name,
            address: from.to_owned(),
            asset: "native".to_owned(),
            transfers: amounts
                .iter()
                .map(|amount| Payment {
                    to: "recipient".into(),
                    amount: (*amount).into(),
                })
                .collect(),
        };
        let queued = Broadcast::enqueue(&queue, &intent).await.expect("a job");
        let Admission::Queued(job) = queued else {
            panic!("a send is always queued");
        };
        let now = tasks::now();
        let claim = Claim {
            kind: Broadcast::KIND,
            limit: 1,
            on_lock: OnLock::Wait,
            now,
            until: now + 60_000,
        };
        let claimed = queue.claim(claim).await.expect("claimed");
        assert_eq!(claimed.len(), 1, "the job runs");
        Self {
            worker: Broadcast::new(payments(&db, script.clone()).await, db.clone()),
            db,
            queue,
            script,
            job,
            _directory: directory,
        }
    }

    /// One run of the job as stored now.
    async fn run(&self) -> Ack {
        let mut job = self
            .queue
            .get(self.job)
            .await
            .expect("a read")
            .expect("the job");
        let store: Arc<dyn Store> = Arc::new(self.queue.clone());
        let cx = Context::new(&mut job, store).expect("a context");
        self.worker.run(cx).await
    }

    /// Whether txid `txid` is recorded as pending.
    async fn recorded(&self, txid: &str) -> bool {
        let id = Transaction::id(&network(), txid).expect("an id");
        self.db
            .read(move |connection| {
                Ok(transaction::db::get(connection, &id)?.is_some_and(|kept| kept.block.is_none()))
            })
            .await
            .expect("a read")
    }

    /// Forgets txid `txid`, as an expiry does.
    async fn forget(&self, txid: &str) {
        let id = Transaction::id(&network(), txid).expect("an id");
        self.db
            .write(move |connection| Ok(transaction::db::delete_pending(connection, &[id])?))
            .await
            .expect("a write");
    }

    fn built(&self) -> u8 {
        *self.script.built.lock().expect("the counter")
    }

    fn sent(&self) -> Vec<Vec<u8>> {
        self.script.sent.lock().expect("the log").clone()
    }
}

#[tokio::test]
async fn it_records_then_broadcasts_each() {
    let fixture = Fixture::new(&["1", "2"], Vec::new()).await;
    assert_eq!(fixture.run().await, Ack::Done);
    assert_eq!(fixture.built(), 2);
    assert_eq!(fixture.sent(), [vec![1], vec![2]]);
    assert!(fixture.recorded("1").await && fixture.recorded("2").await);
}

#[tokio::test]
async fn a_retry_resends_the_recorded_bytes() {
    let fixture = Fixture::new(&["1"], vec![Err(TransactionErrorKind::Unavailable)]).await;
    assert!(matches!(fixture.run().await, Ack::Retry(_)));
    assert!(fixture.recorded("1").await);
    assert_eq!(fixture.run().await, Ack::Done);
    assert_eq!(fixture.built(), 1);
    assert_eq!(fixture.sent(), [vec![1], vec![1]]);
}

#[tokio::test]
async fn a_refusal_forgets_what_is_not_sent() {
    let fixture = Fixture::new(
        &["1", "2"],
        vec![Ok(()), Err(TransactionErrorKind::Rejected)],
    )
    .await;
    assert!(matches!(fixture.run().await, Ack::Fail(reason) if reason.starts_with("refused")));
    assert!(fixture.recorded("1").await);
    assert!(!fixture.recorded("2").await);
}

#[tokio::test]
async fn a_rerun_of_a_forgotten_transaction_is_an_unknown_outcome() {
    let fixture = Fixture::new(&["1"], vec![Err(TransactionErrorKind::Unavailable)]).await;
    assert!(matches!(fixture.run().await, Ack::Retry(_)));
    fixture.forget("1").await;
    assert_eq!(fixture.run().await, Ack::Fail("outcome-unknown".into()));
    assert_eq!(fixture.built(), 1);
}

#[tokio::test]
async fn a_sender_short_of_funds_sends_nothing() {
    let fixture = Fixture::of("sender", &["1"], Vec::new(), 5).await;
    assert_eq!(fixture.run().await, Ack::Fail("insufficient-funds".into()));
    assert!(!fixture.recorded("1").await);
    assert!(fixture.sent().is_empty());
}

#[tokio::test]
async fn a_sender_without_a_key_fails() {
    let fixture = Fixture::of("stranger", &["1"], Vec::new(), 0).await;
    assert!(matches!(fixture.run().await, Ack::Fail(reason) if reason.starts_with("invalid")));
    assert!(fixture.sent().is_empty());
}
