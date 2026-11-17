//! The broadcast job's checks (designs 084, 085) over the scripted network
//! on SQLite, each by the payment's transaction index alone: a sent payment
//! holds its funds and coins whatever its age and is sent again at each
//! check whatever the answer, until it is found in a final block; one not
//! found past its `expires_height` is forgotten (`dead`) and its job fails;
//! one whose index is not read through the final block is not sent again;
//! one whose index was given up, or a jump forgot, fails `forgotten`. The
//! job's end deletes the index.

use std::sync::Arc;

use base::{
    Address, BlockStore, Decimal, InputStore as _, Transaction, TransactionErrorKind,
    TransactionIndexStore, TransactionState,
};
use tasks::{Ack, Admission, Claim, Context, Id, OnLock, Store, Worker};

use super::{
    Script, container, network,
    storage::{
        address, block, coin, confirm, forget, forgotten, look_for, looked_for, mined, payment,
        raw, record, run, send, storage, watch,
    },
};
use crate::{
    db::Db,
    models::transaction_forgotten::Why,
    usecase::{
        sdk::Stores,
        tasks::Queue,
        workers::broadcast::{self, Broadcast, Progress, SendIntent},
    },
};

/// alice, indexed through block 1, holds coin `f:0` (10) of block 0 and has
/// sent `s` of it: 4 to bob, fee 1, its index (from 0, expiring past
/// `expires`) read through 1, the confirmed block. Sent at Unix ms 0, older
/// than any expiry there was; its broadcast job is due for a check.
struct Fixture {
    storage: Stores,
    db: Db,
    queue: Queue,
    script: Arc<Script>,
    worker: Broadcast,
    job: Id,
    directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        Self::expiring(None).await
    }

    async fn expiring(expires: Option<u64>) -> Self {
        let (storage, db, directory) = storage().await;
        let network = network();
        watch(&storage, &alice(), 0).await;
        look_for(&storage, "s", 0, expires).await;
        let funded = vec![
            block(
                &network,
                0,
                0,
                vec![payment(&network, "f", "alice", 10, None)],
            ),
            block(&network, 1, 0, Vec::new()),
        ];
        BlockStore::insert(&storage, &run(&network, funded))
            .await
            .unwrap();
        confirm(&db, 1).await;
        record(&db, &sent("s", "bob", 4, "f")).await;
        let script = Arc::new(Script::default());
        let queue = crate::usecase::tasks::queue(db.clone());
        let intent = SendIntent {
            chain: network.chain.to_string(),
            network: network.name.clone(),
            address: "alice".to_owned(),
            asset: "native".to_owned(),
            to: "bob".to_owned(),
            amount: "4".to_owned(),
        };
        let Admission::Queued(job) = Broadcast::enqueue(&queue, &intent).await.unwrap() else {
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
        let mut claimed = queue.claim(claim).await.unwrap();
        assert_eq!(claimed.len(), 1, "the job runs");
        // It sent `s` on its first run.
        let mut running = claimed.remove(0);
        let progress = Progress {
            txid: "s".to_owned(),
            sent: true,
        };
        running.state = Some(serde_json::to_value(progress).unwrap());
        let attempt = running.attempts;
        queue.update(running, attempt).await.unwrap();
        Self {
            worker: Broadcast::new(container(&db, script.clone()), db.clone(), broadcast::CHECK),
            storage,
            db,
            queue,
            script,
            job,
            directory,
        }
    }

    /// One check.
    async fn check(&self) -> Ack {
        let mut job = self.queue.get(self.job).await.unwrap().unwrap();
        let store: Arc<dyn Store> = Arc::new(self.queue.clone());
        self.worker
            .run(Context::new(&mut job, store).unwrap())
            .await
    }

    /// Whether `s` is still kept, pending or mined.
    async fn kept(&self) -> bool {
        self.db
            .read(|connection| {
                Ok(crate::models::transaction::db::get(connection, "bitcoin.regtest.s")?.is_some())
            })
            .await
            .unwrap()
    }

    /// alice's unspent coins, and what she can spend.
    async fn holds(&self) -> (usize, Decimal) {
        let unspent = self.storage.find_unspent(&[alice()]).await.unwrap().len();
        let spendable = self
            .db
            .read(|connection| {
                let id = "bitcoin.regtest.alice.native";
                let indexes =
                    crate::models::wallet_index::db::find_by_ids(connection, &network(), &[id], 1)?;
                indexes[id].spendable()
            })
            .await
            .unwrap();
        (unspent, spendable)
    }

    /// Keeps block 2 with `transaction` in it.
    async fn mine(&self, transaction: Transaction) {
        let network = network();
        BlockStore::insert(
            &self.storage,
            &run(&network, vec![block(&network, 2, 0, vec![transaction])]),
        )
        .await
        .unwrap();
    }

    /// What was sent again.
    fn sent(&self) -> Vec<Vec<u8>> {
        self.script.sent.lock().unwrap().clone()
    }

    /// The txids looked for: the job's end deletes its index.
    async fn looked_for(&self) -> Vec<String> {
        let indexes = looked_for(&self.storage).await;
        indexes.into_iter().map(|index| index.0).collect()
    }
}

fn alice() -> Address {
    address(&network(), "alice")
}

/// Pending `txid` of alice's coin `spent`: `amount` to `to`, fee 1.
fn sent(txid: &str, to: &str, amount: u64, spent: &str) -> Transaction {
    let network = network();
    let legs = send(&network, "alice", to, amount, 1, coin(&network, spent, 0));
    let mut transaction = mined(&network, txid, 0, legs);
    transaction.state = TransactionState::Pending;
    transaction.order = None;
    transaction
}

