//! The send job over the scripted network: a crash after the bytes were
//! saved resumes with them and never rebuilds; a refusal fails the job; an
//! expired Solana-style blockhash fails it as `outcome-unknown`; a batch
//! keeps its accepted prefix; a sender without a key fails.

use std::sync::{Arc, Mutex};

use base::{TransactionErrorKind, WalletStore as _};
use tasks::{Ack, Executor, Job, State, Store};

use super::{Fake, Script, database, network, payments};
use crate::{
    storage::Storage,
    workers::send::{Payment, SendIntent, Sends},
};

/// A send job of `amounts` from a keyed wallet, its executor and its queue.
struct Fixture {
    queue: Arc<Storage>,
    sends: Sends,
    script: Arc<Script>,
    job: Job,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new(amounts: &[&str], answers: Vec<Result<(), TransactionErrorKind>>) -> Self {
        Self::of("sender", amounts, answers).await
    }

    /// The job sends from `from`; only `sender` is keyed.
    async fn of(
        from: &str,
        amounts: &[&str],
        answers: Vec<Result<(), TransactionErrorKind>>,
    ) -> Self {
        let (storage, directory) = database().await;
        let script = Arc::new(Script {
            answers: Mutex::new(answers.into()),
            ..Script::default()
        });
        let sender = Fake {
            address: base::Address::new(network(), "sender"),
            script: script.clone(),
        };
        storage.insert(&sender).await.expect("the wallet kept");
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
        let mut job = intent.job();
        job.state = State::Running;
        storage.set(job.clone()).await.expect("a job");
        Self {
            sends: Sends::new(payments(&storage, script.clone()).await),
            queue: Arc::new(storage),
            script,
            job,
            _directory: directory,
        }
    }

    /// One execution of the job as the store holds it now.
    async fn execute(&self) -> Ack {
        let job = self
            .queue
            .get(&self.job.id)
            .await
            .expect("a read")
            .expect("the job");
        self.sends.execute(job, self.queue.clone()).await
    }

    fn built(&self) -> u8 {
        *self.script.built.lock().expect("the counter")
    }

    fn sent(&self) -> Vec<Vec<u8>> {
        self.script.sent.lock().expect("the log").clone()
    }

    async fn progress(&self) -> (usize, usize) {
        let job = self
            .queue
            .get(&self.job.id)
            .await
            .expect("a read")
            .expect("the job");
        crate::workers::send::progress(&job.payload)
    }
}

#[tokio::test]
async fn a_retry_resends_the_saved_bytes() {
    let fixture = Fixture::new(&["1"], vec![Err(TransactionErrorKind::Unavailable)]).await;
    assert!(matches!(fixture.execute().await, Ack::Retry(_)));
    assert_eq!(fixture.progress().await, (1, 0));
    assert_eq!(fixture.execute().await, Ack::Done);
    assert_eq!(fixture.built(), 1);
    assert_eq!(fixture.sent(), [vec![1], vec![1]]);
}

#[tokio::test]
async fn a_crash_after_saving_broadcasts_the_saved_bytes() {
    let fixture = Fixture::new(&["1"], Vec::new()).await;
    // A first run that saved its bytes and stopped before broadcasting them.
    let mut job = fixture.job.clone();
    job.payload["progress"] = serde_json::json!({ "signed": ["09"], "accepted": 0 });
    fixture.queue.set(job).await.expect("saved");
    assert_eq!(fixture.execute().await, Ack::Done);
    assert_eq!(fixture.built(), 0);
    assert_eq!(fixture.sent(), [vec![9]]);
}

#[tokio::test]
async fn a_refusal_fails_the_job() {
    let fixture = Fixture::new(&["1"], vec![Err(TransactionErrorKind::Rejected)]).await;
    assert!(matches!(fixture.execute().await, Ack::Fail(reason) if reason.starts_with("refused")));
}

#[tokio::test]
async fn an_expired_blockhash_is_an_unknown_outcome() {
    let fixture = Fixture::new(&["1"], vec![Err(TransactionErrorKind::Expired)]).await;
    assert_eq!(fixture.execute().await, Ack::Fail("outcome-unknown".into()));
}

#[tokio::test]
async fn a_batch_keeps_its_accepted_prefix() {
    let fixture = Fixture::new(
        &["1", "2"],
        vec![Ok(()), Err(TransactionErrorKind::Rejected)],
    )
    .await;
    assert!(matches!(fixture.execute().await, Ack::Fail(_)));
    assert_eq!(fixture.built(), 2);
    assert_eq!(fixture.progress().await, (2, 1));
}

#[tokio::test]
async fn a_sender_without_a_key_fails() {
    let fixture = Fixture::of("stranger", &["1"], Vec::new()).await;
    assert!(matches!(fixture.execute().await, Ack::Fail(reason) if reason.starts_with("invalid")));
    assert!(fixture.sent().is_empty());
}