#[tokio::test]
async fn a_sent_payment_is_sent_again_at_each_check_until_it_is_found_final() {
    let fixture = Fixture::new().await;
    fixture
        .script
        .answers
        .lock()
        .unwrap()
        .push_back(Err(TransactionErrorKind::Rejected));
    for _ in 0..2 {
        assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    }
    assert!(fixture.kept().await, "a refused send again forgets nothing");
    assert_eq!(fixture.holds().await, (0, Decimal::from(5)));
    let s = b"s".to_vec();
    assert_eq!(fixture.sent(), [s.clone(), s]);
    let network = network();
    let legs = send(&network, "alice", "bob", 4, 1, coin(&network, "f", 0));
    fixture.mine(mined(&network, "s", 0, legs)).await;
    assert!(
        matches!(fixture.check().await, Ack::Again(_, None)),
        "found above the confirmed block: not final yet"
    );
    assert_eq!(fixture.sent().len(), 2, "a found one is not sent again");
    confirm(&fixture.db, 2).await;
    assert_eq!(fixture.check().await, Ack::Done);
    assert!(forgotten(&fixture.directory).is_empty());
    assert!(fixture.looked_for().await.is_empty());
}

#[tokio::test]
async fn a_payment_not_found_past_its_expires_height_is_dead() {
    let fixture = Fixture::expiring(Some(0)).await;
    assert_eq!(fixture.check().await, Ack::Fail("dead".into()));
    assert!(!fixture.kept().await);
    assert_eq!(fixture.holds().await, (1, Decimal::from(10)));
    assert!(fixture.sent().is_empty(), "not sent again");
    assert!(fixture.looked_for().await.is_empty());
    assert_eq!(
        forgotten(&fixture.directory),
        [("s".to_owned(), "dead".to_owned())]
    );
    // Who was to be paid what, and its bytes, as recorded.
    #[derive(diesel::QueryableByName)]
    struct Kept {
        #[diesel(sql_type = diesel::sql_types::Text)]
        legs: String,
        #[diesel(sql_type = diesel::sql_types::Binary)]
        bytes: Vec<u8>,
    }
    let kept: Vec<Kept> = diesel::RunQueryDsl::load(
        diesel::sql_query("SELECT legs, bytes FROM transaction_forgotten"),
        &mut raw(&fixture.directory),
    )
    .unwrap();
    let legs: serde_json::Value = serde_json::from_str(&kept[0].legs).unwrap();
    let kinds: Vec<_> = legs
        .as_array()
        .unwrap()
        .iter()
        .map(|leg| (leg["kind"].clone(), leg["amount"].clone()))
        .collect();
    assert_eq!(
        kinds,
        [
            (serde_json::json!(1), serde_json::json!("4")),
            (serde_json::json!(4), serde_json::json!("1"))
        ],
        "alice's spend and fee; bob is not indexed"
    );
    assert_eq!(kept[0].bytes, b"s");
}

#[tokio::test]
async fn a_payment_found_before_its_expires_height_is_done() {
    let fixture = Fixture::expiring(Some(1)).await;
    let network = network();
    let legs = send(&network, "alice", "bob", 4, 1, coin(&network, "f", 0));
    fixture.mine(mined(&network, "s", 0, legs)).await;
    confirm(&fixture.db, 2).await;
    assert_eq!(fixture.check().await, Ack::Done);
    assert!(forgotten(&fixture.directory).is_empty());
}

#[tokio::test]
async fn a_payment_whose_index_is_not_read_through_the_final_block_waits() {
    let fixture = Fixture::new().await;
    confirm(&fixture.db, 5).await;
    assert!(matches!(fixture.check().await, Ack::Again(..)));
    assert!(fixture.kept().await);
    assert!(fixture.sent().is_empty(), "not sent again");
}

#[tokio::test]
async fn a_payment_whose_index_was_given_up_fails_forgotten() {
    let fixture = Fixture::new().await;
    let index = TransactionIndexStore::find_all(&fixture.storage, &network())
        .await
        .unwrap()
        .remove(0);
    fixture.storage.give_up(&index).await.unwrap();
    assert_eq!(fixture.check().await, Ack::Fail("forgotten".into()));
    assert!(!fixture.kept().await);
    assert!(fixture.sent().is_empty());
    assert!(fixture.looked_for().await.is_empty());
    assert_eq!(
        forgotten(&fixture.directory),
        [("s".to_owned(), "jump".to_owned())]
    );
}

#[tokio::test]
async fn a_payment_a_jump_forgot_fails_forgotten() {
    let fixture = Fixture::new().await;
    assert_eq!(forget(&fixture.db, "s", Why::Jump).await, 1);
    assert_eq!(fixture.check().await, Ack::Fail("forgotten".into()));
    assert!(fixture.sent().is_empty());
    assert!(fixture.looked_for().await.is_empty());
}

#[tokio::test]
async fn a_senders_pending_sends_are_those_it_pays_from() {
    let fixture = Fixture::new().await;
    watch(&fixture.storage, &address(&network(), "bob"), 0).await;
    record(&fixture.db, &sent("t", "bob", 1, "g")).await;
    let txids = |sent: Vec<Transaction>| -> Vec<String> {
        let mut txids: Vec<_> = sent
            .into_iter()
            .map(|transaction| transaction.id.id)
            .collect();
        txids.sort();
        txids
    };
    let of_alice = fixture.storage.find_sent(&alice()).await.unwrap();
    assert_eq!(txids(of_alice), ["s", "t"]);
    let of_bob = fixture.storage.find_sent(&address(&network(), "bob")).await;
    assert!(of_bob.unwrap().is_empty(), "bob only receives");
}
